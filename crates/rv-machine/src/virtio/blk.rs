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
