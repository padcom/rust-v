//! The physical address space: RAM plus memory-mapped devices.

use std::sync::Arc;

use crate::code::CodePages;
use crate::mem::{GuestRam, PAGE_SHIFT};

/// A memory-mapped device. Devices are shared between hart threads, so they take `&self`
/// and use interior mutability.
pub trait Device: Send + Sync {
    /// Reads `size` bytes at `offset` from the start of the device's region.
    fn read(&self, offset: u64, size: u8) -> u64;
    /// Writes the low `size` bytes of `value` at `offset`.
    fn write(&self, offset: u64, size: u8, value: u64);
}

struct Mmio {
    base: u64,
    size: u64,
    device: Arc<dyn Device>,
}

pub struct Bus {
    ram: GuestRam,
    ram_base: u64,
    mmio: Vec<Mmio>,
    code: CodePages,
}

impl Bus {
    pub fn new(ram_base: u64, ram_size: u64) -> Self {
        Bus { ram: GuestRam::new(ram_size), ram_base, mmio: Vec::new(), code: CodePages::new(ram_size) }
    }

    /// Maps `device` at `[base, base + size)`. Panics on overlap with RAM or another device.
    pub fn add_device(&mut self, base: u64, size: u64, device: Arc<dyn Device>) {
        let end = base + size;
        let overlaps = |b: u64, s: u64| base < b + s && b < end;
        assert!(!overlaps(self.ram_base, self.ram.size()), "device at {base:#x} overlaps RAM");
        assert!(self.mmio.iter().all(|m| !overlaps(m.base, m.size)), "device at {base:#x} overlaps a device");
        self.mmio.push(Mmio { base, size, device });
    }

    /// Read access to RAM. Writes must go through [`Bus::ram_store`] (or [`Bus::write`]) so
    /// that cached code is invalidated.
    pub fn ram(&self) -> &GuestRam {
        &self.ram
    }

    pub fn code(&self) -> &CodePages {
        &self.code
    }

    /// RAM page index of a host pointer into RAM.
    #[inline(always)]
    pub fn ram_page_of(&self, host: *const u8) -> usize {
        (host as usize - self.ram.host_ptr(0) as usize) >> PAGE_SHIFT
    }

    /// Stores into RAM at offset `off`, invalidating cached code in the page.
    #[inline]
    pub fn ram_store(&self, off: u64, size: u8, value: u64) {
        self.ram.store(off, size, value);
        self.code.note_write((off >> PAGE_SHIFT) as usize);
        if (off & 0xfff) + size as u64 > 0x1000 {
            self.code.note_write(((off + size as u64 - 1) >> PAGE_SHIFT) as usize);
        }
    }

    pub fn ram_base(&self) -> u64 {
        self.ram_base
    }

    pub fn ram_size(&self) -> u64 {
        self.ram.size()
    }

    /// Offset into RAM if `[paddr, paddr + len)` lies entirely within RAM.
    #[inline]
    pub fn ram_offset(&self, paddr: u64, len: u64) -> Option<u64> {
        let off = paddr.wrapping_sub(self.ram_base);
        (off < self.ram.size() && len <= self.ram.size() - off).then_some(off)
    }

    /// Host pointer for `[paddr, paddr + len)` if it lies entirely within RAM.
    #[inline]
    pub fn ram_ptr(&self, paddr: u64, len: u64) -> Option<*mut u8> {
        self.ram_offset(paddr, len).map(|off| self.ram.host_ptr(off))
    }

    fn device(&self, paddr: u64, size: u8) -> Option<(&Mmio, u64)> {
        self.mmio.iter().find_map(|m| {
            let off = paddr.wrapping_sub(m.base);
            (off < m.size && size as u64 <= m.size - off).then_some((m, off))
        })
    }

    /// Physical read. `None` means nothing is mapped there (an access fault).
    pub fn read(&self, paddr: u64, size: u8) -> Option<u64> {
        if let Some(off) = self.ram_offset(paddr, size as u64) {
            return Some(self.ram.load(off, size));
        }
        self.device(paddr, size).map(|(m, off)| m.device.read(off, size))
    }

    /// Physical write. Returns `false` if nothing is mapped there (an access fault).
    pub fn write(&self, paddr: u64, size: u8, value: u64) -> bool {
        if let Some(off) = self.ram_offset(paddr, size as u64) {
            self.ram_store(off, size, value);
            return true;
        }
        match self.device(paddr, size) {
            Some((m, off)) => {
                m.device.write(off, size, value);
                true
            }
            None => false,
        }
    }

    /// Copies `data` into RAM at `paddr`. Panics if it does not fit.
    pub fn load_image(&self, paddr: u64, data: &[u8]) {
        let off = self
            .ram_offset(paddr, data.len() as u64)
            .unwrap_or_else(|| panic!("image at {paddr:#x} (+{:#x}) does not fit in RAM", data.len()));
        self.ram.write_bytes(off, data);
        let last = off + data.len().max(1) as u64 - 1;
        for page in (off >> PAGE_SHIFT)..=(last >> PAGE_SHIFT) {
            self.code.note_write(page as usize);
        }
    }
}
