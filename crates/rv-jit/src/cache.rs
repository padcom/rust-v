//! Executable memory for generated code: one contiguous region per compiler, filled with a
//! bump allocator.
//!
//! On Apple Silicon the region is mapped with `MAP_JIT` and made writable only for the
//! compiling thread (`pthread_jit_write_protect_np`), which is what signed applications with
//! the hardened runtime and the `com.apple.security.cs.allow-jit` entitlement require. Other
//! hosts map it read-write-execute.

use std::ffi::c_void;
use std::ptr::NonNull;

pub struct CodeCache {
    base: NonNull<u8>,
    size: usize,
    used: usize,
}

// SAFETY: the cache only hands out addresses; writing is serialised through `&mut self`.
unsafe impl Send for CodeCache {}

const ALIGN: usize = 16;

impl CodeCache {
    pub fn new(size: usize) -> Result<Self, String> {
        let base = map(size)?;
        Ok(CodeCache { base, size, used: 0 })
    }

    /// Copies `code` into the cache and makes it executable. `None` if the cache is full.
    pub fn install(&mut self, code: &[u8]) -> Option<*const u8> {
        let start = self.used.next_multiple_of(ALIGN);
        if start + code.len() > self.size {
            return None;
        }
        // SAFETY: `start..start + len` is inside the mapping and not yet handed out.
        let dst = unsafe { self.base.as_ptr().add(start) };
        with_write_access(|| {
            // SAFETY: as above; nobody executes this range until we return its address.
            unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), dst, code.len()) };
        });
        // SAFETY: the range was just written.
        unsafe { wasmtime_internal_jit_icache_coherence::clear_cache(dst as *const c_void, code.len()) }
            .expect("instruction cache maintenance failed");
        let _ = wasmtime_internal_jit_icache_coherence::pipeline_flush_mt();
        self.used = start + code.len();
        Some(dst)
    }
}

impl Drop for CodeCache {
    fn drop(&mut self) {
        unmap(self.base, self.size);
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn with_write_access(f: impl FnOnce()) {
    // SAFETY: toggles the calling thread's view of MAP_JIT memory.
    unsafe { libc::pthread_jit_write_protect_np(0) };
    f();
    // SAFETY: as above.
    unsafe { libc::pthread_jit_write_protect_np(1) };
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn with_write_access(f: impl FnOnce()) {
    f();
}

#[cfg(unix)]
fn map(size: usize) -> Result<NonNull<u8>, String> {
    #[cfg(target_os = "macos")]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT;
    #[cfg(not(target_os = "macos"))]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    let prot = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
    // SAFETY: anonymous mapping with no address hint.
    let p = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, flags, -1, 0) };
    if p == libc::MAP_FAILED {
        return Err(format!(
            "mmap of {size} bytes of executable memory failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(NonNull::new(p as *mut u8).unwrap())
}

#[cfg(unix)]
fn unmap(base: NonNull<u8>, size: usize) {
    // SAFETY: mapped by `map` with this size.
    unsafe { libc::munmap(base.as_ptr() as *mut c_void, size) };
}

#[cfg(windows)]
fn map(size: usize) -> Result<NonNull<u8>, String> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc};
    // SAFETY: fresh allocation with no address hint.
    let p = unsafe { VirtualAlloc(std::ptr::null(), size, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) };
    NonNull::new(p as *mut u8).ok_or_else(|| {
        format!(
            "VirtualAlloc of {size} bytes of executable memory failed: {}",
            std::io::Error::last_os_error()
        )
    })
}

#[cfg(windows)]
fn unmap(base: NonNull<u8>, _size: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: allocated by `map`.
    unsafe { VirtualFree(base.as_ptr() as *mut c_void, 0, MEM_RELEASE) };
}
