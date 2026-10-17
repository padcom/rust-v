//! Tracking of RAM pages that hold decoded (and later, translated) code.
//!
//! Each RAM page has a "contains code" bit and a generation counter. Any write to a page whose
//! bit is set clears the bit and bumps the generation; cached code remembers the generation it
//! was decoded at and is discarded when it no longer matches. This works no matter who writes
//! (this hart, another hart, a DMA-capable device) without any cross-thread messaging, as long
//! as every RAM write either goes through [`CodePages::note_write`] or through a write-TLB entry,
//! and write-TLB entries are never created for code pages.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::mem::PAGE_SHIFT;

pub struct CodePages {
    bits: Box<[AtomicU64]>,
    generations: Box<[AtomicU32]>,
    /// FENCE.I epoch shared by all harts (see [`CodePages::fence_epoch`]).
    fence_epoch: AtomicU32,
}

impl CodePages {
    pub fn new(ram_size: u64) -> Self {
        let pages = (ram_size >> PAGE_SHIFT) as usize;
        CodePages {
            bits: (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
            generations: (0..pages).map(|_| AtomicU32::new(0)).collect(),
            fence_epoch: AtomicU32::new(0),
        }
    }

    /// The machine-wide FENCE.I epoch. Each FENCE.I starts a new one; a hart re-checks cached
    /// blocks against memory once per epoch. Harts adopt the latest epoch whenever they enter
    /// the dispatcher, which only causes extra checks, and lets every hart share chain links.
    pub fn fence_epoch(&self) -> u32 {
        self.fence_epoch.load(Ordering::Acquire)
    }

    /// Starts a new FENCE.I epoch and returns it.
    pub fn new_fence_epoch(&self) -> u32 {
        self.fence_epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    #[inline(always)]
    pub fn is_code(&self, page: usize) -> bool {
        self.bits[page / 64].load(Ordering::Relaxed) & (1 << (page % 64)) != 0
    }

    /// Marks `page` as containing code. Returns `true` if it was not marked before.
    pub fn mark(&self, page: usize) -> bool {
        let bit = 1 << (page % 64);
        self.bits[page / 64].fetch_or(bit, Ordering::SeqCst) & bit == 0
    }

    #[inline(always)]
    pub fn generation(&self, page: usize) -> u32 {
        self.generations[page].load(Ordering::Acquire)
    }

    /// Address of `page`'s generation counter, for generated code.
    pub fn generation_ptr(&self, page: usize) -> *const AtomicU32 {
        &self.generations[page]
    }

    /// Must be called after writing to RAM page `page` by any path other than a write-TLB hit.
    #[inline(always)]
    pub fn note_write(&self, page: usize) {
        if self.is_code(page) {
            self.invalidate(page);
        }
    }

    /// Treats `page` as written: everything cached for it becomes stale, on every hart.
    pub fn invalidate_page(&self, page: usize) {
        self.invalidate(page);
    }

    #[cold]
    fn invalidate(&self, page: usize) {
        self.bits[page / 64].fetch_and(!(1 << (page % 64)), Ordering::SeqCst);
        self.generations[page].fetch_add(1, Ordering::Release);
    }
}
