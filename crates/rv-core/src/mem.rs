//! Guest RAM and raw host-memory accessors.
//!
//! RAM is shared between hart threads, so every access goes through atomics. Aligned accesses
//! use relaxed atomic loads/stores, which compile to plain loads/stores on x86-64 and AArch64,
//! so this costs nothing over raw pointer access while keeping the Rust side free of data races.
//! Guest-visible ordering is provided by the fences that `FENCE`/AMO instructions emit.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering::Relaxed};

#[cfg(not(target_endian = "little"))]
compile_error!("risc-vm requires a little-endian host");

pub const PAGE_SIZE: u64 = 4096;
pub const PAGE_SHIFT: u32 = 12;

/// Loads `size` (1, 2, 4 or 8) bytes from host memory, little-endian, zero-extended.
///
/// # Safety
/// `ptr..ptr + size` must be valid, readable memory that is only ever accessed atomically.
#[inline(always)]
pub unsafe fn host_load(ptr: *const u8, size: u8) -> u64 {
    let aligned = ptr as usize & (size as usize - 1) == 0;
    // SAFETY: the caller guarantees validity; the atomic types are only used when aligned.
    unsafe {
        match size {
            1 => (*(ptr as *const AtomicU8)).load(Relaxed) as u64,
            2 if aligned => (*(ptr as *const AtomicU16)).load(Relaxed) as u64,
            4 if aligned => (*(ptr as *const AtomicU32)).load(Relaxed) as u64,
            8 if aligned => (*(ptr as *const AtomicU64)).load(Relaxed),
            _ => {
                let mut v = 0u64;
                for i in 0..size as usize {
                    v |= ((*(ptr.add(i) as *const AtomicU8)).load(Relaxed) as u64) << (8 * i);
                }
                v
            }
        }
    }
}

/// Stores the low `size` (1, 2, 4 or 8) bytes of `value` to host memory, little-endian.
///
/// # Safety
/// Same requirements as [`host_load`], and the memory must be writable.
#[inline(always)]
pub unsafe fn host_store(ptr: *mut u8, size: u8, value: u64) {
    let aligned = ptr as usize & (size as usize - 1) == 0;
    // SAFETY: as for `host_load`.
    unsafe {
        match size {
            1 => (*(ptr as *const AtomicU8)).store(value as u8, Relaxed),
            2 if aligned => (*(ptr as *const AtomicU16)).store(value as u16, Relaxed),
            4 if aligned => (*(ptr as *const AtomicU32)).store(value as u32, Relaxed),
            8 if aligned => (*(ptr as *const AtomicU64)).store(value, Relaxed),
            _ => {
                for i in 0..size as usize {
                    (*(ptr.add(i) as *const AtomicU8)).store((value >> (8 * i)) as u8, Relaxed);
                }
            }
        }
    }
}

/// A contiguous block of guest physical RAM.
pub struct GuestRam {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: all access to the memory goes through atomic operations.
unsafe impl Send for GuestRam {}
unsafe impl Sync for GuestRam {}

impl GuestRam {
    /// Allocates `size` bytes of zeroed RAM. The allocation is lazily backed by the host OS,
    /// so untouched guest memory costs nothing.
    pub fn new(size: u64) -> Self {
        assert!(size > 0 && size.is_multiple_of(PAGE_SIZE), "RAM size must be a non-zero multiple of 4 KiB");
        // 2 MiB alignment lets the host back RAM with huge pages where available.
        let layout = Layout::from_size_align(size as usize, 2 << 20).expect("RAM size too large");
        // SAFETY: the layout has non-zero size.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }).expect("out of memory allocating guest RAM");
        GuestRam { ptr, layout }
    }

    pub fn size(&self) -> u64 {
        self.layout.size() as u64
    }

    /// Host address of byte `offset`. The caller must keep `offset` within bounds.
    #[inline(always)]
    pub fn host_ptr(&self, offset: u64) -> *mut u8 {
        debug_assert!(offset <= self.size());
        // SAFETY: in bounds by contract.
        unsafe { self.ptr.as_ptr().add(offset as usize) }
    }

    fn check(&self, offset: u64, len: u64) {
        assert!(offset.checked_add(len).is_some_and(|end| end <= self.size()), "RAM access out of bounds");
    }

    pub fn load(&self, offset: u64, size: u8) -> u64 {
        self.check(offset, size as u64);
        // SAFETY: bounds checked above.
        unsafe { host_load(self.host_ptr(offset), size) }
    }

    pub fn store(&self, offset: u64, size: u8, value: u64) {
        self.check(offset, size as u64);
        // SAFETY: bounds checked above.
        unsafe { host_store(self.host_ptr(offset), size, value) }
    }

    pub fn write_bytes(&self, offset: u64, data: &[u8]) {
        self.check(offset, data.len() as u64);
        for (i, &b) in data.iter().enumerate() {
            // SAFETY: bounds checked above.
            unsafe { host_store(self.host_ptr(offset + i as u64), 1, b as u64) }
        }
    }

    pub fn read_bytes(&self, offset: u64, out: &mut [u8]) {
        self.check(offset, out.len() as u64);
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: bounds checked above.
            *b = unsafe { host_load(self.host_ptr(offset + i as u64), 1) } as u8;
        }
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}
