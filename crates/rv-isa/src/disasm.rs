use std::fmt;

use crate::{Format, Insn};

const ABI_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5", "a6",
    "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
];

/// ABI name of integer register `x<n>`.
pub fn reg_name(n: u8) -> &'static str {
    ABI_NAMES[n as usize & 31]
}

impl fmt::Display for Insn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.op.mnemonic();
        let rd = reg_name(self.rd);
        let rs1 = reg_name(self.rs1);
        let rs2 = reg_name(self.rs2);
        let imm = self.imm;
        match self.op.format() {
            Format::None => write!(f, "{m}"),
            Format::R => write!(f, "{m} {rd}, {rs1}, {rs2}"),
            Format::I => write!(f, "{m} {rd}, {rs1}, {imm}"),
            Format::Load => write!(f, "{m} {rd}, {imm}({rs1})"),
            Format::Store => write!(f, "{m} {rs2}, {imm}({rs1})"),
            Format::Branch => write!(f, "{m} {rs1}, {rs2}, pc{imm:+}"),
            Format::U => write!(f, "{m} {rd}, {:#x}", (imm as u32) >> 12),
            Format::J => write!(f, "{m} {rd}, pc{imm:+}"),
            Format::Csr => write!(f, "{m} {rd}, {:#x}, {rs1}", self.csr()),
            Format::CsrI => write!(f, "{m} {rd}, {:#x}, {}", self.csr(), self.rs1),
            Format::Amo => write!(f, "{m} {rd}, {rs2}, ({rs1})"),
            Format::Lr => write!(f, "{m} {rd}, ({rs1})"),
            Format::SfenceVma => write!(f, "{m} {rs1}, {rs2}"),
            Format::Fence => write!(f, "{m} {:#x}, {:#x}", (imm >> 4) & 0xf, imm & 0xf),
            Format::FLoad => write!(f, "{m} f{}, {imm}({rs1})", self.rd),
            Format::FStore => write!(f, "{m} f{}, {imm}({rs1})", self.rs2),
            Format::FR => write!(f, "{m} f{}, f{}, f{}", self.rd, self.rs1, self.rs2),
            Format::FR1 => write!(f, "{m} f{}, f{}", self.rd, self.rs1),
            Format::FR4 => write!(f, "{m} f{}, f{}, f{}, f{}", self.rd, self.rs1, self.rs2, self.rs3),
            Format::FToX => write!(f, "{m} {rd}, f{}", self.rs1),
            Format::XToF => write!(f, "{m} f{}, {rs1}", self.rd),
            Format::FCmp => write!(f, "{m} {rd}, f{}, f{}", self.rs1, self.rs2),
            Format::FLi => write!(f, "{m} f{}, {}", self.rd, self.rs1),
            Format::R1 => write!(f, "{m} {rd}, {rs1}"),
            Format::Cbo => write!(f, "{m} ({rs1})"),
        }
    }
}
