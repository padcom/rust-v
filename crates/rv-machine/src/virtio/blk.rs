//! virtio-blk (§5.2), backed by a raw disk image. Requests are served synchronously on the
//! notifying hart's thread.

use std::fs::File;
use std::io;
use std::sync::Mutex;

use super::{Activation, Chain, VirtioDevice, device_type, read_config_bytes};

const SECTOR: u64 = 512;
const QUEUE_SIZE: u16 = 256;
/// Largest piece of a request copied through host memory at once.
const CHUNK: usize = 1 << 20;

mod feature {
    pub const SEG_MAX: u64 = 1 << 2;
    pub const RO: u64 = 1 << 5;
    pub const BLK_SIZE: u64 = 1 << 6;
    pub const FLUSH: u64 = 1 << 9;
}

mod req {
    pub const IN: u32 = 0;
    pub const OUT: u32 = 1;
    pub const FLUSH: u32 = 4;
    pub const GET_ID: u32 = 8;
}

const STATUS_OK: u8 = 0;
const STATUS_IOERR: u8 = 1;
const STATUS_UNSUPP: u8 = 2;

/// Storage behind a block device.
pub trait Backend: Send {
    fn size(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// A raw image file.
pub struct FileBackend {
    file: File,
    len: u64,
}

impl FileBackend {
    pub fn new(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(FileBackend { file, len })
    }
}

impl Backend for FileBackend {
    fn size(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::read_exact_at(&self.file, buf, offset)
        }
        #[cfg(windows)]
        {
            let mut done = 0;
            while done < buf.len() {
                let n = std::os::windows::fs::FileExt::seek_read(
                    &self.file,
                    &mut buf[done..],
                    offset + done as u64,
                )?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                done += n;
            }
            Ok(())
        }
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::write_all_at(&self.file, data, offset)
        }
        #[cfg(windows)]
        {
            let mut done = 0;
            while done < data.len() {
                done += std::os::windows::fs::FileExt::seek_write(
                    &self.file,
                    &data[done..],
                    offset + done as u64,
                )?;
            }
            Ok(())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// An in-memory disk.
impl Backend for Vec<u8> {
    fn size(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = offset as usize;
        buf.copy_from_slice(self.get(start..start + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let start = offset as usize;
        self.get_mut(start..start + data.len()).ok_or(io::ErrorKind::UnexpectedEof)?.copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Copy-on-write layer over another backend: writes stay in host memory and are discarded
/// with the device, so the image underneath is never modified.
pub struct Overlay {
    base: Box<dyn Backend>,
    blocks: std::collections::HashMap<u64, Box<[u8; OVERLAY_BLOCK]>>,
}

const OVERLAY_BLOCK: usize = 4096;

impl Overlay {
    pub fn new(base: Box<dyn Backend>) -> Self {
        Overlay { base, blocks: std::collections::HashMap::new() }
    }

    /// Splits `offset..offset + len` into (block, offset in block, length) pieces.
    fn pieces(offset: u64, len: usize) -> impl Iterator<Item = (u64, usize, usize)> {
        let mut pos = offset;
        let end = offset + len as u64;
        std::iter::from_fn(move || {
            (pos < end).then(|| {
                let block = pos / OVERLAY_BLOCK as u64;
                let at = (pos % OVERLAY_BLOCK as u64) as usize;
                let n = (OVERLAY_BLOCK - at).min((end - pos) as usize);
                pos += n as u64;
                (block, at, n)
            })
        })
    }
}

impl Backend for Overlay {
    fn size(&self) -> u64 {
        self.base.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if self.blocks.is_empty() {
            return self.base.read_at(offset, buf);
        }
        let mut done = 0;
        for (block, at, n) in Self::pieces(offset, buf.len()) {
            match self.blocks.get(&block) {
                Some(data) => buf[done..done + n].copy_from_slice(&data[at..at + n]),
                None => self.base.read_at(offset + done as u64, &mut buf[done..done + n])?,
            }
            done += n;
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        if offset + data.len() as u64 > self.size() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let mut done = 0;
        for (block, at, n) in Self::pieces(offset, data.len()) {
            if !self.blocks.contains_key(&block) {
                let mut fresh = Box::new([0u8; OVERLAY_BLOCK]);
                if n < OVERLAY_BLOCK {
                    let start = block * OVERLAY_BLOCK as u64;
                    let len = (self.size() - start).min(OVERLAY_BLOCK as u64) as usize;
                    self.base.read_at(start, &mut fresh[..len])?;
                }
                self.blocks.insert(block, fresh);
            }
            self.blocks.get_mut(&block).unwrap()[at..at + n].copy_from_slice(&data[done..done + n]);
            done += n;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct Blk {
    backend: Mutex<Box<dyn Backend>>,
    read_only: bool,
    id: String,
    active: Option<Activation>,
}

impl Blk {
    pub fn new(backend: Box<dyn Backend>, read_only: bool, id: &str) -> Self {
        Blk { backend: Mutex::new(backend), read_only, id: id.into(), active: None }
    }

    fn config(&self) -> [u8; 24] {
        let mut c = [0u8; 24];
        let sectors = self.backend.lock().unwrap().size() / SECTOR;
        c[0..8].copy_from_slice(&sectors.to_le_bytes());
        c[12..16].copy_from_slice(&(QUEUE_SIZE as u32 - 2).to_le_bytes());
        c[20..24].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        c
    }

    /// Serves one request; returns the number of bytes written into the chain.
    fn request(&self, a: &Activation, chain: &Chain) -> u32 {
        let mut header = [0u8; 16];
        let mut reader = chain.reader(&a.mem);
        let total = chain.writable_len();
        if total == 0 || reader.read(&mut header).ok() != Some(16) {
            return 0;
        }
        let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let mut writer = chain.writer(&a.mem);
        let data_len = total - 1;
        let mut backend = self.backend.lock().unwrap();
        let capacity = backend.size();
        let in_range = |len: u64| {
            sector.checked_mul(SECTOR).and_then(|o| o.checked_add(len)).is_some_and(|e| e <= capacity)
        };

        let (status, written) = match kind {
            req::IN if in_range(data_len) => {
                let mut buf = vec![0u8; CHUNK.min(data_len as usize)];
                let mut done = 0u64;
                let mut status = STATUS_OK;
                while done < data_len {
                    let n = (data_len - done).min(CHUNK as u64) as usize;
                    if backend.read_at(sector * SECTOR + done, &mut buf[..n]).is_err()
                        || writer.write(&buf[..n]).is_err()
                    {
                        status = STATUS_IOERR;
                        break;
                    }
                    done += n as u64;
                }
                (status, done)
            }
            req::OUT if self.read_only => (STATUS_IOERR, 0),
            req::OUT => {
                let len = reader.remaining();
                if !in_range(len) {
                    (STATUS_IOERR, 0)
                } else {
                    let mut buf = vec![0u8; CHUNK.min(len as usize)];
                    let mut done = 0u64;
                    let mut status = STATUS_OK;
                    while done < len {
                        let n = (len - done).min(CHUNK as u64) as usize;
                        if reader.read(&mut buf[..n]).is_err()
                            || backend.write_at(sector * SECTOR + done, &buf[..n]).is_err()
                        {
                            status = STATUS_IOERR;
                            break;
                        }
                        done += n as u64;
                    }
                    (status, 0)
                }
            }
            req::FLUSH => (if backend.flush().is_ok() { STATUS_OK } else { STATUS_IOERR }, 0),
            req::GET_ID => {
                let mut id = [0u8; 20];
                let n = self.id.len().min(20);
                id[..n].copy_from_slice(&self.id.as_bytes()[..n]);
                let n = writer.write(&id[..(data_len as usize).min(20)]).unwrap_or(0);
                (STATUS_OK, n as u64)
            }
            req::IN => (STATUS_IOERR, 0),
            _ => (STATUS_UNSUPP, 0),
        };
        // The status byte is the last byte of the chain.
        let mut writer = chain.writer(&a.mem);
        writer.skip(data_len as usize);
        let _ = writer.write(&[status]);
        (written + 1) as u32
    }
}

impl VirtioDevice for Blk {
    fn device_type(&self) -> u16 {
        device_type::BLOCK
    }

    fn features(&self) -> u64 {
        feature::SEG_MAX | feature::BLK_SIZE | feature::FLUSH | if self.read_only { feature::RO } else { 0 }
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE]
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        read_config_bytes(&self.config(), offset, data);
    }

    fn activate(&mut self, activation: Activation) {
        self.active = Some(activation);
    }

    fn notify(&mut self, queue: usize) {
        if let Some(a) = &self.active {
            a.drain(queue, |chain| self.request(a, chain));
        }
    }

    fn reset(&mut self) {
        self.active = None;
    }

    fn pci_class(&self) -> u32 {
        0x01_00_00
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_keeps_the_base_unchanged() {
        let base: Vec<u8> = (0..3 * OVERLAY_BLOCK + 100).map(|i| i as u8).collect();
        let mut overlay = Overlay::new(Box::new(base.clone()));
        overlay.write_at(4000, &[0xaa; 200]).unwrap();
        let mut all = vec![0u8; base.len()];
        overlay.read_at(0, &mut all).unwrap();
        let mut expected = base.clone();
        expected[4000..4200].fill(0xaa);
        assert_eq!(all, expected);
        let mut tail = [0u8; 50];
        overlay.read_at(base.len() as u64 - 50, &mut tail).unwrap();
        assert_eq!(&tail[..], &base[base.len() - 50..]);
        assert!(overlay.write_at(base.len() as u64 - 10, &[0; 20]).is_err());
        overlay.write_at(base.len() as u64 - 10, &[1; 10]).unwrap();
        overlay.read_at(base.len() as u64 - 10, &mut tail[..10]).unwrap();
        assert_eq!(&tail[..10], &[1; 10]);
    }
}
