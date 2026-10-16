//! SiFive-compatible CLINT: machine software interrupts (`msip`) and the machine timer.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rv_core::trap::irq;
use rv_core::{Device, HartShared, TimeSource};

const MSIP_BASE: u64 = 0x0000;
const MTIMECMP_BASE: u64 = 0x4000;
const MTIME: u64 = 0xbff8;
pub const SIZE: u64 = 0x10000;

pub struct Clint {
    harts: Vec<Arc<HartShared>>,
    mtimecmp: Vec<AtomicU64>,
    time: Arc<dyn TimeSource>,
}

/// Reads `size` bytes at byte offset `off` within a 64-bit register.
fn read_part(reg: u64, off: u64, size: u8) -> u64 {
    let v = reg >> (8 * off);
    if size >= 8 { v } else { v & ((1 << (8 * size as u64)) - 1) }
}

/// Replaces `size` bytes at byte offset `off` within a 64-bit register.
fn write_part(reg: u64, off: u64, size: u8, value: u64) -> u64 {
    let mask = if size >= 8 { u64::MAX } else { ((1 << (8 * size as u64)) - 1) << (8 * off) };
    (reg & !mask) | ((value << (8 * off)) & mask)
}

impl Clint {
    pub fn new(harts: Vec<Arc<HartShared>>, time: Arc<dyn TimeSource>) -> Self {
        let mtimecmp = harts.iter().map(|_| AtomicU64::new(u64::MAX)).collect();
        Clint { harts, mtimecmp, time }
    }

    pub fn mtimecmp(&self, hart: usize) -> u64 {
        self.mtimecmp[hart].load(Ordering::Relaxed)
    }

    /// Re-evaluates MTIP for one hart.
    pub fn update_timer(&self, hart: usize) {
        self.update_timer_at(hart, &self.harts[hart], self.time.mtime());
    }

    /// Re-evaluates MTIP for every hart. Only touches `mip` when the level changes.
    pub fn update_timers(&self) {
        let now = self.time.mtime();
        for (h, shared) in self.harts.iter().enumerate() {
            self.update_timer_at(h, shared, now);
        }
    }

    fn update_timer_at(&self, hart: usize, shared: &HartShared, now: u64) {
        let pending = now >= self.mtimecmp[hart].load(Ordering::Relaxed);
        let current = shared.mip.load(Ordering::Relaxed) & (1 << irq::MTI) != 0;
        if pending != current {
            shared.set_irq(1 << irq::MTI, pending);
        }
    }
}

impl Device for Clint {
    fn read(&self, offset: u64, size: u8) -> u64 {
        let n = self.harts.len() as u64;
        match offset {
            o if o < MSIP_BASE + 4 * n => {
                let h = (o / 4) as usize;
                (self.harts[h].mip.load(Ordering::Relaxed) >> irq::MSI) & 1
            }
            o if (MTIMECMP_BASE..MTIMECMP_BASE + 8 * n).contains(&o) => {
                let h = ((o - MTIMECMP_BASE) / 8) as usize;
                read_part(self.mtimecmp[h].load(Ordering::Relaxed), o % 8, size)
            }
            o if (MTIME..MTIME + 8).contains(&o) => read_part(self.time.mtime(), o - MTIME, size),
            _ => 0,
        }
    }

    fn write(&self, offset: u64, size: u8, value: u64) {
        let n = self.harts.len() as u64;
        match offset {
            o if o < MSIP_BASE + 4 * n => {
                let h = (o / 4) as usize;
                self.harts[h].set_irq(1 << irq::MSI, value & 1 != 0);
            }
            o if (MTIMECMP_BASE..MTIMECMP_BASE + 8 * n).contains(&o) => {
                let h = ((o - MTIMECMP_BASE) / 8) as usize;
                let old = self.mtimecmp[h].load(Ordering::Relaxed);
                self.mtimecmp[h].store(write_part(old, o % 8, size, value), Ordering::Relaxed);
                self.update_timer_at(h, &self.harts[h], self.time.mtime());
            }
            // mtime is read-only here: it is derived from the host clock.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_register_access() {
        assert_eq!(read_part(0x1122_3344_5566_7788, 4, 4), 0x1122_3344);
        assert_eq!(read_part(0x1122_3344_5566_7788, 0, 8), 0x1122_3344_5566_7788);
        assert_eq!(write_part(u64::MAX, 0, 4, 0), 0xffff_ffff_0000_0000);
        assert_eq!(write_part(0, 4, 4, 0xabcd), 0xabcd_0000_0000);
    }
}
