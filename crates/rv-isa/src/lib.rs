//! RISC-V instruction definitions, decoder and disassembler.
//!
//! The decoder turns raw instruction bits into a compact, fixed-size [`Insn`] that both the
//! interpreter and (later) the JIT consume. Compressed instructions are expanded to their
//! 32-bit equivalents.

mod decode;
mod disasm;
mod op;

pub use decode::{decode, insn_len};
pub use disasm::reg_name;
pub use op::{Format, Op};

/// A decoded instruction.
///
/// Field meaning depends on [`Op::format`]:
/// - CSR ops: `imm` holds the CSR number, `rs1` holds the source register or the 5-bit immediate.
/// - AMOs: `rs3` holds the aq/rl bits.
/// - `fence`: `imm` holds `fm | pred | succ` (instruction bits 31:20).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Insn {
    pub op: Op,
    pub rd: u8,
    pub rs1: u8,
    pub rs2: u8,
    pub rs3: u8,
    /// Length in bytes: 2 for compressed instructions, 4 otherwise.
    pub len: u8,
    pub imm: i32,
    /// The original encoding (low 16 bits only for compressed instructions).
    pub raw: u32,
}

const _: () = assert!(size_of::<Insn>() == 16);

impl Insn {
    pub const fn illegal(raw: u32, len: u8) -> Self {
        Insn { op: Op::Illegal, rd: 0, rs1: 0, rs2: 0, rs3: 0, len, imm: 0, raw }
    }

    /// The CSR number of a Zicsr instruction.
    pub const fn csr(&self) -> u16 {
        self.imm as u16 & 0xfff
    }
}

#[cfg(test)]
mod tests;
