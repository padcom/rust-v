//! Split virtqueues (virtio 1.x §2.7), including indirect descriptors and event-index
//! notification suppression.

use std::sync::atomic::{Ordering, fence};

use rv_core::{DmaError, GuestMemory};

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;
const AVAIL_F_NO_INTERRUPT: u16 = 1;

#[derive(Debug)]
pub enum QueueError {
    Dma(DmaError),
    /// The driver handed us a malformed descriptor chain.
    Invalid(&'static str),
}

impl From<DmaError> for QueueError {
    fn from(e: DmaError) -> Self {
        QueueError::Dma(e)
    }
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::Dma(e) => write!(f, "{e}"),
            QueueError::Invalid(why) => write!(f, "invalid descriptor chain: {why}"),
        }
    }
}

/// A guest buffer: physical address and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffer {
    pub addr: u64,
    pub len: u32,
}

/// A descriptor chain taken from the available ring: device-readable buffers followed by
/// device-writable ones.
#[derive(Debug, Default)]
pub struct Chain {
    pub head: u16,
    pub readable: Vec<Buffer>,
    pub writable: Vec<Buffer>,
}

impl Chain {
    pub fn readable_len(&self) -> u64 {
        self.readable.iter().map(|b| b.len as u64).sum()
    }

    pub fn writable_len(&self) -> u64 {
        self.writable.iter().map(|b| b.len as u64).sum()
    }

    /// Reader over the device-readable part.
    pub fn reader<'a>(&'a self, mem: &'a GuestMemory) -> ChainIo<'a> {
        ChainIo { mem, bufs: &self.readable, index: 0, offset: 0 }
    }

    /// Writer over the device-writable part.
    pub fn writer<'a>(&'a self, mem: &'a GuestMemory) -> ChainIo<'a> {
        ChainIo { mem, bufs: &self.writable, index: 0, offset: 0 }
    }
}

/// Sequential access to a list of guest buffers.
pub struct ChainIo<'a> {
    mem: &'a GuestMemory,
    bufs: &'a [Buffer],
    index: usize,
    offset: u32,
}

impl ChainIo<'_> {
    /// Bytes left.
    pub fn remaining(&self) -> u64 {
        self.bufs[self.index.min(self.bufs.len())..].iter().map(|b| b.len as u64).sum::<u64>()
            - self.offset as u64
    }

    /// Reads up to `out.len()` bytes; returns how many were read.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, DmaError> {
        self.transfer(out.len(), |mem, addr, range| mem.read(addr, &mut out[range]))
    }

    /// Writes up to `data.len()` bytes; returns how many were written.
    pub fn write(&mut self, data: &[u8]) -> Result<usize, DmaError> {
        self.transfer(data.len(), |mem, addr, range| mem.write(addr, &data[range]))
    }

    /// Skips `n` bytes (or what is left).
    pub fn skip(&mut self, n: usize) -> usize {
        self.transfer(n, |_, _, _| Ok(())).unwrap()
    }

    fn transfer(
        &mut self,
        len: usize,
        mut f: impl FnMut(&GuestMemory, u64, std::ops::Range<usize>) -> Result<(), DmaError>,
    ) -> Result<usize, DmaError> {
        let mut done = 0;
        while done < len && self.index < self.bufs.len() {
            let b = self.bufs[self.index];
            let n = ((b.len - self.offset) as usize).min(len - done);
            f(self.mem, b.addr + self.offset as u64, done..done + n)?;
            done += n;
            self.offset += n as u32;
            if self.offset == b.len {
                self.index += 1;
                self.offset = 0;
            }
        }
        Ok(done)
    }
}

/// Queue state, as configured by the driver through the transport.
#[derive(Debug)]
pub struct Queue {
    pub max_size: u16,
    pub size: u16,
    pub ready: bool,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    /// VIRTIO_F_RING_EVENT_IDX was negotiated.
    pub event_idx: bool,
    next_avail: u16,
    next_used: u16,
    /// `next_used` when the driver was last notified.
    signalled_used: u16,
}

impl Queue {
    pub fn new(max_size: u16) -> Self {
        Queue {
            max_size,
            size: max_size,
            ready: false,
            desc: 0,
            avail: 0,
            used: 0,
            event_idx: false,
            next_avail: 0,
            next_used: 0,
            signalled_used: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Queue::new(self.max_size);
    }

    pub fn save(&self, w: &mut rv_core::snapshot::Writer) {
        w.u16(self.size);
        w.bool(self.ready);
        w.u64(self.desc);
        w.u64(self.avail);
        w.u64(self.used);
        w.bool(self.event_idx);
        w.u16(self.next_avail);
        w.u16(self.next_used);
        w.u16(self.signalled_used);
    }

    pub fn restore(&mut self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        self.size = r.u16()?.min(self.max_size);
        self.ready = r.bool()?;
        self.desc = r.u64()?;
        self.avail = r.u64()?;
        self.used = r.u64()?;
        self.event_idx = r.bool()?;
        self.next_avail = r.u16()?;
        self.next_used = r.u16()?;
        self.signalled_used = r.u16()?;
        Ok(())
    }

    fn avail_idx(&self, mem: &GuestMemory) -> Result<u16, DmaError> {
        let idx = mem.load(self.avail + 2, 2)? as u16;
        // Descriptors and ring entries the index covers are read after it.
        fence(Ordering::Acquire);
        Ok(idx)
    }

    /// Takes the next available descriptor chain, if any.
    pub fn pop(&mut self, mem: &GuestMemory) -> Result<Option<Chain>, QueueError> {
        if !self.ready || self.size == 0 {
            return Ok(None);
        }
        let mut avail_idx = self.avail_idx(mem)?;
        if avail_idx == self.next_avail {
            if !self.event_idx {
                return Ok(None);
            }
            // Ask to be notified when the driver adds the next buffer, then look once more to
            // close the race with a driver that added one meanwhile.
            self.set_avail_event(mem, self.next_avail)?;
            fence(Ordering::SeqCst);
            avail_idx = self.avail_idx(mem)?;
            if avail_idx == self.next_avail {
                return Ok(None);
            }
        }
        if avail_idx.wrapping_sub(self.next_avail) > self.size {
            return Err(QueueError::Invalid("available index ran ahead of the ring"));
        }
        let slot = self.next_avail % self.size;
        let head = mem.load(self.avail + 4 + 2 * slot as u64, 2)? as u16;
        self.next_avail = self.next_avail.wrapping_add(1);
        if self.event_idx {
            self.set_avail_event(mem, self.next_avail)?;
        }
        self.walk(mem, head).map(Some)
    }

    fn set_avail_event(&self, mem: &GuestMemory, idx: u16) -> Result<(), DmaError> {
        mem.store(self.used + 4 + 8 * self.size as u64, 2, idx as u64)
    }

    fn walk(&self, mem: &GuestMemory, head: u16) -> Result<Chain, QueueError> {
        let mut chain = Chain { head, ..Chain::default() };
        let (mut table, mut table_len) = (self.desc, self.size as u32);
        let mut index = head;
        let mut indirect = false;
        let mut seen = 0u32;
        loop {
            if index as u32 >= table_len {
                return Err(QueueError::Invalid("descriptor index out of range"));
            }
            let mut raw = [0u8; 16];
            mem.read(table + 16 * index as u64, &mut raw)?;
            let addr = u64::from_le_bytes(raw[0..8].try_into().unwrap());
            let len = u32::from_le_bytes(raw[8..12].try_into().unwrap());
            let flags = u16::from_le_bytes([raw[12], raw[13]]);
            let next = u16::from_le_bytes([raw[14], raw[15]]);
            if flags & DESC_F_INDIRECT != 0 {
                if indirect || flags & DESC_F_NEXT != 0 || len % 16 != 0 || len == 0 {
                    return Err(QueueError::Invalid("bad indirect descriptor"));
                }
                (table, table_len) = (addr, len / 16);
                index = 0;
                indirect = true;
                seen = 0;
                continue;
            }
            let buf = Buffer { addr, len };
            if flags & DESC_F_WRITE != 0 {
                chain.writable.push(buf);
            } else if chain.writable.is_empty() {
                chain.readable.push(buf);
            } else {
                return Err(QueueError::Invalid("readable descriptor after a writable one"));
            }
            seen += 1;
            if seen > table_len {
                return Err(QueueError::Invalid("descriptor loop"));
            }
            if flags & DESC_F_NEXT == 0 {
                return Ok(chain);
            }
            index = next;
        }
    }

    /// Returns a chain to the driver, `written` bytes having been stored into it.
    pub fn push(&mut self, mem: &GuestMemory, head: u16, written: u32) -> Result<(), DmaError> {
        let slot = self.next_used % self.size;
        let elem = self.used + 4 + 8 * slot as u64;
        mem.store(elem, 4, head as u64)?;
        mem.store(elem + 4, 4, written as u64)?;
        self.next_used = self.next_used.wrapping_add(1);
        // The element must be visible before the index that publishes it.
        fence(Ordering::Release);
        mem.store(self.used + 2, 2, self.next_used as u64)
    }

    /// The device's and the driver's view of the rings, for diagnosing stalls.
    pub fn debug(&self, mem: &GuestMemory) -> String {
        if !self.ready {
            return "not ready".into();
        }
        let load = |addr: u64| mem.load(addr, 2).map_or_else(|_| "?".to_string(), |v| v.to_string());
        let ring = 4 + 2 * self.size as u64;
        format!(
            "size {} | avail: idx {} flags {} taken {} | used: idx {} put {} signalled {} | used_event {} avail_event {}",
            self.size,
            load(self.avail + 2),
            load(self.avail),
            self.next_avail,
            load(self.used + 2),
            self.next_used,
            self.signalled_used,
            load(self.avail + ring),
            load(self.used + 4 + 8 * self.size as u64),
        )
    }

    /// Whether the driver wants an interrupt for the chains pushed since the last one.
    pub fn needs_interrupt(&mut self, mem: &GuestMemory) -> Result<bool, DmaError> {
        fence(Ordering::SeqCst);
        let (old, new) = (self.signalled_used, self.next_used);
        if old == new {
            return Ok(false);
        }
        self.signalled_used = new;
        if self.event_idx {
            let used_event = mem.load(self.avail + 4 + 2 * self.size as u64, 2)? as u16;
            Ok(new.wrapping_sub(used_event).wrapping_sub(1) < new.wrapping_sub(old))
        } else {
            Ok(mem.load(self.avail, 2)? as u16 & AVAIL_F_NO_INTERRUPT == 0)
        }
    }
}
