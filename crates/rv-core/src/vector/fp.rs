//! Vector floating point (OPFVV/OPFVF), with Zvfh (binary16 elements) and Zvfhmin.

use rv_isa::Insn;
use rv_softfloat::{self as sf, F16, F32, F64, Rm};

use super::alu::{Ctx, Src};
use super::{mask, sext};
use crate::hart::Hart;
use crate::trap::Exception;

/// Dispatches a softfloat function on an element width in bytes (2, 4 or 8).
macro_rules! sfw {
    ($w:expr, $f:ident($($arg:expr),*)) => {
        match $w {
            2 => sf::$f::<F16>($($arg),*),
            4 => sf::$f::<F32>($($arg),*),
            _ => sf::$f::<F64>($($arg),*),
        }
    };
}

/// Converts a float of width `from` bytes to width `to` bytes.
fn fconv(x: u64, from: usize, to: usize, rm: Rm, fl: &mut u8) -> u64 {
    match (from, to) {
        (2, 4) => sf::convert::<F16, F32>(x, rm, fl),
        (2, 8) => sf::convert::<F16, F64>(x, rm, fl),
        (4, 8) => sf::convert::<F32, F64>(x, rm, fl),
        (4, 2) => sf::convert::<F32, F16>(x, rm, fl),
        (8, 2) => sf::convert::<F64, F16>(x, rm, fl),
        (8, 4) => sf::convert::<F64, F32>(x, rm, fl),
        _ => x,
    }
}

fn sign_bit(w: usize) -> u64 {
    1 << (8 * w - 1)
}

/// The 7-bit reciprocal square-root estimate (`vfrsqrt7`), bit-exact with the specification.
fn rsqrt7(x: u64, w: usize, fl: &mut u8) -> u64 {
    const TABLE: [u8; 128] = [
        52, 51, 50, 48, 47, 46, 44, 43, 42, 41, 40, 39, 38, 36, 35, 34, 33, 32, 31, 30, 30, 29, 28, 27, 26,
        25, 24, 23, 23, 22, 21, 20, 19, 19, 18, 17, 16, 16, 15, 14, 14, 13, 12, 12, 11, 10, 10, 9, 9, 8, 7,
        7, 6, 6, 5, 4, 4, 3, 3, 2, 2, 1, 1, 0, 127, 125, 123, 121, 119, 118, 116, 114, 113, 111, 109, 108,
        106, 105, 103, 102, 100, 99, 97, 96, 95, 93, 92, 91, 90, 88, 87, 86, 85, 84, 83, 82, 80, 79, 78, 77,
        76, 75, 74, 73, 72, 71, 70, 70, 69, 68, 67, 66, 65, 64, 63, 63, 62, 61, 60, 59, 59, 58, 57, 56, 56,
        55, 54, 53,
    ];
    let (e, s) = exp_frac_bits(w);
    let class = sfw!(w, classify(x));
    let inf = ((1u64 << e) - 1) << s;
    match class {
        0x001 | 0x002 | 0x004 | 0x100 => {
            *fl |= sf::flags::NV;
            return sfw!(w, canonical_nan());
        }
        0x200 => return sfw!(w, canonical_nan()),
        0x008 => {
            *fl |= sf::flags::DZ;
            return sign_bit(w) | inf;
        }
        0x010 => {
            *fl |= sf::flags::DZ;
            return inf;
        }
        0x080 => return 0,
        _ => {}
    }
    let mut exp = (x >> s) & ((1 << e) - 1);
    let mut sig = x & ((1 << s) - 1);
    if class == 0x020 {
        // Normalise subnormals; the exponent goes "negative" with wrapping arithmetic.
        while sig & (1 << (s - 1)) == 0 {
            exp = exp.wrapping_sub(1);
            sig <<= 1;
        }
        sig = (sig << 1) & ((1 << s) - 1);
    }
    let idx = (((exp & 1) << 6) | (sig >> (s - 6))) as usize;
    let out_sig = (TABLE[idx] as u64) << (s - 7);
    let out_exp = (3 * ((1u64 << (e - 1)) - 1)).wrapping_add(!exp) / 2;
    ((out_exp & ((1 << e) - 1)) << s) | out_sig
}

/// The 7-bit reciprocal estimate (`vfrec7`), bit-exact with the specification.
fn rec7(x: u64, w: usize, rm: Rm, fl: &mut u8) -> u64 {
    const TABLE: [u8; 128] = [
        127, 125, 123, 121, 119, 117, 116, 114, 112, 110, 109, 107, 105, 104, 102, 100, 99, 97, 96, 94, 93,
        91, 90, 88, 87, 85, 84, 83, 81, 80, 79, 77, 76, 75, 74, 72, 71, 70, 69, 68, 66, 65, 64, 63, 62, 61,
        60, 59, 58, 57, 56, 55, 54, 53, 52, 51, 50, 49, 48, 47, 46, 45, 44, 43, 42, 41, 40, 40, 39, 38, 37,
        36, 35, 35, 34, 33, 32, 31, 31, 30, 29, 28, 28, 27, 26, 25, 25, 24, 23, 23, 22, 21, 21, 20, 19, 19,
        18, 17, 17, 16, 15, 15, 14, 14, 13, 12, 12, 11, 11, 10, 9, 9, 8, 8, 7, 7, 6, 5, 5, 4, 4, 3, 3, 2, 2,
        1, 1, 0,
    ];
    let (e, s) = exp_frac_bits(w);
    let class = sfw!(w, classify(x));
    let exp_mask = (1u64 << e) - 1;
    let inf = exp_mask << s;
    match class {
        0x001 => return sign_bit(w),
        0x080 => return 0,
        0x008 => {
            *fl |= sf::flags::DZ;
            return sign_bit(w) | inf;
        }
        0x010 => {
            *fl |= sf::flags::DZ;
            return inf;
        }
        0x100 => {
            *fl |= sf::flags::NV;
            return sfw!(w, canonical_nan());
        }
        0x200 => return sfw!(w, canonical_nan()),
        _ => {}
    }
    let sign = (x >> (8 * w - 1)) & 1;
    let mut exp = (x >> s) & exp_mask;
    let mut sig = x & ((1 << s) - 1);
    if class == 0x004 || class == 0x020 {
        while sig & (1 << (s - 1)) == 0 {
            exp = exp.wrapping_sub(1);
            sig <<= 1;
        }
        sig = (sig << 1) & ((1 << s) - 1);
        if exp != 0 && exp != u64::MAX {
            // Too small: the reciprocal overflows.
            *fl |= sf::flags::NX | sf::flags::OF;
            let toward_zero = rm == Rm::Rtz || (rm == Rm::Rdn && sign == 0) || (rm == Rm::Rup && sign == 1);
            let r = (sign << (8 * w - 1)) | inf;
            return if toward_zero { r - 1 } else { r };
        }
    }
    let idx = (sig >> (s - 7)) as usize;
    let mut out_sig = (TABLE[idx] as u64) << (s - 7);
    let mut out_exp = (2 * ((1u64 << (e - 1)) - 1)).wrapping_add(!exp);
    if out_exp == 0 || out_exp == u64::MAX {
        out_sig = (out_sig >> 1) | (1 << (s - 1));
        if out_exp == u64::MAX {
            out_sig >>= 1;
            out_exp = 0;
        }
    }
    (sign << (8 * w - 1)) | ((out_exp & exp_mask) << s) | out_sig
}

fn exp_frac_bits(w: usize) -> (u32, u32) {
    match w {
        2 => (5, 10),
        4 => (8, 23),
        _ => (11, 52),
    }
}

impl Hart {
    /// Rounding mode for vector FP: always `frm`.
    fn vrm(&self, raw: u32) -> Result<Rm, Exception> {
        Rm::from_bits((self.state.csr.fcsr >> 5) & 7).ok_or(Exception::illegal(raw))
    }

    fn vaccrue(&mut self, fl: u8) {
        self.state.csr.fcsr |= fl as u64;
        if fl != 0 {
            self.state.csr.mstatus |= crate::csr::status::FS;
        }
    }

    /// Element loop for FP operations: `vd(dw)[i] = f(vs2(s2w)[i], op1(bw)[i], vd(dw)[i])`.
    /// With `to_mask`, `f` returns a bit for the mask destination.
    fn vfloop(
        &mut self,
        c: &Ctx,
        (dw, s2w, bw): (usize, usize, usize),
        to_mask: bool,
        mut f: impl FnMut(u64, u64, u64, &mut u8) -> u64,
    ) -> Result<(), Exception> {
        let log2 = |n: usize| n.trailing_zeros() as i32;
        let emul = |w: usize| c.vt.lmul + log2(w) - log2(c.vt.sew);
        if !to_mask {
            self.vcheck_group(c.vd, emul(dw), c.raw)?;
            if !c.vm && c.vd == 0 {
                return Err(Exception::illegal(c.raw));
            }
        }
        self.vcheck_group(c.vs2, emul(s2w), c.raw)?;
        if let Src::V(r) = c.src {
            self.vcheck_group(r, emul(bw), c.raw)?;
        }
        let mut fl = 0;
        let mut out = Vec::with_capacity(c.vl);
        for i in c.start..c.vl {
            if !self.vactive(c.vm, i) {
                continue;
            }
            let a = self.velem(c.vs2, i, s2w);
            let b = match c.src {
                Src::V(r) => self.velem(r, i, bw),
                Src::X(x) => x,
            };
            let d = if to_mask { 0 } else { self.velem(c.vd, i, dw) };
            out.push((i, f(a, b, d, &mut fl)));
        }
        for (i, v) in out {
            if to_mask {
                self.set_vbit(c.vd, i, v != 0);
            } else {
                self.set_velem(c.vd, i, dw, v & mask(dw));
            }
        }
        self.vaccrue(fl);
        Ok(())
    }

    pub(crate) fn exec_opf(&mut self, insn: &Insn) -> Result<(), Exception> {
        let raw = insn.raw;
        if !self.fp_enabled() {
            return Err(Exception::illegal(raw));
        }
        let vt = self.vtype(raw)?;
        let w = vt.sew;
        let funct6 = raw >> 26;
        let vv = (raw >> 12) & 7 == 1;
        let vm = (raw >> 25) & 1 == 1;
        let rm = self.vrm(raw)?;
        let illegal = || Err(Exception::illegal(raw));
        // Conversions handle their own element widths.
        if vv && funct6 == 0b010010 {
            return self.vfcvt(insn, rm);
        }
        if w == 1 {
            return illegal();
        }
        let src = if vv { Src::V(insn.rs1) } else { Src::X(self.fscalar(insn.rs1, w)) };
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
        let same = (w, w, w);
        let neg = sign_bit(w);
        let ww = 2 * w;

        match funct6 {
            0b000000 => self.vfloop(&c, same, false, |a, b, _, fl| sfw!(w, add(a, b, rm, fl))),
            0b000010 => self.vfloop(&c, same, false, |a, b, _, fl| sfw!(w, sub(a, b, rm, fl))),
            0b100111 if !vv => self.vfloop(&c, same, false, |a, b, _, fl| sfw!(w, sub(b, a, rm, fl))),
            0b100100 => self.vfloop(&c, same, false, |a, b, _, fl| sfw!(w, mul(a, b, rm, fl))),
            0b100000 => self.vfloop(&c, same, false, |a, b, _, fl| sfw!(w, div(a, b, rm, fl))),
            0b100001 if !vv => self.vfloop(&c, same, false, |a, b, _, fl| sfw!(w, div(b, a, rm, fl))),
            0b000100 => self.vfloop(&c, same, false, |a, b, _, fl| sfw!(w, min_max(a, b, false, false, fl))),
            0b000110 => self.vfloop(&c, same, false, |a, b, _, fl| sfw!(w, min_max(a, b, true, false, fl))),
            0b001000 => self.vfloop(&c, same, false, |a, b, _, _| (a & !neg) | (b & neg)),
            0b001001 => self.vfloop(&c, same, false, |a, b, _, _| (a & !neg) | (!b & neg)),
            0b001010 => self.vfloop(&c, same, false, |a, b, _, _| a ^ (b & neg)),
            0b000001 | 0b000011 | 0b000101 | 0b000111 if vv => self.vfred(&c, funct6, rm, false),
            0b110001 | 0b110011 if vv => self.vfred(&c, funct6, rm, true),
            0b001110 if !vv => {
                let Src::X(x) = c.src else { unreachable!() };
                self.vslide1up_value(&c, x)
            }
            0b001111 if !vv => {
                let Src::X(x) = c.src else { unreachable!() };
                self.vslide1down_value(&c, x)
            }
            0b010000 if vv && insn.rs1 == 0 => {
                // vfmv.f.s
                let v = self.velem(c.vs2, 0, w);
                let boxed = if w == 8 { v } else { v | !mask(w) };
                self.state.f[insn.rd as usize] = boxed;
                self.state.csr.mstatus |= crate::csr::status::FS;
                Ok(())
            }
            0b010000 if !vv && insn.rs2 == 0 => {
                // vfmv.s.f
                let Src::X(x) = c.src else { unreachable!() };
                if c.start < c.vl {
                    self.set_velem(c.vd, 0, w, x);
                }
                Ok(())
            }
            0b010011 if vv => {
                // Unary: the rs1 field selects the operation and is not a register.
                let c = Ctx { src: Src::X(0), ..c };
                match insn.rs1 {
                    0b00000 => self.vfloop(&c, same, false, |a, _, _, fl| sfw!(w, sqrt(a, rm, fl))),
                    0b00100 => self.vfloop(&c, same, false, |a, _, _, fl| rsqrt7(a, w, fl)),
                    0b00101 => self.vfloop(&c, same, false, |a, _, _, fl| rec7(a, w, rm, fl)),
                    0b10000 => self.vfloop(&c, same, false, |a, _, _, _| sfw!(w, classify(a))),
                    _ => illegal(),
                }
            }
            0b010111 if !vv => {
                // vfmerge.vfm / vfmv.v.f
                if vm && c.vs2 != 0 {
                    return illegal();
                }
                let unmasked = Ctx { vm: true, ..c };
                let v0 = (0..c.vl).map(|i| self.vbit(0, i)).collect::<Vec<_>>();
                let mut idx = c.start;
                self.vfloop(&unmasked, same, false, |a, b, _, _| {
                    let take = vm || v0[idx];
                    idx += 1;
                    if take { b } else { a }
                })
            }
            0b011000 => self.vfloop(&c, same, true, |a, b, _, fl| sfw!(w, eq(a, b, fl)) as u64),
            0b011100 => self.vfloop(&c, same, true, |a, b, _, fl| !sfw!(w, eq(a, b, fl)) as u64),
            0b011001 => self.vfloop(&c, same, true, |a, b, _, fl| sfw!(w, le(a, b, false, fl)) as u64),
            0b011011 => self.vfloop(&c, same, true, |a, b, _, fl| sfw!(w, lt(a, b, false, fl)) as u64),
            0b011101 if !vv => self.vfloop(&c, same, true, |a, b, _, fl| sfw!(w, lt(b, a, false, fl)) as u64),
            0b011111 if !vv => self.vfloop(&c, same, true, |a, b, _, fl| sfw!(w, le(b, a, false, fl)) as u64),
            // Fused multiply-add; vs1/rs1 is `b`, vs2 is `a`, the destination is `d`.
            0b101000..=0b101111 => {
                let (product_with_vd, neg_product, neg_addend) = match funct6 {
                    0b101000 => (true, false, false),  // vfmadd:  (b * d) + a
                    0b101001 => (true, true, true),    // vfnmadd: -(b * d) - a
                    0b101010 => (true, false, true),   // vfmsub:  (b * d) - a
                    0b101011 => (true, true, false),   // vfnmsub: -(b * d) + a
                    0b101100 => (false, false, false), // vfmacc:  (b * a) + d
                    0b101101 => (false, true, true),   // vfnmacc: -(b * a) - d
                    0b101110 => (false, false, true),  // vfmsac:  (b * a) - d
                    _ => (false, true, false),         // vfnmsac: -(b * a) + d
                };
                self.vfloop(&c, same, false, |a, b, d, fl| {
                    let (m, add) = if product_with_vd { (d, a) } else { (a, d) };
                    let m = if neg_product { m ^ neg } else { m };
                    let add = if neg_addend { add ^ neg } else { add };
                    sfw!(w, mul_add(b, m, add, rm, fl))
                })
            }
            0b110000..=0b111111 => {
                if w == 8 {
                    return illegal();
                }
                let wneg = sign_bit(ww);
                let up = move |x: u64, fl: &mut u8| fconv(x, w, ww, rm, fl);
                match funct6 {
                    0b110000 => self.vfloop(&c, (ww, w, w), false, |a, b, _, fl| {
                        let (a, b) = (up(a, fl), up(b, fl));
                        sfw!(ww, add(a, b, rm, fl))
                    }),
                    0b110010 => self.vfloop(&c, (ww, w, w), false, |a, b, _, fl| {
                        let (a, b) = (up(a, fl), up(b, fl));
                        sfw!(ww, sub(a, b, rm, fl))
                    }),
                    0b110100 => self.vfloop(&c, (ww, ww, w), false, |a, b, _, fl| {
                        let b = up(b, fl);
                        sfw!(ww, add(a, b, rm, fl))
                    }),
                    0b110110 => self.vfloop(&c, (ww, ww, w), false, |a, b, _, fl| {
                        let b = up(b, fl);
                        sfw!(ww, sub(a, b, rm, fl))
                    }),
                    0b111000 => self.vfloop(&c, (ww, w, w), false, |a, b, _, fl| {
                        let (a, b) = (up(a, fl), up(b, fl));
                        sfw!(ww, mul(a, b, rm, fl))
                    }),
                    0b111100..=0b111111 => {
                        let (neg_product, neg_addend) = match funct6 {
                            0b111100 => (false, false), // vfwmacc
                            0b111101 => (true, true),   // vfwnmacc
                            0b111110 => (false, true),  // vfwmsac
                            _ => (true, false),         // vfwnmsac
                        };
                        self.vfloop(&c, (ww, w, w), false, |a, b, d, fl| {
                            let (a, b) = (up(a, fl), up(b, fl));
                            let a = if neg_product { a ^ wneg } else { a };
                            let d = if neg_addend { d ^ wneg } else { d };
                            sfw!(ww, mul_add(b, a, d, rm, fl))
                        })
                    }
                    _ => illegal(),
                }
            }
            _ => illegal(),
        }
    }

    /// FP reductions. Unordered sums are computed in order, which is a valid ordering.
    fn vfred(&mut self, c: &Ctx, funct6: u32, rm: Rm, widen: bool) -> Result<(), Exception> {
        let w = c.vt.sew;
        if c.start != 0 || (widen && w == 8) {
            return Err(Exception::illegal(c.raw));
        }
        self.vcheck_group(c.vs2, c.vt.lmul, c.raw)?;
        let Src::V(vs1) = c.src else { unreachable!() };
        if c.vl == 0 {
            return Ok(());
        }
        let aw = if widen { 2 * w } else { w };
        let mut fl = 0;
        let mut acc = self.velem(vs1, 0, aw);
        for i in 0..c.vl {
            if !self.vactive(c.vm, i) {
                continue;
            }
            let mut v = self.velem(c.vs2, i, w);
            if widen {
                v = fconv(v, w, aw, rm, &mut fl);
            }
            acc = match funct6 {
                0b000101 => sfw!(aw, min_max(acc, v, false, false, &mut fl)),
                0b000111 => sfw!(aw, min_max(acc, v, true, false, &mut fl)),
                _ => sfw!(aw, add(acc, v, rm, &mut fl)),
            };
        }
        self.set_velem(c.vd, 0, aw, acc);
        self.vaccrue(fl);
        Ok(())
    }

    /// VFUNARY0: conversions between integers and floats, and between float widths.
    fn vfcvt(&mut self, insn: &Insn, rm: Rm) -> Result<(), Exception> {
        let vt = self.vtype(insn.raw)?;
        let w = vt.sew;
        let sel = insn.rs1;
        let illegal = || Err(Exception::illegal(insn.raw));
        let c = Ctx {
            vt,
            vl: self.state.csr.vl as usize,
            start: self.state.csr.vstart as usize,
            vm: (insn.raw >> 25) & 1 == 1,
            vd: insn.rd,
            vs2: insn.rs2,
            src: Src::X(0),
            raw: insn.raw,
        };
        let is_float = |n: usize| matches!(n, 2 | 4 | 8);
        let rtz = if sel & 0b110 == 0b110 { Rm::Rtz } else { rm };
        // (destination width, source width)
        let (dw, sw) = match sel >> 3 {
            0 => (w, w),
            1 => (2 * w, w),
            2 => (w, 2 * w),
            _ => return illegal(),
        };
        if dw > 8 || sw > 8 {
            return illegal();
        }
        match sel & 0b111 {
            // float -> unsigned / signed integer (also the rtz forms)
            0b000 | 0b001 | 0b110 | 0b111 => {
                if !is_float(sw) {
                    return illegal();
                }
                let signed = sel & 1 == 1;
                self.vfloop(&c, (dw, sw, sw), false, |a, _, _, fl| {
                    sfw!(sw, to_int(a, signed, 8 * dw as u32, rtz, fl))
                })
            }
            // unsigned / signed integer -> float
            0b010 | 0b011 => {
                if !is_float(dw) {
                    return illegal();
                }
                let signed = sel & 1 == 1;
                self.vfloop(&c, (dw, sw, sw), false, |a, _, _, fl| {
                    let v = if signed { sext(a, sw) as u64 } else { a };
                    sfw!(dw, from_int(v, signed, rm, fl))
                })
            }
            // float -> float (widening or narrowing), and round-to-odd narrowing
            0b100 | 0b101 if sel >> 3 != 0 => {
                if !is_float(sw) || !is_float(dw) || (sel == 0b01101) {
                    return illegal();
                }
                let rm = if sel == 0b10101 { Rm::Rod } else { rm };
                self.vfloop(&c, (dw, sw, sw), false, |a, _, _, fl| fconv(a, sw, dw, rm, fl))
            }
            _ => illegal(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rec7_overflow_rounds_toward_zero() {
        let mut fl = 0;
        assert_eq!(rec7(0x0001, 2, Rm::Rne, &mut fl), 0x7c00);
        assert_eq!(rec7(0x0001, 2, Rm::Rtz, &mut fl), 0x7bff);
        assert_eq!(rec7(0x8001, 2, Rm::Rdn, &mut fl), 0xfc00);
        assert_eq!(rec7(0x8001, 2, Rm::Rup, &mut fl), 0xfbff);
        // riscv-vector-tests vfrec7_v-3 case 668 (RTZ).
        let got: Vec<u64> =
            [0x4e07, 0x0008, 0xfffc, 0x0010].iter().map(|&x| rec7(x, 2, Rm::Rtz, &mut fl)).collect();
        assert_eq!(got, [0x2950, 0x7bff, 0x7e00, 0x7bff]);
    }
}
