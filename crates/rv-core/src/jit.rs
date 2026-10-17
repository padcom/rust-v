//! Interface between the hart and a native-code block compiler (see the `rv-jit` crate).
//!
//! A compiled block executes its instructions against [`HartState`] and keeps `pc` up to date.
//! On leaving a block through a static successor it may tail-call that successor's code
//! directly ("chaining"), through a [`ChainSlot`] the dispatcher fills in once it has seen
//! which block follows. Before chaining, generated code checks the instruction budget,
//! `exit_request`, the page's code generation and the hart's FENCE.I epoch. Chains only link
//! blocks within one physical page, so the virtual-to-physical mapping cannot have changed
//! between them; privilege, MPRV and pointer masking only change through instructions that are
//! never compiled.
//!
//! Generated code reports through [`JitState`]: instructions retired (accumulated over the
//! whole chain), and the chain slot of the exit it returned through (or 0). The function's
//! return value is 0, or [`EXCEPTION`] if an instruction raised an exception; the exception is
//! left in `Hart::jit_exception` and `pc` points at the faulting instruction.

use std::mem::offset_of;
use std::sync::atomic::AtomicU32;

use rv_isa::Insn;

use crate::hart::{Hart, HartState, Priv};
use crate::mmu::{Access, TLB_ENTRIES, Tlb, TlbEntry};
use crate::trap::Exception;

/// Address of a compiled block (a function with the compiler's internal calling convention).
pub type JitCode = u64;

/// Calls compiled code from Rust: `entry(state, hart, code)`.
pub type JitEntry = unsafe extern "C" fn(state: *mut HartState, hart: *mut Hart, code: JitCode) -> u64;

/// Returned by compiled code when it stopped on an exception.
pub const EXCEPTION: u64 = 1 << 63;

/// Per-hart state shared with generated code.
#[derive(Debug)]
#[repr(C)]
pub struct JitState {
    /// Instructions retired by compiled code since the dispatcher entered it.
    pub retired: u64,
    /// Instructions compiled code may still run before returning to the dispatcher.
    pub budget: i64,
    /// Address of the [`ChainSlot`] the last exit went through, or 0.
    pub chain_from: u64,
    /// Address of the hart's `exit_request` word.
    pub exit_request: u64,
    /// Incremented by FENCE.I; chain slots record the epoch they were linked at.
    pub fence_epoch: u32,
    pub jump_cache: [JumpCacheEntry; JUMP_CACHE_ENTRIES],
}

impl Default for JitState {
    fn default() -> Self {
        JitState {
            retired: 0,
            budget: 0,
            chain_from: 0,
            exit_request: 0,
            fence_epoch: 0,
            jump_cache: [JumpCacheEntry::default(); JUMP_CACHE_ENTRIES],
        }
    }
}

/// Entries in the per-hart indirect-jump cache.
pub const JUMP_CACHE_ENTRIES: usize = 1024;

/// An indirect-jump cache entry: compiled code for the block at host address `host`, valid
/// while the page generation and FENCE.I epoch match and data accesses run at `data_priv`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct JumpCacheEntry {
    pub host: u64,
    pub code: u64,
    pub page_generation: u64,
    pub generation: u32,
    pub epoch: u32,
    pub data_priv: u32,
    pub _pad: u32,
}

/// Index of `host` in the jump cache (instructions are at least 2-byte aligned).
#[inline(always)]
pub const fn jump_cache_index(host: u64) -> usize {
    ((host >> 1) as usize) & (JUMP_CACHE_ENTRIES - 1)
}

/// A static exit of a compiled block. Filled in by the dispatcher, read by generated code.
#[derive(Debug, Default)]
#[repr(C)]
pub struct ChainSlot {
    /// Code of the successor block, or 0 if not linked.
    pub target: u64,
    /// FENCE.I epoch at which the link was made.
    pub epoch: u32,
    /// RAM page of the block owning the slot; only successors in the same page are linked.
    pub page: u32,
}

/// What the compiler needs to know about a block besides its instructions.
#[derive(Clone, Copy, Debug)]
pub struct BlockInfo {
    /// Host address of the block: identifies it when an asynchronous result comes back.
    pub host: u64,
    /// Privilege data accesses are made at (selects the inlined TLB table).
    pub data_priv: Priv,
    /// The block's page generation counter and its value when the block was decoded.
    pub page_generation: *const AtomicU32,
    pub generation: u32,
    /// RAM page index of the block.
    pub page: usize,
    /// Code cache epoch the request was made in; results from an older epoch are stale.
    pub cache_epoch: u32,
    /// Vector register length in bytes.
    pub vlenb: u32,
}

// SAFETY: the generation pointer refers to the bus, which outlives every hart and compiler.
unsafe impl Send for BlockInfo {}

/// Outcome of a compilation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compiled {
    /// Code for (a prefix of) the block.
    Code(JitCode),
    /// Not even the first instruction is supported.
    Unsupported,
    /// Compiling in the background; the result arrives through [`BlockCompiler::poll`].
    Pending,
    /// The code cache is full. The hart must drop all compiled code and call
    /// [`BlockCompiler::reset`].
    CacheFull,
}

pub use crate::block::{Region, RegionBlock};

pub trait BlockCompiler: Send {
    /// Compiles `region` (or part of it: at least a prefix of its entry block), now or in the
    /// background.
    fn compile(&mut self, region: &std::sync::Arc<Region>, info: BlockInfo) -> Compiled;

    /// A finished background compilation: the request's `info` and its result (never
    /// `Pending`).
    fn poll(&mut self) -> Option<(BlockInfo, Compiled)> {
        None
    }

    /// The trampoline used to enter compiled code.
    fn entry(&self) -> JitEntry;

    /// Empties the code cache (or, for a shared one, acknowledges that it was emptied). The
    /// caller guarantees that it no longer runs or references any compiled code. Requests made
    /// before the reset may still report results; their `cache_epoch` identifies them.
    fn reset(&mut self) {}

    /// Called before the hart uses compiled code (looking it up, linking it, running it);
    /// after [`Enter::Go`], [`BlockCompiler::leave`] must follow. A code cache shared between
    /// harts is only emptied while none of them is between the two.
    fn enter(&mut self) -> Enter {
        Enter::Go
    }

    fn leave(&mut self) {}
}

/// May the hart use compiled code now?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enter {
    Go,
    /// The code cache is being emptied; interpret for now.
    Busy,
    /// The code cache was emptied since the hart last looked: it must drop every reference to
    /// compiled code (and call [`BlockCompiler::reset`]) before using any.
    Stale,
}

/// Layout of [`HartState`] as seen by generated code.
pub mod abi {
    use super::*;

    pub const X: usize = offset_of!(HartState, x);
    pub const PC: usize = offset_of!(HartState, pc);
    pub const V: usize = offset_of!(HartState, v);
    pub const MSTATUS: usize = offset_of!(HartState, csr) + offset_of!(crate::hart::Csrs, mstatus);
    pub const VL: usize = offset_of!(HartState, csr) + offset_of!(crate::hart::Csrs, vl);
    pub const VTYPE: usize = offset_of!(HartState, csr) + offset_of!(crate::hart::Csrs, vtype);
    pub const VSTART: usize = offset_of!(HartState, csr) + offset_of!(crate::hart::Csrs, vstart);
    pub const TLB: usize = offset_of!(HartState, tlb) + offset_of!(Tlb, entries);
    pub const TLB_ENTRY_SIZE: usize = size_of::<TlbEntry>();
    pub const TLB_TAG: usize = offset_of!(TlbEntry, tag);
    pub const TLB_ADDEND: usize = offset_of!(TlbEntry, addend);
    pub const TLB_SIZE: usize = TLB_ENTRIES;
    pub const JIT: usize = offset_of!(HartState, jit);
    pub const JIT_RETIRED: usize = JIT + offset_of!(JitState, retired);
    pub const JIT_BUDGET: usize = JIT + offset_of!(JitState, budget);
    pub const JIT_CHAIN_FROM: usize = JIT + offset_of!(JitState, chain_from);
    pub const JIT_EXIT_REQUEST: usize = JIT + offset_of!(JitState, exit_request);
    pub const JIT_FENCE_EPOCH: usize = JIT + offset_of!(JitState, fence_epoch);
    pub const JUMP_CACHE: usize = JIT + offset_of!(JitState, jump_cache);
    pub const JC_ENTRY_SIZE: usize = size_of::<JumpCacheEntry>();
    pub const JC_HOST: usize = offset_of!(JumpCacheEntry, host);
    pub const JC_CODE: usize = offset_of!(JumpCacheEntry, code);
    pub const JC_PAGE_GENERATION: usize = offset_of!(JumpCacheEntry, page_generation);
    pub const JC_GENERATION: usize = offset_of!(JumpCacheEntry, generation);
    pub const JC_EPOCH: usize = offset_of!(JumpCacheEntry, epoch);
    pub const JC_DATA_PRIV: usize = offset_of!(JumpCacheEntry, data_priv);
    pub const SLOT_TARGET: usize = offset_of!(ChainSlot, target);
    pub const SLOT_EPOCH: usize = offset_of!(ChainSlot, epoch);

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

/// Whether compiled code may run `insn` by calling [`jit_exec`] and carry on: anything that
/// cannot change control flow, privilege or address translation (those end blocks).
pub fn executable_by_helper(insn: &Insn) -> bool {
    insn.op != rv_isa::Op::Illegal && !crate::block::ends_block_at(insn)
}

/// Executes one instruction with the interpreter, for compiled code. The instruction is
/// passed as the two 64-bit halves of its [`Insn`]; `state.pc` must already point at it.
/// Returns 0 on success, 1 if it raised an exception (left in `Hart::jit_exception`).
///
/// # Safety
/// `hart` must be the hart executing the block.
pub unsafe extern "C" fn jit_exec(hart: *mut Hart, lo: u64, hi: u64) -> u64 {
    // SAFETY: by contract; `Insn` is plain data of exactly 16 bytes.
    let (hart, insn) = unsafe { (&mut *hart, std::mem::transmute::<[u64; 2], Insn>([lo, hi])) };
    match hart.execute(&insn) {
        Ok(()) => {
            // The block's exit counts this instruction; a write to minstret must not be.
            if std::mem::take(&mut hart.suppress_instret) {
                hart.state.csr.minstret = hart.state.csr.minstret.wrapping_sub(1);
            }
            0
        }
        Err(e) => {
            hart.jit_exception = Some(e);
            1
        }
    }
}

/// Raises an illegal-instruction exception for the instruction with encoding `raw`, for
/// compiled code that found the instruction's unit disabled. Always returns 1.
///
/// # Safety
/// `hart` must be the hart executing the block.
pub unsafe extern "C" fn jit_illegal(hart: *mut Hart, raw: u64, _unused: u64) -> u64 {
    // SAFETY: by contract.
    let hart = unsafe { &mut *hart };
    hart.jit_exception = Some(Exception::illegal(raw as u32));
    1
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
    /// Runs compiled code (possibly a chain of blocks) with an instruction budget. Returns the
    /// number of instructions attempted.
    pub(crate) fn run_compiled(&mut self, code: JitCode, budget: u64) -> u64 {
        let entry = self.compiler.as_ref().expect("compiled code without a compiler").entry();
        self.state.jit.retired = 0;
        self.state.jit.chain_from = 0;
        self.state.jit.budget = budget.min(i64::MAX as u64) as i64;
        let hart: *mut Hart = self;
        // SAFETY: the compiled code only touches `HartState` through the pointer and the rest
        // of the hart through the helpers above.
        let result = unsafe { entry(&mut *(*hart).state, hart, code) };
        let retired = std::mem::take(&mut self.state.jit.retired);
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
