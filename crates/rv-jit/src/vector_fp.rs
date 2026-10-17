//! Native code for vector floating-point instructions, in the style of QEMU's "hardfloat":
//! the host's IEEE arithmetic gives exactly the soft-float result, with no exception flags
//! besides inexact, whenever
//!
//! - the rounding mode is round-to-nearest-even and the inexact flag is already set (it is
//!   sticky, so it need not be computed),
//! - every active input is a normal number or zero, and
//! - every active result is a normal number, or a zero that cannot have come from rounding.
//!
//! Each instruction computes all its results first and checks those conditions; if any lane
//! fails them the interpreter helper runs the instruction from its untouched inputs.
//! Sign injection, merges and moves raise no flags and need no checks; min, max and compares
//! only need the inputs to be numbers.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F32X4, F64X2, I32, I32X4, I64, I64X2};
use cranelift_codegen::ir::{Endianness, InstBuilder, MemFlagsData, Type, Value};
use rv_core::jit::abi;
use rv_isa::Insn;

use crate::Translator;
use crate::vector::Vconfig;

const FFLAGS_NX: i64 = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FOp {
    Add,
    Sub,
    /// operand - vs2 (the .vf reversed forms).
    Rsub,
    Mul,
    Div,
    Rdiv,
    Min,
    Max,
    Sgnj,
    Sgnjn,
    Sgnjx,
    /// vd = ±(x * y) ± z, see `Fma`.
    Fma(Fma),
    /// vfmv.v.f / vfmerge.vfm.
    Merge,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Fma {
    /// The product uses vd (vfmadd family) rather than vs2 (vfmacc family).
    product_with_vd: bool,
    neg_product: bool,
    neg_addend: bool,
}

#[derive(Clone, Copy)]
enum Src {
    V(u8),
    /// f[rs1], NaN-boxed to SEW.
    F(u8),
}

struct Types {
    float: Type,
    int: Type,
    sign: i64,
    min_normal: i64,
    inf: i64,
}

fn types(sew: u32) -> Option<Types> {
    match sew {
        4 => Some(Types {
            float: F32X4,
            int: I32X4,
            sign: 0x8000_0000,
            min_normal: 0x0080_0000,
            inf: 0x7f80_0000,
        }),
        8 => Some(Types {
            float: F64X2,
            int: I64X2,
            sign: i64::MIN,
            min_normal: 0x0010_0000_0000_0000,
            inf: 0x7ff0_0000_0000_0000,
        }),
        _ => None,
    }
}

impl Translator<'_> {
    fn little() -> MemFlagsData {
        MemFlagsData::new().with_endianness(Endianness::Little)
    }

    fn splat_int(&mut self, t: &Types, v: i64) -> Value {
        let lane = if t.int == I32X4 { I32 } else { I64 };
        let c = self.b.ins().iconst(lane, v);
        self.b.ins().splat(t.int, c)
    }

    /// Lanes of `x` (as integers) that are normal numbers or zeros.
    fn normal_or_zero(&mut self, t: &Types, x: Value) -> Value {
        let abs_mask = self.splat_int(t, !t.sign);
        let abs = self.b.ins().band(x, abs_mask);
        let zero = self.splat_int(t, 0);
        let is_zero = self.b.ins().icmp(IntCC::Equal, abs, zero);
        let normal = self.normal(t, abs);
        self.b.ins().bor(is_zero, normal)
    }

    /// Lanes of `abs` (magnitudes as integers) that are finite normal numbers.
    fn normal(&mut self, t: &Types, abs: Value) -> Value {
        let min = self.splat_int(t, t.min_normal);
        let inf = self.splat_int(t, t.inf);
        let big_enough = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, abs, min);
        let finite = self.b.ins().icmp(IntCC::UnsignedLessThan, abs, inf);
        self.b.ins().band(big_enough, finite)
    }

    fn is_zero(&mut self, t: &Types, x: Value) -> Value {
        let abs_mask = self.splat_int(t, !t.sign);
        let abs = self.b.ins().band(x, abs_mask);
        let zero = self.splat_int(t, 0);
        self.b.ins().icmp(IntCC::Equal, abs, zero)
    }

    /// Lanes of `x` that are not NaN.
    fn number(&mut self, t: &Types, x: Value) -> Value {
        let abs_mask = self.splat_int(t, !t.sign);
        let abs = self.b.ins().band(x, abs_mask);
        let inf = self.splat_int(t, t.inf);
        self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, abs, inf)
    }

    fn as_int(&mut self, t: &Types, v: Value) -> Value {
        self.b.ins().bitcast(t.int, Self::little(), v)
    }

    fn as_float(&mut self, t: &Types, v: Value) -> Value {
        self.b.ins().bitcast(t.float, Self::little(), v)
    }

    /// Rounding mode RNE, inexact already set, and the FP unit on: the conditions every
    /// checked operation shares.
    fn fp_state_ok(&mut self) -> Value {
        let tf = self.trusted;
        let fcsr = self.b.ins().load(I64, tf, self.state, abi::FCSR as i32);
        let frm = self.b.ins().band_imm_s(fcsr, 0xe0);
        let rne = self.b.ins().icmp_imm_s(IntCC::Equal, frm, 0);
        let nx = self.b.ins().band_imm_s(fcsr, FFLAGS_NX);
        let nx = self.b.ins().icmp_imm_s(IntCC::NotEqual, nx, 0);
        let mstatus = self.b.ins().load(I64, tf, self.state, abi::MSTATUS as i32);
        let fs = self.b.ins().band_imm_s(mstatus, rv_core::csr::status::FS as i64);
        let fs = self.b.ins().icmp_imm_s(IntCC::NotEqual, fs, 0);
        let ok = self.b.ins().band(rne, nx);
        self.b.ins().band(ok, fs)
    }

    /// The scalar operand f[r] splatted, and whether it is properly NaN-boxed.
    fn fp_scalar(&mut self, t: &Types, r: u8) -> (Value, Value) {
        let tf = self.trusted;
        let at = (abi::F + 8 * r as usize) as i32;
        let raw = self.b.ins().load(I64, tf, self.state, at);
        if t.int == I32X4 {
            let high = self.b.ins().ushr_imm_s(raw, 32);
            let boxed = self.b.ins().icmp_imm_s(IntCC::Equal, high, 0xffff_ffff);
            let low = self.b.ins().ireduce(I32, raw);
            let splat = self.b.ins().splat(I32X4, low);
            (splat, boxed)
        } else {
            let splat = self.b.ins().splat(I64X2, raw);
            let yes = self.b.ins().iconst(cranelift_codegen::ir::types::I8, 1);
            (splat, yes)
        }
    }

    /// Translates a vector FP instruction natively when possible (`false`: use the helper).
    pub(crate) fn translate_vector_fp(&mut self, cfg: Vconfig, insn: &Insn, index: u64, offset: i64) -> bool {
        let raw = insn.raw;
        let f3 = (raw >> 12) & 7;
        let funct6 = raw >> 26;
        let vm = (raw >> 25) & 1 == 1;
        let vv = f3 == 1;
        let Some(t) = types(cfg.sew) else { return false };
        let src = if vv { Src::V(insn.rs1) } else { Src::F(insn.rs1) };
        let fma = |product_with_vd, neg_product, neg_addend| {
            FOp::Fma(Fma { product_with_vd, neg_product, neg_addend })
        };
        let op = match funct6 {
            0b000000 => FOp::Add,
            0b000010 => FOp::Sub,
            0b100111 if !vv => FOp::Rsub,
            0b100100 => FOp::Mul,
            0b100000 => FOp::Div,
            0b100001 if !vv => FOp::Rdiv,
            0b000100 => FOp::Min,
            0b000110 => FOp::Max,
            0b001000 => FOp::Sgnj,
            0b001001 => FOp::Sgnjn,
            0b001010 => FOp::Sgnjx,
            0b010111 if !vv && (!vm || insn.rs2 == 0) => FOp::Merge,
            0b101000 => fma(true, false, false),
            0b101001 => fma(true, true, true),
            0b101010 => fma(true, false, true),
            0b101011 => fma(true, true, false),
            0b101100 => fma(false, false, false),
            0b101101 => fma(false, true, true),
            0b101110 => fma(false, false, true),
            0b101111 => fma(false, true, false),
            0b011000 | 0b011100 | 0b011001 | 0b011011 => {
                return self.vfcompare(cfg, &t, insn, src, index, offset);
            }
            0b011101 | 0b011111 if !vv => return self.vfcompare(cfg, &t, insn, src, index, offset),
            0b010010 if vv => return self.vfcvt(cfg, &t, insn, index, offset),
            _ => return false,
        };
        if matches!(op, FOp::Fma(_)) && !self.has_fma {
            return false;
        }
        let masked = !vm && op != FOp::Merge;
        let regs = cfg.regs();
        let (vd, vs2) = (insn.rd, insn.rs2);
        let aligned = |r: u8| (r as u32).is_multiple_of(regs);
        if !aligned(vd)
            || !aligned(vs2)
            || matches!(src, Src::V(r) if !aligned(r))
            || ((masked || op == FOp::Merge && !vm) && vd == 0)
        {
            return false;
        }
        let checked = !matches!(op, FOp::Sgnj | FOp::Sgnjn | FOp::Sgnjx | FOp::Merge);
        let vl = self.load_vl();
        let mut ok = if checked { Some(self.fp_state_ok()) } else { None };
        let scalar = match src {
            Src::F(r) => {
                let (v, boxed) = self.fp_scalar(&t, r);
                ok = Some(match ok {
                    Some(o) => self.b.ins().band(o, boxed),
                    None => boxed,
                });
                Some(v)
            }
            Src::V(_) => None,
        };
        let mut results = Vec::with_capacity(regs as usize);
        for r in 0..regs {
            let a_int = self.load_vreg(cfg, vs2 as u32 + r);
            let b_int = match src {
                Src::V(reg) => self.load_vreg(cfg, reg as u32 + r),
                Src::F(_) => scalar.unwrap(),
            };
            let d_int = self.load_vreg(cfg, vd as u32 + r);
            let mut write = self.body_mask(cfg, r, vl);
            if masked {
                let active = self.active_lanes(cfg, r);
                write = self.b.ins().band(write, active);
            }
            let sign = self.splat_int(&t, t.sign);
            let (result, lane_ok) = match op {
                FOp::Sgnj | FOp::Sgnjn | FOp::Sgnjx => {
                    let s = match op {
                        FOp::Sgnj => b_int,
                        FOp::Sgnjn => self.b.ins().bnot(b_int),
                        _ => self.b.ins().bxor(a_int, b_int),
                    };
                    let s = self.b.ins().band(s, sign);
                    let rest = self.b.ins().band_not(a_int, sign);
                    (self.b.ins().bor(rest, s), None)
                }
                FOp::Merge => {
                    let v = if vm {
                        b_int
                    } else {
                        let active = self.active_lanes(cfg, r);
                        self.b.ins().bitselect(active, b_int, a_int)
                    };
                    (v, None)
                }
                FOp::Min | FOp::Max => {
                    let (a, b) = (self.as_float(&t, a_int), self.as_float(&t, b_int));
                    let v = if op == FOp::Min { self.b.ins().fmin(a, b) } else { self.b.ins().fmax(a, b) };
                    let na = self.number(&t, a_int);
                    let nb = self.number(&t, b_int);
                    (self.as_int(&t, v), Some(self.b.ins().band(na, nb)))
                }
                _ => {
                    let (a, b, d) =
                        (self.as_float(&t, a_int), self.as_float(&t, b_int), self.as_float(&t, d_int));
                    let v = match op {
                        FOp::Add => self.b.ins().fadd(a, b),
                        FOp::Sub => self.b.ins().fsub(a, b),
                        FOp::Rsub => self.b.ins().fsub(b, a),
                        FOp::Mul => self.b.ins().fmul(a, b),
                        FOp::Div => self.b.ins().fdiv(a, b),
                        FOp::Rdiv => self.b.ins().fdiv(b, a),
                        FOp::Fma(f) => {
                            // vs1/rs1 is `b`, vs2 is `a`, the destination `d`.
                            let (m, add) = if f.product_with_vd { (d, a) } else { (a, d) };
                            let m = if f.neg_product { self.b.ins().fneg(m) } else { m };
                            let add = if f.neg_addend { self.b.ins().fneg(add) } else { add };
                            self.b.ins().fma(b, m, add)
                        }
                        _ => unreachable!(),
                    };
                    let v_int = self.as_int(&t, v);
                    // Inputs normal or zero.
                    let mut inputs = self.normal_or_zero(&t, a_int);
                    let nb = self.normal_or_zero(&t, b_int);
                    inputs = self.b.ins().band(inputs, nb);
                    if matches!(op, FOp::Fma(_)) {
                        let nd = self.normal_or_zero(&t, d_int);
                        inputs = self.b.ins().band(inputs, nd);
                    }
                    // Division by zero raises DZ.
                    match op {
                        FOp::Div => {
                            let z = self.is_zero(&t, b_int);
                            inputs = self.b.ins().band_not(inputs, z);
                        }
                        FOp::Rdiv => {
                            let z = self.is_zero(&t, a_int);
                            inputs = self.b.ins().band_not(inputs, z);
                        }
                        _ => {}
                    }
                    // Result normal, or a zero that is exact: sums of normals and zeros cannot
                    // round to zero; products and quotients only are zero for a zero operand.
                    let abs_mask = self.splat_int(&t, !t.sign);
                    let abs = self.b.ins().band(v_int, abs_mask);
                    let normal = self.normal(&t, abs);
                    let zero = self.is_zero(&t, v_int);
                    let exact_zero = match op {
                        FOp::Add | FOp::Sub | FOp::Rsub => zero,
                        FOp::Mul => {
                            let za = self.is_zero(&t, a_int);
                            let zb = self.is_zero(&t, b_int);
                            let z = self.b.ins().bor(za, zb);
                            self.b.ins().band(zero, z)
                        }
                        FOp::Div => {
                            let za = self.is_zero(&t, a_int);
                            self.b.ins().band(zero, za)
                        }
                        FOp::Rdiv => {
                            let zb = self.is_zero(&t, b_int);
                            self.b.ins().band(zero, zb)
                        }
                        _ => {
                            // Fused: zero only from a zero product plus a zero addend.
                            let za = self.is_zero(&t, a_int);
                            let zb = self.is_zero(&t, b_int);
                            let zd = self.is_zero(&t, d_int);
                            let product_zero = match op {
                                FOp::Fma(f) if f.product_with_vd => self.b.ins().bor(zb, zd),
                                _ => self.b.ins().bor(zb, za),
                            };
                            let addend_zero = match op {
                                FOp::Fma(f) if f.product_with_vd => za,
                                _ => zd,
                            };
                            let both = self.b.ins().band(product_zero, addend_zero);
                            self.b.ins().band(zero, both)
                        }
                    };
                    let result_ok = self.b.ins().bor(normal, exact_zero);
                    (v_int, Some(self.b.ins().band(inputs, result_ok)))
                }
            };
            if let Some(lane_ok) = lane_ok {
                // Only lanes that are written must pass.
                let fine = self.b.ins().bor_not(lane_ok, write);
                let all = self.b.ins().vall_true(fine);
                let all = self.b.ins().icmp_imm_s(IntCC::NotEqual, all, 0);
                ok = Some(match ok {
                    Some(o) => self.b.ins().band(o, all),
                    None => all,
                });
            }
            results.push((result, write, d_int));
        }
        let (commit, helper, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        match ok {
            Some(ok) => self.b.ins().brif(ok, commit, &[], helper, &[]),
            None => self.b.ins().jump(commit, &[]),
        };
        self.b.switch_to_block(commit);
        for (r, (result, write, old)) in results.into_iter().enumerate() {
            let out = self.b.ins().bitselect(write, result, old);
            self.store_vreg(out, vd as u32 + r as u32);
        }
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(helper);
        self.exec_via_helper(insn, index, offset);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        true
    }

    /// vmfeq, vmfne, vmfle, vmflt, vmfgt, vmfge: no NaN inputs, so no flags.
    fn vfcompare(&mut self, cfg: Vconfig, t: &Types, insn: &Insn, src: Src, index: u64, offset: i64) -> bool {
        let raw = insn.raw;
        let funct6 = raw >> 26;
        let vm = (raw >> 25) & 1 == 1;
        let regs = cfg.regs();
        let vs2 = insn.rs2;
        let aligned = |r: u8| (r as u32).is_multiple_of(regs);
        if !aligned(vs2) || matches!(src, Src::V(r) if !aligned(r)) {
            return false;
        }
        let (cc, swap) = match funct6 {
            0b011000 => (FloatCC::Equal, false),
            0b011100 => (FloatCC::NotEqual, false),
            0b011001 => (FloatCC::LessThanOrEqual, false),
            0b011011 => (FloatCC::LessThan, false),
            0b011101 => (FloatCC::LessThan, true),
            _ => (FloatCC::LessThanOrEqual, true),
        };
        let tf = self.trusted;
        let fcsr_mstatus = {
            let mstatus = self.b.ins().load(I64, tf, self.state, abi::MSTATUS as i32);
            let fs = self.b.ins().band_imm_s(mstatus, rv_core::csr::status::FS as i64);
            self.b.ins().icmp_imm_s(IntCC::NotEqual, fs, 0)
        };
        let mut ok = fcsr_mstatus;
        let vl = self.load_vl();
        let scalar = match src {
            Src::F(r) => {
                let (v, boxed) = self.fp_scalar(t, r);
                ok = self.b.ins().band(ok, boxed);
                Some(v)
            }
            Src::V(_) => None,
        };
        let zero = self.b.ins().iconst(I64, 0);
        let (mut lo, mut hi) = (zero, zero);
        let lanes = cfg.lanes();
        for r in 0..regs {
            let a_int = self.load_vreg(cfg, vs2 as u32 + r);
            let b_int = match src {
                Src::V(reg) => self.load_vreg(cfg, reg as u32 + r),
                Src::F(_) => scalar.unwrap(),
            };
            let na = self.number(t, a_int);
            let nb = self.number(t, b_int);
            let numbers = self.b.ins().band(na, nb);
            let body = self.body_mask(cfg, r, vl);
            let fine = self.b.ins().bor_not(numbers, body);
            let all = self.b.ins().vall_true(fine);
            let all = self.b.ins().icmp_imm_s(IntCC::NotEqual, all, 0);
            ok = self.b.ins().band(ok, all);
            let (a, b) = (self.as_float(t, a_int), self.as_float(t, b_int));
            let (x, y) = if swap { (b, a) } else { (a, b) };
            let lanes_true = self.b.ins().fcmp(cc, x, y);
            let bits = self.b.ins().vhigh_bits(I64, lanes_true);
            let at = r * lanes;
            if at < 64 {
                let shifted = self.b.ins().ishl_imm_s(bits, at as i64);
                lo = self.b.ins().bor(lo, shifted);
            } else {
                let shifted = self.b.ins().ishl_imm_s(bits, (at - 64) as i64);
                hi = self.b.ins().bor(hi, shifted);
            }
        }
        let (commit, helper, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().brif(ok, commit, &[], helper, &[]);
        self.b.switch_to_block(commit);
        let (mut w_lo, mut w_hi) = self.vl_bits(vl);
        if !vm {
            let (m_lo, m_hi) = self.load_mask(0);
            w_lo = self.b.ins().band(w_lo, m_lo);
            w_hi = self.b.ins().band(w_hi, m_hi);
        }
        self.store_mask_bits(insn.rd, (lo, hi), (w_lo, w_hi));
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(helper);
        self.exec_via_helper(insn, index, offset);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        true
    }

    /// Same-width conversions between 32-bit integers and floats, unmasked: vfcvt.f.x.v and
    /// vfcvt.f.xu.v (only inexact can be raised), vfcvt.x.f.v and vfcvt.rtz.x.f.v (in range).
    fn vfcvt(&mut self, cfg: Vconfig, t: &Types, insn: &Insn, index: u64, offset: i64) -> bool {
        let vm = (insn.raw >> 25) & 1 == 1;
        let sel = insn.rs1;
        if cfg.sew != 4 || !vm || !matches!(sel, 0b00001 | 0b00010 | 0b00011 | 0b00111) {
            return false;
        }
        let regs = cfg.regs();
        let (vd, vs2) = (insn.rd, insn.rs2);
        if !(vd as u32).is_multiple_of(regs) || !(vs2 as u32).is_multiple_of(regs) {
            return false;
        }
        let vl = self.load_vl();
        let mut ok = self.fp_state_ok();
        let mut results = Vec::new();
        for r in 0..regs {
            let x = self.load_vreg(cfg, vs2 as u32 + r);
            let old = self.load_vreg(cfg, vd as u32 + r);
            let body = self.body_mask(cfg, r, vl);
            let result = match sel {
                0b00011 => {
                    let f = self.b.ins().fcvt_from_sint(F32X4, x);
                    self.as_int(t, f)
                }
                0b00010 => {
                    let f = self.b.ins().fcvt_from_uint(F32X4, x);
                    self.as_int(t, f)
                }
                _ => {
                    // Only numbers well inside the int32 range: no invalid-operation flag.
                    let f = self.as_float(t, x);
                    let rounded = if sel == 0b00001 { self.b.ins().nearest(f) } else { f };
                    let abs_mask = self.splat_int(t, !t.sign);
                    let abs = self.b.ins().band(x, abs_mask);
                    let limit = self.splat_int(t, 0x4f00_0000); // 2^31
                    let in_range = self.b.ins().icmp(IntCC::UnsignedLessThan, abs, limit);
                    let fine = self.b.ins().bor_not(in_range, body);
                    let all = self.b.ins().vall_true(fine);
                    let all = self.b.ins().icmp_imm_s(IntCC::NotEqual, all, 0);
                    ok = self.b.ins().band(ok, all);
                    self.b.ins().fcvt_to_sint_sat(I32X4, rounded)
                }
            };
            results.push((result, body, old));
        }
        let (commit, helper, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().brif(ok, commit, &[], helper, &[]);
        self.b.switch_to_block(commit);
        for (r, (result, body, old)) in results.into_iter().enumerate() {
            let out = self.b.ins().bitselect(body, result, old);
            self.store_vreg(out, vd as u32 + r as u32);
        }
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(helper);
        self.exec_via_helper(insn, index, offset);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        true
    }
}
