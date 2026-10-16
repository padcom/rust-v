//! Interface between the hart and a native-code block compiler (see the `rv-jit` crate).
//!
//! A compiled block is a function `fn(state, hart) -> u64` that executes the block's
//! instructions against [`HartState`], keeping `pc` up to date on exit. The return value is the
//! number of instructions retired, with [`EXCEPTION`] set if the next instruction raised an
//! exception; the exception itself is left in `Hart::jit_exception` and `pc` points at the
//! faulting instruction.

use std::mem::offset_of;

use rv_isa::Insn;

use crate::hart::{Hart, HartState, Priv};
use crate::mmu::{Access, TLB_ENTRIES, Tlb, TlbEntry};
use crate::trap::Exception;

pub type JitFn = unsafe extern "C" fn(state: *mut HartState, hart: *mut Hart) -> u64;

/// Set in a compiled block's return value when it stopped on an exception.
pub const EXCEPTION: u64 = 1 << 63;

pub trait BlockCompiler: Send {
    /// Compiles `insns` for execution with data accesses at privilege `data_priv`. Returns
    /// `None` if the block contains anything the compiler does not handle.
    fn compile(&mut self, insns: &[Insn], data_priv: Priv) -> Option<JitFn>;
}

/// Layout of [`HartState`] as seen by generated code.
pub mod abi {
    use super::*;

    pub const X: usize = offset_of!(HartState, x);
    pub const PC: usize = offset_of!(HartState, pc);
    pub const TLB: usize = offset_of!(HartState, tlb) + offset_of!(Tlb, entries);
    pub const TLB_ENTRY_SIZE: usize = size_of::<TlbEntry>();
    pub const TLB_TAG: usize = offset_of!(TlbEntry, tag);
    pub const TLB_ADDEND: usize = offset_of!(TlbEntry, addend);
    pub const TLB_SIZE: usize = TLB_ENTRIES;

    /// Offset of the TLB table used for `access` at privilege `p`.
    pub const fn tlb_table(p: Priv, access: Access) -> usize {
        let p = match p {
            Priv::User => 0,
            Priv::Supervisor => 1,
            Priv::Machine => 2,
        };
        TLB + (p * 3 + access as usize) * TLB_ENTRIES * TLB_ENTRY_SIZE
    }
}

/// Slow-path load for generated code. Returns 0 and writes `*out` on success.
///
/// # Safety
/// `hart` must be the hart executing the block and `out` must be writable.
pub unsafe extern "C" fn jit_load(hart: *mut Hart, addr: u64, size: u64, out: *mut u64) -> u64 {
    // SAFETY: by contract.
    let hart = unsafe { &mut *hart };
    match hart.load(addr, size as u8) {
        Ok(v) => {
            // SAFETY: by contract.
            unsafe { *out = v };
            0
        }
        Err(e) => {
            hart.jit_exception = Some(e);
            1
        }
    }
}

/// Slow-path store for generated code. Returns 0 on success.
///
/// # Safety
/// `hart` must be the hart executing the block.
pub unsafe extern "C" fn jit_store(hart: *mut Hart, addr: u64, size: u64, value: u64) -> u64 {
    // SAFETY: by contract.
    let hart = unsafe { &mut *hart };
    match hart.store(addr, size as u8, value) {
        Ok(()) => 0,
        Err(e) => {
            hart.jit_exception = Some(e);
            1
        }
    }
}

impl Hart {
    /// Runs a compiled block. Returns the number of instructions attempted.
    pub(crate) fn run_compiled(&mut self, f: JitFn) -> u64 {
        let hart: *mut Hart = self;
        // SAFETY: the compiled code only touches `HartState` through the pointer and the rest
        // of the hart through the helpers above.
        let result = unsafe { f(&mut *(*hart).state, hart) };
        let retired = result & 0xffff_ffff;
        self.state.csr.minstret = self.state.csr.minstret.wrapping_add(retired);
        self.state.csr.mcycle = self.state.csr.mcycle.wrapping_add(retired);
        if result & EXCEPTION != 0 {
            let Exception { cause, tval } =
                self.jit_exception.take().expect("compiled block reported an exception");
            self.take_trap(cause, tval);
            return retired + 1;
        }
        retired
    }
}
