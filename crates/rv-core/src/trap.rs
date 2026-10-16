//! Exception and interrupt causes.

/// A synchronous exception raised by an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exception {
    pub cause: u64,
    pub tval: u64,
}

impl Exception {
    pub const fn new(cause: u64, tval: u64) -> Self {
        Exception { cause, tval }
    }

    pub const fn illegal(raw: u32) -> Self {
        Exception::new(cause::ILLEGAL_INSN, raw as u64)
    }
}

/// Exception cause codes (`mcause` with the interrupt bit clear).
pub mod cause {
    pub const INSN_MISALIGNED: u64 = 0;
    pub const INSN_ACCESS: u64 = 1;
    pub const ILLEGAL_INSN: u64 = 2;
    pub const BREAKPOINT: u64 = 3;
    pub const LOAD_MISALIGNED: u64 = 4;
    pub const LOAD_ACCESS: u64 = 5;
    pub const STORE_MISALIGNED: u64 = 6;
    pub const STORE_ACCESS: u64 = 7;
    pub const ECALL_U: u64 = 8;
    pub const ECALL_S: u64 = 9;
    pub const ECALL_M: u64 = 11;
    pub const INSN_PAGE_FAULT: u64 = 12;
    pub const LOAD_PAGE_FAULT: u64 = 13;
    pub const STORE_PAGE_FAULT: u64 = 15;
}

/// Interrupt numbers, which are also the bit positions in `mip`/`mie`.
pub mod irq {
    pub const SSI: u64 = 1;
    pub const MSI: u64 = 3;
    pub const STI: u64 = 5;
    pub const MTI: u64 = 7;
    pub const SEI: u64 = 9;
    pub const MEI: u64 = 11;
    pub const LCOFI: u64 = 13;

    /// Interrupts in decreasing priority order.
    pub const PRIORITY: [u64; 7] = [MEI, MSI, MTI, SEI, SSI, STI, LCOFI];
}

pub const INTERRUPT_BIT: u64 = 1 << 63;
