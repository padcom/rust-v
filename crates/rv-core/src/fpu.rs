//! F, D, Zfa and Zfhmin execution.

use rv_isa::{Insn, Op};
use rv_softfloat::{self as sf, F16, F32, F64, Rm};

use crate::csr::status;
use crate::hart::Hart;
use crate::trap::Exception;

/// Precision of an operation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum P {
    H,
    S,
    D,
}

/// Single- and half-precision values live NaN-boxed in the 64-bit registers.
const BOX_S: u64 = 0xffff_ffff_0000_0000;
const BOX_H: u64 = 0xffff_ffff_ffff_0000;

/// Dispatches a softfloat function on the precision.
macro_rules! sf {
    ($p:expr, $f:ident($($arg:expr),*)) => {
        match $p {
            P::H => sf::$f::<F16>($($arg),*),
            P::S => sf::$f::<F32>($($arg),*),
            P::D => sf::$f::<F64>($($arg),*),
        }
    };
}

fn sign_bit(p: P) -> u64 {
    match p {
        P::H => 1 << 15,
        P::S => 1 << 31,
        P::D => 1 << 63,
    }
}

impl Hart {
    pub(crate) fn fp_enabled(&self) -> bool {
        self.state.csr.mstatus & status::FS != 0
    }

    fn fp_dirty(&mut self) {
        self.state.csr.mstatus |= status::FS;
    }

    /// Reads `f[r]` as precision `p`; improperly NaN-boxed values read as the canonical NaN.
    fn fread(&self, p: P, r: u8) -> u64 {
        let v = self.state.f[r as usize];
        match p {
            P::D => v,
            P::S if v & BOX_S == BOX_S => v & 0xffff_ffff,
            P::S => sf::canonical_nan::<F32>(),
            P::H if v & BOX_H == BOX_H => v & 0xffff,
            P::H => sf::canonical_nan::<F16>(),
        }
    }

    fn fwrite(&mut self, p: P, r: u8, v: u64) {
        self.state.f[r as usize] = match p {
            P::D => v,
            P::S => BOX_S | v,
            P::H => BOX_H | v,
        };
    }

    /// The rounding mode for `insn`: static, or `frm` when the field is 7 ("dynamic").
    fn rounding(&self, insn: &Insn) -> Result<Rm, Exception> {
        let field = insn.imm as u64 & 7;
        let rm = if field == 7 { (self.state.csr.fcsr >> 5) & 7 } else { field };
        Rm::from_bits(rm).ok_or(Exception::illegal(insn.raw))
    }

    fn accrue(&mut self, flags: u8) {
        self.state.csr.fcsr |= flags as u64;
    }

    /// Executes a floating-point instruction (except loads and stores).
    pub(crate) fn execute_fp(&mut self, insn: &Insn) -> Result<(), Exception> {
        use Op::*;
        if !self.fp_enabled() {
            return Err(Exception::illegal(insn.raw));
        }
        let (rd, rs1, rs2, rs3) = (insn.rd, insn.rs1, insn.rs2, insn.rs3);
        let mut fl = 0u8;
        let p = match insn.op {
            FaddS | FsubS | FmulS | FdivS | FsqrtS | FsgnjS | FsgnjnS | FsgnjxS | FminS | FmaxS | FminmS
            | FmaxmS | FeqS | FltS | FleS | FltqS | FleqS | FclassS | FmaddS | FmsubS | FnmsubS | FnmaddS
            | FliS | FroundS | FroundnxS | FcvtWS | FcvtWuS | FcvtLS | FcvtLuS | FcvtSW | FcvtSWu
            | FcvtSL | FcvtSLu => P::S,
            _ => P::D,
        };

        match insn.op {
            FaddS | FaddD | FsubS | FsubD | FmulS | FmulD | FdivS | FdivD => {
                let rm = self.rounding(insn)?;
                let (a, b) = (self.fread(p, rs1), self.fread(p, rs2));
                let r = match insn.op {
                    FaddS | FaddD => sf!(p, add(a, b, rm, &mut fl)),
                    FsubS | FsubD => sf!(p, sub(a, b, rm, &mut fl)),
                    FmulS | FmulD => sf!(p, mul(a, b, rm, &mut fl)),
                    _ => sf!(p, div(a, b, rm, &mut fl)),
                };
                self.fwrite(p, rd, r);
            }
            FsqrtS | FsqrtD => {
                let rm = self.rounding(insn)?;
                let r = sf!(p, sqrt(self.fread(p, rs1), rm, &mut fl));
                self.fwrite(p, rd, r);
            }
            FmaddS | FmaddD | FmsubS | FmsubD | FnmsubS | FnmsubD | FnmaddS | FnmaddD => {
                let rm = self.rounding(insn)?;
                let (mut a, b, mut c) = (self.fread(p, rs1), self.fread(p, rs2), self.fread(p, rs3));
                // Negating an operand is exact, and NaN results are canonical anyway.
                if matches!(insn.op, FnmsubS | FnmsubD | FnmaddS | FnmaddD) {
                    a ^= sign_bit(p);
                }
                if matches!(insn.op, FmsubS | FmsubD | FnmaddS | FnmaddD) {
                    c ^= sign_bit(p);
                }
                let r = sf!(p, mul_add(a, b, c, rm, &mut fl));
                self.fwrite(p, rd, r);
            }
            FsgnjS | FsgnjD | FsgnjnS | FsgnjnD | FsgnjxS | FsgnjxD => {
                let (a, b) = (self.fread(p, rs1), self.fread(p, rs2));
                let s = sign_bit(p);
                let sign = match insn.op {
                    FsgnjS | FsgnjD => b & s,
                    FsgnjnS | FsgnjnD => !b & s,
                    _ => (a ^ b) & s,
                };
                self.fwrite(p, rd, (a & !s) | sign);
            }
            FminS | FminD | FmaxS | FmaxD | FminmS | FminmD | FmaxmS | FmaxmD => {
                let (a, b) = (self.fread(p, rs1), self.fread(p, rs2));
                let max = matches!(insn.op, FmaxS | FmaxD | FmaxmS | FmaxmD);
                let nan_propagating = matches!(insn.op, FminmS | FminmD | FmaxmS | FmaxmD);
                let r = sf!(p, min_max(a, b, max, nan_propagating, &mut fl));
                self.fwrite(p, rd, r);
            }
            FeqS | FeqD | FltS | FltD | FleS | FleD | FltqS | FltqD | FleqS | FleqD => {
                let (a, b) = (self.fread(p, rs1), self.fread(p, rs2));
                let r = match insn.op {
                    FeqS | FeqD => sf!(p, eq(a, b, &mut fl)),
                    FltS | FltD => sf!(p, lt(a, b, false, &mut fl)),
                    FleS | FleD => sf!(p, le(a, b, false, &mut fl)),
                    FltqS | FltqD => sf!(p, lt(a, b, true, &mut fl)),
                    _ => sf!(p, le(a, b, true, &mut fl)),
                };
                self.set_x(rd, r as u64);
                self.accrue(fl);
                if fl != 0 {
                    self.fp_dirty();
                }
                return Ok(());
            }
            FclassS | FclassD => {
                let v = sf!(p, classify(self.fread(p, rs1)));
                self.set_x(rd, v);
                return Ok(());
            }
            FliS | FliD => {
                let v = sf!(p, fli(rs1 as u32));
                self.fwrite(p, rd, v);
            }
            FroundS | FroundD | FroundnxS | FroundnxD => {
                let rm = self.rounding(insn)?;
                let exact = matches!(insn.op, FroundnxS | FroundnxD);
                let r = sf!(p, round_to_int(self.fread(p, rs1), rm, exact, &mut fl));
                self.fwrite(p, rd, r);
            }
            FcvtWS | FcvtWD | FcvtWuS | FcvtWuD | FcvtLS | FcvtLD | FcvtLuS | FcvtLuD => {
                let rm = self.rounding(insn)?;
                let (signed, width) = match insn.op {
                    FcvtWS | FcvtWD => (true, 32),
                    FcvtWuS | FcvtWuD => (false, 32),
                    FcvtLS | FcvtLD => (true, 64),
                    _ => (false, 64),
                };
                let v = sf!(p, to_int(self.fread(p, rs1), signed, width, rm, &mut fl));
                self.set_x(rd, v);
                self.accrue(fl);
                if fl != 0 {
                    self.fp_dirty();
                }
                return Ok(());
            }
            FcvtmodWD => {
                // The encoding fixes the rounding mode to RTZ.
                let v = sf::to_i32_modular(self.fread(P::D, rs1), &mut fl);
                self.set_x(rd, v);
                self.accrue(fl);
                if fl != 0 {
                    self.fp_dirty();
                }
                return Ok(());
            }
            FcvtSW | FcvtDW | FcvtSWu | FcvtDWu | FcvtSL | FcvtDL | FcvtSLu | FcvtDLu => {
                let rm = self.rounding(insn)?;
                let x = self.x(rs1);
                let (value, signed) = match insn.op {
                    FcvtSW | FcvtDW => (x as i32 as i64 as u64, true),
                    FcvtSWu | FcvtDWu => (x as u32 as u64, false),
                    FcvtSL | FcvtDL => (x, true),
                    _ => (x, false),
                };
                let r = sf!(p, from_int(value, signed, rm, &mut fl));
                self.fwrite(p, rd, r);
            }
            FmvXW => {
                let v = self.state.f[rs1 as usize] as u32 as i32 as i64 as u64;
                self.set_x(rd, v);
                return Ok(());
            }
            FmvXD => {
                let v = self.state.f[rs1 as usize];
                self.set_x(rd, v);
                return Ok(());
            }
            FmvXH => {
                let v = self.state.f[rs1 as usize] as u16 as i16 as i64 as u64;
                self.set_x(rd, v);
                return Ok(());
            }
            FmvWX => self.fwrite(P::S, rd, self.x(rs1) & 0xffff_ffff),
            FmvDX => self.fwrite(P::D, rd, self.x(rs1)),
            FmvHX => self.fwrite(P::H, rd, self.x(rs1) & 0xffff),
            FcvtSD | FcvtDS | FcvtSH | FcvtHS | FcvtDH | FcvtHD => {
                let rm = self.rounding(insn)?;
                let (from, to) = match insn.op {
                    FcvtSD => (P::D, P::S),
                    FcvtDS => (P::S, P::D),
                    FcvtSH => (P::H, P::S),
                    FcvtHS => (P::S, P::H),
                    FcvtDH => (P::H, P::D),
                    _ => (P::D, P::H),
                };
                let a = self.fread(from, rs1);
                let r = match (from, to) {
                    (P::D, P::S) => sf::convert::<F64, F32>(a, rm, &mut fl),
                    (P::S, P::D) => sf::convert::<F32, F64>(a, rm, &mut fl),
                    (P::H, P::S) => sf::convert::<F16, F32>(a, rm, &mut fl),
                    (P::S, P::H) => sf::convert::<F32, F16>(a, rm, &mut fl),
                    (P::H, P::D) => sf::convert::<F16, F64>(a, rm, &mut fl),
                    _ => sf::convert::<F64, F16>(a, rm, &mut fl),
                };
                self.fwrite(to, rd, r);
            }
            op => unreachable!("{op:?} is not a floating-point operation"),
        }
        self.accrue(fl);
        self.fp_dirty();
        Ok(())
    }

    /// FLH/FLW/FLD.
    pub(crate) fn fp_load(&mut self, insn: &Insn, addr: u64) -> Result<(), Exception> {
        if !self.fp_enabled() {
            return Err(Exception::illegal(insn.raw));
        }
        let (p, size) = match insn.op {
            Op::Flh => (P::H, 2),
            Op::Flw => (P::S, 4),
            _ => (P::D, 8),
        };
        let v = self.load(addr, size)?;
        self.fwrite(p, insn.rd, v);
        self.fp_dirty();
        Ok(())
    }

    /// FSH/FSW/FSD store the raw register bits, whatever their NaN-boxing.
    pub(crate) fn fp_store(&mut self, insn: &Insn, addr: u64) -> Result<(), Exception> {
        if !self.fp_enabled() {
            return Err(Exception::illegal(insn.raw));
        }
        let size = match insn.op {
            Op::Fsh => 2,
            Op::Fsw => 4,
            _ => 8,
        };
        self.store(addr, size, self.state.f[insn.rs2 as usize])
    }
}
