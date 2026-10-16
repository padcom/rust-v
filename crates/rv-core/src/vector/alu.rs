//! Integer, fixed-point, mask and permutation vector instructions (OPIVV/OPIVX/OPIVI and
//! OPMVV/OPMVX), including Zvbb.

use rv_isa::Insn;

use super::{Vtype, mask, sext};
use crate::hart::Hart;
use crate::trap::Exception;

/// The second operand: vs1, or an already-resolved scalar (rs1 or immediate).
#[derive(Clone, Copy)]
pub(crate) enum Src {
    V(u8),
    X(u64),
}

/// Per-instruction context.
#[derive(Clone, Copy)]
pub(crate) struct Ctx {
    pub vt: Vtype,
    pub vl: usize,
    pub start: usize,
    pub vm: bool,
    pub vd: u8,
    pub vs2: u8,
    pub src: Src,
    pub raw: u32,
}

/// Fixed-point rounding (`vxrm`) of `v >> d`, for unsigned values.
fn round_u(v: u128, d: u32, vxrm: u64) -> u128 {
    if d == 0 {
        return v;
    }
    let bit = |n: u32| (v >> n) & 1;
    let low_nonzero = d >= 2 && v & ((1u128 << (d - 1)) - 1) != 0;
    let r = match vxrm {
        0 => bit(d - 1),
        1 => bit(d - 1) & ((low_nonzero || bit(d) == 1) as u128),
        2 => 0,
        _ => ((bit(d) == 0) && (v & ((1u128 << d) - 1) != 0)) as u128,
    };
    (v >> d) + r
}

/// Fixed-point rounding of `v >> d` for signed values.
fn round_s(v: i128, d: u32, vxrm: u64) -> i128 {
    if d == 0 {
        return v;
    }
    let bit = |n: u32| (v >> n) & 1;
    let low_nonzero = d >= 2 && v & ((1i128 << (d - 1)) - 1) != 0;
    let r = match vxrm {
        0 => bit(d - 1),
        1 => bit(d - 1) & ((low_nonzero || bit(d) == 1) as i128),
        2 => 0,
        _ => ((bit(d) == 0) && (v & ((1i128 << d) - 1) != 0)) as i128,
    };
    (v >> d) + r
}

fn smin(sew: usize) -> i128 {
    -(1i128 << (8 * sew - 1))
}
fn smax(sew: usize) -> i128 {
    (1i128 << (8 * sew - 1)) - 1
}

impl Hart {
    #[inline(always)]
    fn op1(&self, src: Src, i: usize, sew: usize) -> u64 {
        match src {
            Src::V(r) => self.velem(r, i, sew),
            Src::X(x) => x & mask(sew),
        }
    }

    /// Checks the common register-group constraints of a SEW-wide operation.
    fn check_sew_op(&self, c: &Ctx, dest_is_mask: bool) -> Result<(), Exception> {
        if !dest_is_mask {
            self.vcheck_group(c.vd, c.vt.lmul, c.raw)?;
            if !c.vm && c.vd == 0 {
                return Err(Exception::illegal(c.raw));
            }
        }
        self.vcheck_group(c.vs2, c.vt.lmul, c.raw)?;
        if let Src::V(r) = c.src {
            self.vcheck_group(r, c.vt.lmul, c.raw)?;
        }
        Ok(())
    }

    /// `vd[i] = f(vs2[i], op1[i], vd[i])` over active body elements.
    fn vbin(&mut self, c: &Ctx, f: impl Fn(u64, u64, u64) -> u64) -> Result<(), Exception> {
        self.check_sew_op(c, false)?;
        let sew = c.vt.sew;
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                let (a, b, d) =
                    (self.velem(c.vs2, i, sew), self.op1(c.src, i, sew), self.velem(c.vd, i, sew));
                self.set_velem(c.vd, i, sew, f(a, b, d) & mask(sew));
            }
        }
        Ok(())
    }

    /// Like [`vbin`](Self::vbin) for fixed-point operations that may saturate.
    fn vfixed(&mut self, c: &Ctx, f: impl Fn(u64, u64, u64, &mut bool) -> u64) -> Result<(), Exception> {
        self.check_sew_op(c, false)?;
        let (sew, vxrm) = (c.vt.sew, self.state.csr.vxrm);
        let mut sat = false;
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                let (a, b) = (self.velem(c.vs2, i, sew), self.op1(c.src, i, sew));
                self.set_velem(c.vd, i, sew, f(a, b, vxrm, &mut sat) & mask(sew));
            }
        }
        if sat {
            self.state.csr.vxsat = 1;
        }
        Ok(())
    }

    /// `vd.mask[i] = f(vs2[i], op1[i])` over active body elements.
    fn vcmp(&mut self, c: &Ctx, f: impl Fn(u64, u64) -> bool) -> Result<(), Exception> {
        self.check_sew_op(c, true)?;
        let sew = c.vt.sew;
        let mut bits = Vec::with_capacity(c.vl);
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                bits.push((i, f(self.velem(c.vs2, i, sew), self.op1(c.src, i, sew))));
            }
        }
        // Written afterwards so that vd may overlap the sources.
        for (i, b) in bits {
            self.set_vbit(c.vd, i, b);
        }
        Ok(())
    }

    /// Widening: `vd(2*SEW)[i] = f(vs2[i], op1[i], vd[i])`. With `wide_vs2`, vs2 is 2*SEW too.
    fn vwiden(&mut self, c: &Ctx, wide_vs2: bool, f: impl Fn(u64, u64, u64) -> u64) -> Result<(), Exception> {
        let sew = c.vt.sew;
        if sew == 8 || c.vt.lmul == 3 {
            return Err(Exception::illegal(c.raw));
        }
        self.vcheck_group(c.vd, c.vt.lmul + 1, c.raw)?;
        self.vcheck_group(c.vs2, c.vt.lmul + wide_vs2 as i32, c.raw)?;
        if let Src::V(r) = c.src {
            self.vcheck_group(r, c.vt.lmul, c.raw)?;
        }
        if !c.vm && c.vd == 0 {
            return Err(Exception::illegal(c.raw));
        }
        let s2 = if wide_vs2 { 2 * sew } else { sew };
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                let (a, b, d) =
                    (self.velem(c.vs2, i, s2), self.op1(c.src, i, sew), self.velem(c.vd, i, 2 * sew));
                self.set_velem(c.vd, i, 2 * sew, f(a, b, d) & mask(2 * sew));
            }
        }
        Ok(())
    }

    /// Narrowing: `vd(SEW)[i] = f(vs2(2*SEW)[i], op1[i])`.
    fn vnarrow(&mut self, c: &Ctx, f: impl Fn(u64, u64, u64, &mut bool) -> u64) -> Result<(), Exception> {
        let sew = c.vt.sew;
        if sew == 8 || c.vt.lmul == 3 {
            return Err(Exception::illegal(c.raw));
        }
        self.vcheck_group(c.vd, c.vt.lmul, c.raw)?;
        self.vcheck_group(c.vs2, c.vt.lmul + 1, c.raw)?;
        if let Src::V(r) = c.src {
            self.vcheck_group(r, c.vt.lmul, c.raw)?;
        }
        if !c.vm && c.vd == 0 {
            return Err(Exception::illegal(c.raw));
        }
        let vxrm = self.state.csr.vxrm;
        let mut sat = false;
        let mut out = Vec::with_capacity(c.vl);
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                let (a, b) = (self.velem(c.vs2, i, 2 * sew), self.op1(c.src, i, sew));
                out.push((i, f(a, b, vxrm, &mut sat) & mask(sew)));
            }
        }
        for (i, v) in out {
            self.set_velem(c.vd, i, sew, v);
        }
        if sat {
            self.state.csr.vxsat = 1;
        }
        Ok(())
    }

    pub(crate) fn exec_opv(&mut self, insn: &Insn) -> Result<(), Exception> {
        let raw = insn.raw;
        let f3 = (raw >> 12) & 7;
        if f3 == 1 || f3 == 5 {
            return self.exec_opf(insn);
        }
        let funct6 = raw >> 26;
        let vm = (raw >> 25) & 1 == 1;

        // Whole-register moves do not depend on vtype.
        if f3 == 3 && funct6 == 0b100111 {
            return self.vmv_whole(insn);
        }

        let vt = self.vtype(raw)?;
        let sew = vt.sew;
        let bits = 8 * sew as u32;
        let simm = ((insn.rs1 as i8) << 3 >> 3) as i64 as u64;
        let src = match f3 {
            0 | 2 => Src::V(insn.rs1),
            3 => Src::X(simm),
            _ => Src::X(self.x(insn.rs1)),
        };
        let c = Ctx {
            vt,
            vl: self.state.csr.vl as usize,
            start: self.state.csr.vstart as usize,
            vm,
            vd: insn.rd,
            vs2: insn.rs2,
            src,
            raw,
        };
        // Shifts, rotates, gathers and slides take an unsigned immediate.
        let cu = Ctx { src: if f3 == 3 { Src::X(insn.rs1 as u64) } else { src }, ..c };
        let shamt = move |b: u64| (b as u32) & (bits - 1);
        let s = |v: u64| sext(v, sew);
        let illegal = || Err(Exception::illegal(raw));

        if f3 == 2 || f3 == 6 {
            return self.exec_opm(insn, c);
        }

        match funct6 {
            0b000000 => self.vbin(&c, |a, b, _| a.wrapping_add(b)),
            0b000001 if f3 != 3 => self.vbin(&c, |a, b, _| a & !b), // vandn (Zvbb)
            0b000010 if f3 != 3 => self.vbin(&c, |a, b, _| a.wrapping_sub(b)),
            0b000011 if f3 != 0 => self.vbin(&c, |a, b, _| b.wrapping_sub(a)),
            0b000100 if f3 != 3 => self.vbin(&c, |a, b, _| a.min(b)),
            0b000101 if f3 != 3 => self.vbin(&c, |a, b, _| s(a).min(s(b)) as u64),
            0b000110 if f3 != 3 => self.vbin(&c, |a, b, _| a.max(b)),
            0b000111 if f3 != 3 => self.vbin(&c, |a, b, _| s(a).max(s(b)) as u64),
            0b001001 => self.vbin(&c, |a, b, _| a & b),
            0b001010 => self.vbin(&c, |a, b, _| a | b),
            0b001011 => self.vbin(&c, |a, b, _| a ^ b),
            0b001100 => self.vrgather(&cu, sew),
            0b001110 if f3 == 0 => self.vrgather(&c, 2), // vrgatherei16
            0b001110 => self.vslideup(&cu),
            0b001111 if f3 != 0 => self.vslidedown(&cu),
            0b010000..=0b010011 => self.vcarry(&c, funct6),
            // vror.vi encodes imm[5] in funct6[0].
            0b010100 | 0b010101 if f3 == 3 => {
                let n = insn.rs1 as u64 | ((funct6 as u64 & 1) << 5);
                self.vbin(&Ctx { src: Src::X(n), ..c }, |a, b, _| rotr(a, shamt(b), bits))
            }
            0b010100 => self.vbin(&c, |a, b, _| rotr(a, shamt(b), bits)),
            0b010101 => self.vbin(&c, |a, b, _| rotr(a, bits - shamt(b), bits)),
            0b010111 => self.vmerge(&c),
            0b011000 => self.vcmp(&c, |a, b| a == b),
            0b011001 => self.vcmp(&c, |a, b| a != b),
            0b011010 if f3 != 3 => self.vcmp(&c, |a, b| a < b),
            0b011011 if f3 != 3 => self.vcmp(&c, |a, b| s(a) < s(b)),
            0b011100 => self.vcmp(&c, |a, b| a <= b),
            0b011101 => self.vcmp(&c, |a, b| s(a) <= s(b)),
            0b011110 if f3 != 0 => self.vcmp(&c, |a, b| a > b),
            0b011111 if f3 != 0 => self.vcmp(&c, |a, b| s(a) > s(b)),
            0b100000 => self.vfixed(&c, |a, b, _, sat| {
                let r = a as u128 + b as u128;
                if r > mask(sew) as u128 {
                    *sat = true;
                    mask(sew)
                } else {
                    r as u64
                }
            }),
            0b100001 => self.vfixed(&c, |a, b, _, sat| saturate(s(a) as i128 + s(b) as i128, sew, sat)),
            0b100010 if f3 != 3 => self.vfixed(&c, |a, b, _, sat| {
                if a < b {
                    *sat = true;
                    0
                } else {
                    a - b
                }
            }),
            0b100011 if f3 != 3 => {
                self.vfixed(&c, |a, b, _, sat| saturate(s(a) as i128 - s(b) as i128, sew, sat))
            }
            0b100101 => self.vbin(&cu, |a, b, _| a << shamt(b)),
            0b100111 if f3 != 3 => self.vfixed(&c, |a, b, rm, sat| {
                let p = s(a) as i128 * s(b) as i128;
                saturate(round_s(p, bits - 1, rm), sew, sat)
            }),
            0b101000 => self.vbin(&cu, |a, b, _| a >> shamt(b)),
            0b101001 => self.vbin(&cu, |a, b, _| (s(a) >> shamt(b)) as u64),
            0b101010 => self.vfixed(&cu, |a, b, rm, _| round_u(a as u128, shamt(b), rm) as u64),
            0b101011 => self.vfixed(&cu, |a, b, rm, _| round_s(s(a) as i128, shamt(b), rm) as u64),
            0b101100..=0b101111 => {
                let wshamt = move |b: u64| (b as u32) & (2 * bits - 1);
                let w = |v: u64| sext(v, 2 * sew);
                match funct6 {
                    0b101100 => self.vnarrow(&cu, |a, b, _, _| a >> wshamt(b)),
                    0b101101 => self.vnarrow(&cu, |a, b, _, _| (w(a) >> wshamt(b)) as u64),
                    0b101110 => self.vnarrow(&cu, |a, b, rm, sat| {
                        let r = round_u(a as u128, wshamt(b), rm);
                        if r > mask(sew) as u128 {
                            *sat = true;
                            mask(sew)
                        } else {
                            r as u64
                        }
                    }),
                    _ => self.vnarrow(&cu, |a, b, rm, sat| {
                        saturate(round_s(w(a) as i128, wshamt(b), rm), sew, sat)
                    }),
                }
            }
            0b110000 | 0b110001 if f3 == 0 => self.vwred(&c, funct6 == 0b110001),
            // vwsll (Zvbb)
            0b110101 => self.vwiden(&cu, false, |a, b, _| a << ((b as u32) & (2 * bits - 1))),
            _ => illegal(),
        }
    }

    fn exec_opm(&mut self, insn: &Insn, c: Ctx) -> Result<(), Exception> {
        let raw = insn.raw;
        let f3 = (raw >> 12) & 7;
        let funct6 = raw >> 26;
        let sew = c.vt.sew;
        let bits = 8 * sew as u32;
        let s = |v: u64| sext(v, sew);
        let illegal = || Err(Exception::illegal(raw));
        let vv = f3 == 2;

        match funct6 {
            0b000000..=0b000111 if vv => self.vred(&c, funct6),
            0b001000 => self.vfixed(&c, |a, b, rm, _| round_u(a as u128 + b as u128, 1, rm) as u64),
            0b001001 => self.vfixed(&c, |a, b, rm, _| round_s(s(a) as i128 + s(b) as i128, 1, rm) as u64),
            0b001010 => self.vfixed(&c, |a, b, rm, _| round_s(a as i128 - b as i128, 1, rm) as u64),
            0b001011 => self.vfixed(&c, |a, b, rm, _| round_s(s(a) as i128 - s(b) as i128, 1, rm) as u64),
            0b001110 if !vv => self.vslide1up(&c),
            0b001111 if !vv => self.vslide1down(&c),
            0b010000 if vv => match insn.rs1 {
                0b00000 => {
                    // vmv.x.s
                    let v = s(self.velem(c.vs2, 0, sew)) as u64;
                    self.set_x(insn.rd, v);
                    Ok(())
                }
                0b10000 | 0b10001 => self.vmask_scan(&c, insn.rd, insn.rs1 == 0b10001),
                _ => illegal(),
            },
            0b010000 if insn.rs2 == 0 => {
                // vmv.s.x
                if c.start < c.vl {
                    self.set_velem(c.vd, 0, sew, self.x(insn.rs1) & mask(sew));
                }
                Ok(())
            }
            0b010010 if vv => self.vxunary(&c, insn.rs1),
            0b010100 if vv => self.vmunary(&c, insn.rs1),
            0b010111 if vv => self.vcompress(&c),
            0b011000..=0b011111 if vv => self.vmask_logic(&c, funct6),
            0b100000 => self.vbin(&c, |a, b, _| a.checked_div(b).unwrap_or(u64::MAX)),
            0b100001 => self.vbin(&c, |a, b, _| {
                if b == 0 {
                    u64::MAX
                } else if s(a) as i128 == smin(sew) && s(b) == -1 {
                    a
                } else {
                    (s(a) / s(b)) as u64
                }
            }),
            0b100010 => self.vbin(&c, |a, b, _| if b == 0 { a } else { a % b }),
            0b100011 => self.vbin(&c, |a, b, _| {
                if b == 0 {
                    a
                } else if s(a) as i128 == smin(sew) && s(b) == -1 {
                    0
                } else {
                    (s(a) % s(b)) as u64
                }
            }),
            0b100100 => self.vbin(&c, |a, b, _| ((a as u128 * b as u128) >> bits) as u64),
            0b100101 => self.vbin(&c, |a, b, _| a.wrapping_mul(b)),
            0b100110 => self.vbin(&c, |a, b, _| ((s(a) as i128 * b as i128) >> bits) as u64),
            0b100111 => self.vbin(&c, |a, b, _| ((s(a) as i128 * s(b) as i128) >> bits) as u64),
            // Multiply-add: vs2 is `a`, vs1/rs1 is `b`, the old destination is `d`.
            0b101001 => self.vbin(&c, |a, b, d| b.wrapping_mul(d).wrapping_add(a)), // vmadd
            0b101011 => self.vbin(&c, |a, b, d| a.wrapping_sub(b.wrapping_mul(d))), // vnmsub
            0b101101 => self.vbin(&c, |a, b, d| b.wrapping_mul(a).wrapping_add(d)), // vmacc
            0b101111 => self.vbin(&c, |a, b, d| d.wrapping_sub(b.wrapping_mul(a))), // vnmsac
            0b110000..=0b111111 => {
                let w = |v: u64| sext(v, 2 * sew) as u64;
                let z = |v: u64| v;
                match funct6 {
                    0b110000 => self.vwiden(&c, false, |a, b, _| z(a).wrapping_add(z(b))),
                    0b110001 => self.vwiden(&c, false, |a, b, _| (s(a) as u64).wrapping_add(s(b) as u64)),
                    0b110010 => self.vwiden(&c, false, |a, b, _| z(a).wrapping_sub(z(b))),
                    0b110011 => self.vwiden(&c, false, |a, b, _| (s(a) as u64).wrapping_sub(s(b) as u64)),
                    0b110100 => self.vwiden(&c, true, |a, b, _| a.wrapping_add(z(b))),
                    0b110101 => self.vwiden(&c, true, |a, b, _| w(a).wrapping_add(s(b) as u64)),
                    0b110110 => self.vwiden(&c, true, |a, b, _| a.wrapping_sub(z(b))),
                    0b110111 => self.vwiden(&c, true, |a, b, _| w(a).wrapping_sub(s(b) as u64)),
                    0b111000 => self.vwiden(&c, false, |a, b, _| a.wrapping_mul(b)),
                    0b111010 => self.vwiden(&c, false, |a, b, _| (s(a) as u64).wrapping_mul(b)),
                    0b111011 => self.vwiden(&c, false, |a, b, _| (s(a) as u64).wrapping_mul(s(b) as u64)),
                    0b111100 => self.vwiden(&c, false, |a, b, d| d.wrapping_add(a.wrapping_mul(b))),
                    0b111101 => self
                        .vwiden(&c, false, |a, b, d| d.wrapping_add((s(a) as u64).wrapping_mul(s(b) as u64))),
                    0b111110 if !vv => {
                        self.vwiden(&c, false, |a, b, d| d.wrapping_add((s(a) as u64).wrapping_mul(b)))
                    }
                    0b111111 => self.vwiden(&c, false, |a, b, d| d.wrapping_add(a.wrapping_mul(s(b) as u64))),
                    _ => illegal(),
                }
            }
            _ => illegal(),
        }
    }

    fn vmerge(&mut self, c: &Ctx) -> Result<(), Exception> {
        self.vcheck_group(c.vd, c.vt.lmul, c.raw)?;
        if let Src::V(r) = c.src {
            self.vcheck_group(r, c.vt.lmul, c.raw)?;
        }
        let sew = c.vt.sew;
        if c.vm {
            // vmv.v.{v,x,i}
            if c.vs2 != 0 {
                return Err(Exception::illegal(c.raw));
            }
            for i in c.start..c.vl {
                let v = self.op1(c.src, i, sew);
                self.set_velem(c.vd, i, sew, v);
            }
        } else {
            if c.vd == 0 {
                return Err(Exception::illegal(c.raw));
            }
            self.vcheck_group(c.vs2, c.vt.lmul, c.raw)?;
            for i in c.start..c.vl {
                let v = if self.vbit(0, i) { self.op1(c.src, i, sew) } else { self.velem(c.vs2, i, sew) };
                self.set_velem(c.vd, i, sew, v);
            }
        }
        Ok(())
    }

    /// vadc, vmadc, vsbc, vmsbc.
    fn vcarry(&mut self, c: &Ctx, funct6: u32) -> Result<(), Exception> {
        let sew = c.vt.sew;
        let sub = funct6 & 2 != 0;
        let to_mask = funct6 & 1 != 0;
        // vadc/vsbc must use v0 as carry and may not write it.
        if !to_mask && (c.vm || c.vd == 0) {
            return Err(Exception::illegal(c.raw));
        }
        if sub && matches!(c.src, Src::X(_)) && (c.raw >> 12) & 7 == 3 {
            return Err(Exception::illegal(c.raw));
        }
        self.check_sew_op(&Ctx { vm: true, ..*c }, to_mask)?;
        let mut out = Vec::with_capacity(c.vl);
        for i in c.start..c.vl {
            let carry = (!c.vm && self.vbit(0, i)) as u128;
            let (a, b) = (self.velem(c.vs2, i, sew) as u128, self.op1(c.src, i, sew) as u128);
            let wide = if sub { a.wrapping_sub(b).wrapping_sub(carry) } else { a + b + carry };
            out.push((i, wide));
        }
        for (i, wide) in out {
            if to_mask {
                // Carry out of / borrow into the top bit.
                let flag = (wide >> (8 * sew)) & 1 != 0;
                self.set_vbit(c.vd, i, flag);
            } else {
                self.set_velem(c.vd, i, sew, wide as u64 & mask(sew));
            }
        }
        Ok(())
    }

    fn vred(&mut self, c: &Ctx, funct6: u32) -> Result<(), Exception> {
        if c.start != 0 {
            return Err(Exception::illegal(c.raw));
        }
        self.vcheck_group(c.vs2, c.vt.lmul, c.raw)?;
        let sew = c.vt.sew;
        let Src::V(vs1) = c.src else { unreachable!() };
        if c.vl == 0 {
            return Ok(());
        }
        let s = |v: u64| sext(v, sew);
        let mut acc = self.velem(vs1, 0, sew);
        for i in 0..c.vl {
            if !self.vactive(c.vm, i) {
                continue;
            }
            let v = self.velem(c.vs2, i, sew);
            acc = match funct6 {
                0 => acc.wrapping_add(v),
                1 => acc & v,
                2 => acc | v,
                3 => acc ^ v,
                4 => acc.min(v),
                5 => s(acc).min(s(v)) as u64,
                6 => acc.max(v),
                _ => s(acc).max(s(v)) as u64,
            } & mask(sew);
        }
        self.set_velem(c.vd, 0, sew, acc);
        Ok(())
    }

    fn vwred(&mut self, c: &Ctx, signed: bool) -> Result<(), Exception> {
        let sew = c.vt.sew;
        if c.start != 0 || sew == 8 {
            return Err(Exception::illegal(c.raw));
        }
        self.vcheck_group(c.vs2, c.vt.lmul, c.raw)?;
        let Src::V(vs1) = c.src else { unreachable!() };
        if c.vl == 0 {
            return Ok(());
        }
        let mut acc = self.velem(vs1, 0, 2 * sew);
        for i in 0..c.vl {
            if self.vactive(c.vm, i) {
                let v = self.velem(c.vs2, i, sew);
                let v = if signed { sext(v, sew) as u64 } else { v };
                acc = acc.wrapping_add(v) & mask(2 * sew);
            }
        }
        self.set_velem(c.vd, 0, 2 * sew, acc);
        Ok(())
    }

    fn vrgather(&mut self, c: &Ctx, index_eew: usize) -> Result<(), Exception> {
        let sew = c.vt.sew;
        let vlmax = self.vlmax(sew, c.vt.lmul);
        self.vcheck_group(c.vd, c.vt.lmul, c.raw)?;
        self.vcheck_group(c.vs2, c.vt.lmul, c.raw)?;
        if let Src::V(r) = c.src {
            let emul = c.vt.lmul + index_eew.trailing_zeros() as i32 - sew.trailing_zeros() as i32;
            self.vcheck_group(r, emul, c.raw)?;
            if r == c.vd {
                return Err(Exception::illegal(c.raw));
            }
        }
        if c.vd == c.vs2 || (!c.vm && c.vd == 0) {
            return Err(Exception::illegal(c.raw));
        }
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                let idx = match c.src {
                    Src::V(r) => self.velem(r, i, index_eew),
                    Src::X(x) => x,
                };
                let v = if idx < vlmax as u64 { self.velem(c.vs2, idx as usize, sew) } else { 0 };
                self.set_velem(c.vd, i, sew, v);
            }
        }
        Ok(())
    }

    fn vslideup(&mut self, c: &Ctx) -> Result<(), Exception> {
        self.check_sew_op(c, false)?;
        if c.vd == c.vs2 {
            return Err(Exception::illegal(c.raw));
        }
        let Src::X(offset) = c.src else { unreachable!() };
        let sew = c.vt.sew;
        let from = (offset.min(c.vl as u64) as usize).max(c.start);
        for i in from..c.vl {
            if self.vactive(c.vm, i) {
                let v = self.velem(c.vs2, i - offset as usize, sew);
                self.set_velem(c.vd, i, sew, v);
            }
        }
        Ok(())
    }

    fn vslidedown(&mut self, c: &Ctx) -> Result<(), Exception> {
        self.check_sew_op(c, false)?;
        let Src::X(offset) = c.src else { unreachable!() };
        let sew = c.vt.sew;
        let vlmax = self.vlmax(sew, c.vt.lmul) as u64;
        let mut out = Vec::with_capacity(c.vl);
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                let src = (i as u64).checked_add(offset).filter(|&s| s < vlmax);
                out.push((i, src.map_or(0, |s| self.velem(c.vs2, s as usize, sew))));
            }
        }
        for (i, v) in out {
            self.set_velem(c.vd, i, sew, v);
        }
        Ok(())
    }

    pub(crate) fn vslide1up_value(&mut self, c: &Ctx, x: u64) -> Result<(), Exception> {
        self.check_sew_op(&Ctx { src: Src::X(0), ..*c }, false)?;
        if c.vd == c.vs2 {
            return Err(Exception::illegal(c.raw));
        }
        let sew = c.vt.sew;
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                let v = if i == 0 { x & mask(sew) } else { self.velem(c.vs2, i - 1, sew) };
                self.set_velem(c.vd, i, sew, v);
            }
        }
        Ok(())
    }

    pub(crate) fn vslide1down_value(&mut self, c: &Ctx, x: u64) -> Result<(), Exception> {
        self.check_sew_op(&Ctx { src: Src::X(0), ..*c }, false)?;
        let sew = c.vt.sew;
        let mut out = Vec::with_capacity(c.vl);
        for i in c.start..c.vl {
            if self.vactive(c.vm, i) {
                out.push((i, if i + 1 < c.vl { self.velem(c.vs2, i + 1, sew) } else { x & mask(sew) }));
            }
        }
        for (i, v) in out {
            self.set_velem(c.vd, i, sew, v);
        }
        Ok(())
    }

    fn vslide1up(&mut self, c: &Ctx) -> Result<(), Exception> {
        let Src::X(x) = c.src else { unreachable!() };
        self.vslide1up_value(c, x)
    }

    fn vslide1down(&mut self, c: &Ctx) -> Result<(), Exception> {
        let Src::X(x) = c.src else { unreachable!() };
        self.vslide1down_value(c, x)
    }

    fn vcompress(&mut self, c: &Ctx) -> Result<(), Exception> {
        let Src::V(vs1) = c.src else { unreachable!() };
        if !c.vm || c.start != 0 || c.vd == c.vs2 || c.vd == vs1 {
            return Err(Exception::illegal(c.raw));
        }
        self.vcheck_group(c.vd, c.vt.lmul, c.raw)?;
        self.vcheck_group(c.vs2, c.vt.lmul, c.raw)?;
        let sew = c.vt.sew;
        let mut k = 0;
        for i in 0..c.vl {
            if self.vbit(vs1, i) {
                let v = self.velem(c.vs2, i, sew);
                self.set_velem(c.vd, k, sew, v);
                k += 1;
            }
        }
        Ok(())
    }

    /// vmv<nr>r.v
    fn vmv_whole(&mut self, insn: &Insn) -> Result<(), Exception> {
        let nr = insn.rs1 as usize + 1;
        if !matches!(nr, 1 | 2 | 4 | 8)
            || !(insn.rd as usize).is_multiple_of(nr)
            || !(insn.rs2 as usize).is_multiple_of(nr)
        {
            return Err(Exception::illegal(insn.raw));
        }
        let vlenb = self.vlenb();
        let sew = self.vtype(insn.raw).map_or(1, |vt| vt.sew);
        let start = self.state.csr.vstart as usize * sew;
        let (dst, src) = (insn.rd as usize * vlenb, insn.rs2 as usize * vlenb);
        self.state.v.copy_within(src + start..src + nr * vlenb, dst + start);
        Ok(())
    }

    /// vcpop.m / vfirst.m
    fn vmask_scan(&mut self, c: &Ctx, rd: u8, first: bool) -> Result<(), Exception> {
        if c.start != 0 {
            return Err(Exception::illegal(c.raw));
        }
        let set = (0..c.vl).filter(|&i| self.vactive(c.vm, i) && self.vbit(c.vs2, i));
        let v = if first { set.min().map_or(u64::MAX, |i| i as u64) } else { set.count() as u64 };
        self.set_x(rd, v);
        Ok(())
    }

    fn vmask_logic(&mut self, c: &Ctx, funct6: u32) -> Result<(), Exception> {
        let Src::V(vs1) = c.src else { unreachable!() };
        if !c.vm {
            return Err(Exception::illegal(c.raw));
        }
        let mut out = Vec::with_capacity(c.vl);
        for i in c.start..c.vl {
            let (a, b) = (self.vbit(c.vs2, i), self.vbit(vs1, i));
            out.push(match funct6 & 7 {
                0 => a & !b,
                1 => a & b,
                2 => a | b,
                3 => a ^ b,
                4 => a | !b,
                5 => !(a & b),
                6 => !(a | b),
                _ => !(a ^ b),
            });
        }
        for (k, bit) in out.into_iter().enumerate() {
            self.set_vbit(c.vd, c.start + k, bit);
        }
        Ok(())
    }

    /// VXUNARY0: integer extension and the Zvbb unary operations.
    fn vxunary(&mut self, c: &Ctx, sel: u8) -> Result<(), Exception> {
        let sew = c.vt.sew;
        let bits = 8 * sew as u32;
        let (frac, signed): (usize, bool) = match sel {
            0b00010 => (8, false),
            0b00011 => (8, true),
            0b00100 => (4, false),
            0b00101 => (4, true),
            0b00110 => (2, false),
            0b00111 => (2, true),
            _ => (0, false),
        };
        if frac != 0 {
            // vzext/vsext.vf{2,4,8}
            let src_emul = c.vt.lmul - frac.trailing_zeros() as i32;
            if sew < frac || src_emul < -3 {
                return Err(Exception::illegal(c.raw));
            }
            let src_eew = sew / frac;
            self.vcheck_group(c.vd, c.vt.lmul, c.raw)?;
            self.vcheck_group(c.vs2, src_emul, c.raw)?;
            if !c.vm && c.vd == 0 {
                return Err(Exception::illegal(c.raw));
            }
            let mut out = Vec::with_capacity(c.vl);
            for i in c.start..c.vl {
                if self.vactive(c.vm, i) {
                    let v = self.velem(c.vs2, i, src_eew);
                    out.push((i, if signed { sext(v, src_eew) as u64 } else { v }));
                }
            }
            for (i, v) in out {
                self.set_velem(c.vd, i, sew, v & mask(sew));
            }
            return Ok(());
        }
        let f: fn(u64, u32) -> u64 = match sel {
            0b01000 => |v, bits| {
                // vbrev8: reverse the bits within each byte.
                let r = v.reverse_bits().swap_bytes();
                r & mask(bits as usize / 8)
            },
            0b01001 => |v, bits| v.swap_bytes() >> (64 - bits),
            0b01010 => |v, bits| v.reverse_bits() >> (64 - bits),
            0b01100 => |v, bits| (v.leading_zeros() - (64 - bits)) as u64,
            0b01101 => |v, bits| (v.trailing_zeros().min(bits)) as u64,
            0b01110 => |v, _| v.count_ones() as u64,
            _ => return Err(Exception::illegal(c.raw)),
        };
        self.vbin(&Ctx { src: Src::X(0), ..*c }, |a, _, _| f(a, bits))
    }

    /// VMUNARY0: vmsbf, vmsof, vmsif, viota, vid.
    fn vmunary(&mut self, c: &Ctx, sel: u8) -> Result<(), Exception> {
        let sew = c.vt.sew;
        match sel {
            0b00001..=0b00011 => {
                if c.start != 0 || c.vd == c.vs2 || (!c.vm && c.vd == 0) {
                    return Err(Exception::illegal(c.raw));
                }
                let mut seen = false;
                let mut out = Vec::with_capacity(c.vl);
                for i in 0..c.vl {
                    if !self.vactive(c.vm, i) {
                        continue;
                    }
                    let bit = self.vbit(c.vs2, i);
                    let v = match sel {
                        0b00001 => !seen && !bit, // set-before-first
                        0b00010 => !seen && bit,  // set-only-first
                        _ => !seen,               // set-including-first
                    };
                    seen |= bit;
                    out.push((i, v));
                }
                for (i, v) in out {
                    self.set_vbit(c.vd, i, v);
                }
                Ok(())
            }
            0b10000 => {
                if c.start != 0 || c.vd == c.vs2 || (!c.vm && c.vd == 0) {
                    return Err(Exception::illegal(c.raw));
                }
                self.vcheck_group(c.vd, c.vt.lmul, c.raw)?;
                let mut count = 0u64;
                for i in 0..c.vl {
                    if self.vactive(c.vm, i) {
                        let bit = self.vbit(c.vs2, i);
                        self.set_velem(c.vd, i, sew, count & mask(sew));
                        count += bit as u64;
                    }
                }
                Ok(())
            }
            0b10001 if c.vs2 == 0 => {
                self.vcheck_group(c.vd, c.vt.lmul, c.raw)?;
                if !c.vm && c.vd == 0 {
                    return Err(Exception::illegal(c.raw));
                }
                for i in c.start..c.vl {
                    if self.vactive(c.vm, i) {
                        self.set_velem(c.vd, i, sew, i as u64 & mask(sew));
                    }
                }
                Ok(())
            }
            _ => Err(Exception::illegal(c.raw)),
        }
    }
}

fn rotr(v: u64, n: u32, bits: u32) -> u64 {
    let n = n % bits;
    if n == 0 { v } else { ((v >> n) | (v << (bits - n))) & mask(bits as usize / 8) }
}

fn saturate(v: i128, sew: usize, sat: &mut bool) -> u64 {
    if v > smax(sew) {
        *sat = true;
        smax(sew) as u64
    } else if v < smin(sew) {
        *sat = true;
        smin(sew) as u64
    } else {
        v as u64
    }
}
