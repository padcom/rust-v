//! Native code for common vector instructions.
//!
//! Only used where the vector configuration is known at compile time: after a VSETVLI or
//! VSETIVLI with an immediate `vtype` earlier in the same guest block (which also guarantees
//! that the vector unit is on and `vstart` is 0), and with 128-bit vector registers, so that
//! each register of a group is one Cranelift vector value. `vl` stays a runtime value: lanes
//! at or past it are left undisturbed. Masked forms, other register lengths and everything
//! else go through the interpreter helper.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I8X16, I16, I16X8, I32, I32X4, I64, I64X2};
use cranelift_codegen::ir::{ConstantData, InstBuilder, MemFlagsData, Type, Value};
use rv_core::jit::abi;
use rv_core::mmu::Access;
use rv_isa::{Insn, Op};

use crate::Translator;

/// Bytes per vector register this module handles.
pub(crate) const VLENB: u32 = 16;

/// The vector configuration a guest block established: SEW in bytes and log2(LMUL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Vconfig {
    pub sew: u32,
    pub lmul: i32,
}

impl Vconfig {
    pub(crate) fn from_vtype(vtype: u64) -> Self {
        let sew = 1 << ((vtype >> 3) & 7);
        let lmul = match vtype & 7 {
            v @ 0..=3 => v as i32,
            v => v as i32 - 8,
        };
        Vconfig { sew, lmul }
    }

    /// Registers in a group (fractional LMUL uses part of one register).
    fn regs(self) -> u32 {
        if self.lmul > 0 { 1 << self.lmul } else { 1 }
    }

    fn lanes(self) -> u32 {
        VLENB / self.sew
    }

    fn vector_type(self) -> Type {
        lane_vector(self.sew)
    }

    fn lane_type(self) -> Type {
        match self.sew {
            1 => I8,
            2 => I16,
            4 => I32,
            _ => I64,
        }
    }

    fn vlmax(self) -> u32 {
        if self.lmul >= 0 { self.lanes() << self.lmul } else { self.lanes() >> -self.lmul }
    }
}

fn lane_vector(bytes: u32) -> Type {
    match bytes {
        1 => I8X16,
        2 => I16X8,
        4 => I32X4,
        _ => I64X2,
    }
}

#[derive(Clone, Copy)]
enum Operand {
    V(u8),
    X(u8),
    Imm(i64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arith {
    Add,
    Sub,
    Rsub,
    And,
    Or,
    Xor,
    MinU,
    Min,
    MaxU,
    Max,
    Sll,
    Srl,
    Sra,
    Mul,
    /// vmv.v.v / vmv.v.x / vmv.v.i: the operand itself.
    Move,
}

impl Translator<'_> {
    fn v_offset(reg: u32) -> i32 {
        (abi::V + (reg * VLENB) as usize) as i32
    }

    fn load_vreg(&mut self, cfg: Vconfig, reg: u32) -> Value {
        self.b.ins().load(cfg.vector_type(), self.trusted, self.state, Self::v_offset(reg))
    }

    fn store_vreg(&mut self, value: Value, reg: u32) {
        self.b.ins().store(self.trusted, value, self.state, Self::v_offset(reg));
    }

    fn vconst(&mut self, ty: Type, bytes: Vec<u8>) -> Value {
        let c = self.b.func.dfg.constants.insert(ConstantData::from(bytes));
        self.b.ins().vconst(ty, c)
    }

    /// Lanes of register `r` of a group that are below `vl` (all-ones lanes), as a vector of
    /// the configuration's lane type.
    fn body_mask(&mut self, cfg: Vconfig, r: u32, vl: Value) -> Value {
        let lanes = cfg.lanes();
        let mut bytes = Vec::with_capacity(16);
        for i in 0..lanes {
            let index = (r * lanes + i) as u64;
            bytes.extend_from_slice(&index.to_le_bytes()[..cfg.sew as usize]);
        }
        let index = self.vconst(cfg.vector_type(), bytes);
        let narrow = if cfg.lane_type() == I64 { vl } else { self.b.ins().ireduce(cfg.lane_type(), vl) };
        let limit = self.b.ins().splat(cfg.vector_type(), narrow);
        self.b.ins().icmp(IntCC::UnsignedLessThan, index, limit)
    }

    fn load_vl(&mut self) -> Value {
        self.b.ins().load(I64, self.trusted, self.state, abi::VL as i32)
    }

    /// The scalar operand of a .vx/.vi form, splatted.
    fn splat_operand(&mut self, cfg: Vconfig, op: Operand) -> Value {
        let scalar = match op {
            Operand::X(r) => self.get(r),
            Operand::Imm(v) => self.b.ins().iconst(I64, v),
            Operand::V(_) => unreachable!(),
        };
        let narrow =
            if cfg.lane_type() == I64 { scalar } else { self.b.ins().ireduce(cfg.lane_type(), scalar) };
        self.b.ins().splat(cfg.vector_type(), narrow)
    }

    /// Whether `reg` is a legal base for a group of `regs` registers.
    fn aligned(reg: u8, regs: u32) -> bool {
        (reg as u32).is_multiple_of(regs)
    }

    /// Translates `insn` natively if it is one of the supported forms under the block's known
    /// vector configuration. Returns `false` to have the caller use the helper.
    pub(crate) fn translate_vector(&mut self, insn: &Insn, index: u64, offset: i64) -> bool {
        let Some(cfg) = self.vconfig else { return false };
        if self.vlenb != VLENB {
            return false;
        }
        match insn.op {
            Op::Vload | Op::Vstore => self.vector_memory(cfg, insn, index, offset),
            Op::Vop => self.vector_op(cfg, insn),
            _ => false,
        }
    }

    fn vector_op(&mut self, cfg: Vconfig, insn: &Insn) -> bool {
        let raw = insn.raw;
        let f3 = (raw >> 12) & 7;
        let funct6 = raw >> 26;
        let vm = (raw >> 25) & 1 == 1;
        let masked = !vm;
        let (vd, vs2, rs1) = (insn.rd, insn.rs2, insn.rs1);
        let simm = ((rs1 as i8) << 3 >> 3) as i64;
        match f3 {
            // OPIVV, OPIVX, OPIVI
            0 | 3 | 4 => {
                let operand = match f3 {
                    0 => Operand::V(rs1),
                    4 => Operand::X(rs1),
                    // Shifts, slides and gathers take the immediate unsigned.
                    _ if matches!(
                        funct6,
                        0b100101 | 0b101000 | 0b101001 | 0b001100 | 0b001110 | 0b001111
                    ) =>
                    {
                        Operand::Imm(rs1 as i64)
                    }
                    _ => Operand::Imm(simm),
                };
                let arith = match funct6 {
                    0b000000 => Arith::Add,
                    0b000010 if f3 != 3 => Arith::Sub,
                    0b000011 if f3 != 0 => Arith::Rsub,
                    0b001001 => Arith::And,
                    0b001010 => Arith::Or,
                    0b001011 => Arith::Xor,
                    0b000100 if f3 != 3 => Arith::MinU,
                    0b000101 if f3 != 3 => Arith::Min,
                    0b000110 if f3 != 3 => Arith::MaxU,
                    0b000111 if f3 != 3 => Arith::Max,
                    0b100101 => Arith::Sll,
                    0b101000 => Arith::Srl,
                    0b101001 => Arith::Sra,
                    // vmv.v.* is vmerge with vm=1 and vs2=0.
                    0b010111 if vm && vs2 == 0 => Arith::Move,
                    0b010111 if masked => return self.vmerge(cfg, vd, vs2, operand),
                    0b011000..=0b011111 => {
                        return self.vector_compare(cfg, funct6, vd, vs2, operand, f3, masked);
                    }
                    0b001111 if f3 == 3 && vm => return self.vslide(cfg, vd, vs2, rs1 as u32, false),
                    0b001110 if f3 == 3 && vm => return self.vslide(cfg, vd, vs2, rs1 as u32, true),
                    0b001100 if f3 == 3 && vm => return self.vrgather_vi(cfg, vd, vs2, rs1 as u32),
                    _ => return false,
                };
                self.vector_arith(cfg, arith, vd, vs2, operand, masked)
            }
            // OPMVV
            2 => match funct6 {
                0b100101 => self.vector_arith(cfg, Arith::Mul, vd, vs2, Operand::V(rs1), masked),
                0b000000..=0b000111 if vm => self.vreduce(cfg, funct6, vd, vs2, rs1),
                // VWXUNARY0: vmv.x.s, vcpop.m, vfirst.m
                0b010000 if vm => match rs1 {
                    0b00000 => self.vmv_x_s(cfg, vd, vs2),
                    0b10000 | 0b10001 => self.vmask_count(vd, vs2, rs1 == 0b10001),
                    _ => false,
                },
                // VMUNARY0: vid.v
                0b010100 if vm && rs1 == 0b10001 && vs2 == 0 => self.vid(cfg, vd),
                // Mask logical operations (always unmasked).
                0b011000..=0b011111 if vm => self.vmask_logic(funct6, vd, vs2, rs1),
                _ => false,
            },
            // OPMVX
            6 => match funct6 {
                0b100101 => self.vector_arith(cfg, Arith::Mul, vd, vs2, Operand::X(rs1), masked),
                // VRXUNARY0: vmv.s.x
                0b010000 if vm && vs2 == 0 => self.vmv_s_x(cfg, vd, rs1),
                0b001111 if vm => self.vslide1(cfg, vd, vs2, rs1, false),
                0b001110 if vm => self.vslide1(cfg, vd, vs2, rs1, true),
                _ => false,
            },
            _ => false,
        }
    }

    fn vector_arith(
        &mut self,
        cfg: Vconfig,
        arith: Arith,
        vd: u8,
        vs2: u8,
        operand: Operand,
        masked: bool,
    ) -> bool {
        let regs = cfg.regs();
        let aligned = Self::aligned(vd, regs)
            && Self::aligned(vs2, regs)
            && match operand {
                Operand::V(r) => Self::aligned(r, regs),
                _ => true,
            };
        // A masked destination must not be the mask.
        if !aligned || (masked && vd == 0) {
            return false;
        }
        let vl = self.load_vl();
        let scalar = match operand {
            Operand::V(_) => None,
            _ => Some(self.splat_operand(cfg, operand)),
        };
        let amount = match (arith, operand) {
            (Arith::Sll | Arith::Srl | Arith::Sra, Operand::X(r)) => Some(self.get(r)),
            (Arith::Sll | Arith::Srl | Arith::Sra, Operand::Imm(v)) => Some(self.b.ins().iconst(I64, v)),
            _ => None,
        };
        for r in 0..regs {
            let a = self.load_vreg(cfg, vs2 as u32 + r);
            let b = match operand {
                Operand::V(reg) => self.load_vreg(cfg, reg as u32 + r),
                _ => scalar.unwrap(),
            };
            let result = match (arith, amount) {
                // Per-lane shift amounts: Cranelift shifts take one amount, so go lane by lane.
                (Arith::Sll | Arith::Srl | Arith::Sra, None) => self.per_lane_shift(cfg, arith, a, b),
                _ => {
                    let ins = self.b.ins();
                    match arith {
                        Arith::Add => ins.iadd(a, b),
                        Arith::Sub => ins.isub(a, b),
                        Arith::Rsub => ins.isub(b, a),
                        Arith::And => ins.band(a, b),
                        Arith::Or => ins.bor(a, b),
                        Arith::Xor => ins.bxor(a, b),
                        Arith::MinU => ins.umin(a, b),
                        Arith::Min => ins.smin(a, b),
                        Arith::MaxU => ins.umax(a, b),
                        Arith::Max => ins.smax(a, b),
                        // Shift amounts are taken modulo SEW by Cranelift as by RVV.
                        Arith::Sll => ins.ishl(a, amount.unwrap()),
                        Arith::Srl => ins.ushr(a, amount.unwrap()),
                        Arith::Sra => ins.sshr(a, amount.unwrap()),
                        Arith::Mul => ins.imul(a, b),
                        Arith::Move => b,
                    }
                }
            };
            let old = self.load_vreg(cfg, vd as u32 + r);
            let mut write = self.body_mask(cfg, r, vl);
            if masked {
                let active = self.active_lanes(cfg, r);
                write = self.b.ins().band(write, active);
            }
            let out = self.b.ins().bitselect(write, result, old);
            self.store_vreg(out, vd as u32 + r);
        }
        true
    }

    fn per_lane_shift(&mut self, cfg: Vconfig, arith: Arith, a: Value, amounts: Value) -> Value {
        let mut out = a;
        for l in 0..cfg.lanes() as u8 {
            let x = self.b.ins().extractlane(a, l);
            let n = self.b.ins().extractlane(amounts, l);
            // Scalar shifts also take the amount modulo the width.
            let y = match arith {
                Arith::Sll => self.b.ins().ishl(x, n),
                Arith::Srl => self.b.ins().ushr(x, n),
                _ => self.b.ins().sshr(x, n),
            };
            out = self.b.ins().insertlane(out, y, l);
        }
        out
    }

    /// Lanes of register `r` of a group whose bit in the mask register v0 is set.
    fn active_lanes(&mut self, cfg: Vconfig, r: u32) -> Value {
        let lanes = cfg.lanes();
        let first = r * lanes;
        let word = Self::v_offset(0) + (first / 64 * 8) as i32;
        let bits = self.b.ins().load(I64, self.trusted, self.state, word);
        let bits = self.b.ins().ushr_imm_s(bits, (first % 64) as i64);
        if cfg.sew == 1 {
            // 16 lanes from 16 bits: lanes 0-7 test bits of the low byte, 8-15 of the high one.
            let half = self.b.ins().ireduce(I16, bits);
            let splat = self.b.ins().splat(I16X8, half);
            let le = MemFlagsData::new().with_endianness(cranelift_codegen::ir::Endianness::Little);
            let bytes = self.b.ins().bitcast(I8X16, le, splat);
            let pick: Vec<u8> = (0..16).map(|i| (i / 8) as u8).collect();
            let pick = self.b.func.dfg.immediates.push(ConstantData::from(pick));
            let spread = self.b.ins().shuffle(bytes, bytes, pick);
            let tests = self.vconst(I8X16, (0..16).map(|i| 1u8 << (i % 8)).collect());
            let set = self.b.ins().band(spread, tests);
            let zero = self.vconst(I8X16, vec![0; 16]);
            return self.b.ins().icmp(IntCC::NotEqual, set, zero);
        }
        let ty = cfg.lane_type();
        let narrow = if ty == I64 { bits } else { self.b.ins().ireduce(ty, bits) };
        let splat = self.b.ins().splat(cfg.vector_type(), narrow);
        let mut tests = Vec::with_capacity(16);
        for i in 0..lanes {
            tests.extend_from_slice(&(1u64 << i).to_le_bytes()[..cfg.sew as usize]);
        }
        let tests = self.vconst(cfg.vector_type(), tests);
        let set = self.b.ins().band(splat, tests);
        let zero = self.vconst(cfg.vector_type(), vec![0; 16]);
        self.b.ins().icmp(IntCC::NotEqual, set, zero)
    }

    /// vmerge.vvm/vxm/vim: vd[i] = v0[i] ? operand : vs2[i], for every i below vl.
    fn vmerge(&mut self, cfg: Vconfig, vd: u8, vs2: u8, operand: Operand) -> bool {
        let regs = cfg.regs();
        let aligned = Self::aligned(vd, regs)
            && Self::aligned(vs2, regs)
            && match operand {
                Operand::V(r) => Self::aligned(r, regs),
                _ => true,
            };
        if !aligned || vd == 0 {
            return false;
        }
        let vl = self.load_vl();
        let scalar = match operand {
            Operand::V(_) => None,
            _ => Some(self.splat_operand(cfg, operand)),
        };
        for r in 0..regs {
            let a = self.load_vreg(cfg, vs2 as u32 + r);
            let b = match operand {
                Operand::V(reg) => self.load_vreg(cfg, reg as u32 + r),
                _ => scalar.unwrap(),
            };
            let active = self.active_lanes(cfg, r);
            let merged = self.b.ins().bitselect(active, b, a);
            let old = self.load_vreg(cfg, vd as u32 + r);
            let body = self.body_mask(cfg, r, vl);
            let out = self.b.ins().bitselect(body, merged, old);
            self.store_vreg(out, vd as u32 + r);
        }
        true
    }

    /// Per-byte lane numbers of one register, and the bytes of lanes below `vl`.
    fn byte_lanes(&mut self, cfg: Vconfig, vl: Value) -> (Value, Value) {
        let index = self.vconst(I8X16, (0..16).map(|j| (j / cfg.sew) as u8).collect());
        let narrow = self.b.ins().ireduce(I8, vl);
        let limit = self.b.ins().splat(I8X16, narrow);
        let body = self.b.ins().icmp(IntCC::UnsignedLessThan, index, limit);
        (index, body)
    }

    fn load_bytes(&mut self, reg: u32) -> Value {
        self.b.ins().load(I8X16, self.trusted, self.state, Self::v_offset(reg))
    }

    /// A byte shuffle of `a` (indices 0-15) and `b` (16-31) choosing, for each lane of the
    /// result, the lane `pick(lane)` of `a`, or `Err(lane)` of `b`.
    fn shuffle_lanes(
        &mut self,
        cfg: Vconfig,
        a: Value,
        b: Value,
        pick: impl Fn(u32) -> Result<u32, u32>,
    ) -> Value {
        let sew = cfg.sew;
        let mask: Vec<u8> = (0..16)
            .map(|j| {
                let (lane, byte) = (j / sew, j % sew);
                match pick(lane) {
                    Ok(src) => (src * sew + byte) as u8,
                    Err(src) => (16 + src * sew + byte) as u8,
                }
            })
            .collect();
        let mask = self.b.func.dfg.immediates.push(ConstantData::from(mask));
        self.b.ins().shuffle(a, b, mask)
    }

    /// vslidedown.vi / vslideup.vi within a single register.
    fn vslide(&mut self, cfg: Vconfig, vd: u8, vs2: u8, by: u32, up: bool) -> bool {
        if cfg.regs() != 1 || (up && vd == vs2) {
            return false;
        }
        let vl = self.load_vl();
        let vlmax = cfg.vlmax();
        let src = self.load_bytes(vs2 as u32);
        let old = self.load_bytes(vd as u32);
        let zero = self.vconst(I8X16, vec![0; 16]);
        let result = if up {
            // Lanes below the offset keep their value.
            self.shuffle_lanes(cfg, src, old, |i| if i >= by { Ok(i - by) } else { Err(i) })
        } else {
            // Lanes reading past VLMAX get zero.
            self.shuffle_lanes(cfg, src, zero, |i| if i + by < vlmax { Ok(i + by) } else { Err(0) })
        };
        let (_, body) = self.byte_lanes(cfg, vl);
        let out = self.b.ins().bitselect(body, result, old);
        self.store_bytes(out, vd as u32);
        true
    }

    /// Copies the first `len` bytes of the register file from register `reg` on to guest
    /// memory at `host`, then continues at `done`.
    fn store_bytes_loop(&mut self, reg: u32, host: Value, len: Value, done: cranelift_codegen::ir::Block) {
        let guest = MemFlagsData::new().with_notrap();
        let t = self.trusted;
        let (head, body) = (self.b.create_block(), self.b.create_block());
        let i = self.b.append_block_param(head, I64);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().jump(head, &[zero.into()]);
        self.b.switch_to_block(head);
        let more = self.b.ins().icmp(IntCC::UnsignedLessThan, i, len);
        self.b.ins().brif(more, body, &[], done, &[]);
        self.b.switch_to_block(body);
        let src = self.b.ins().iadd(self.state, i);
        let byte = self.b.ins().uload8(I64, t, src, Self::v_offset(reg));
        let dst = self.b.ins().iadd(host, i);
        self.b.ins().istore8(guest, byte, dst, 0);
        let next = self.b.ins().iadd_imm_s(i, 1);
        self.b.ins().jump(head, &[next.into()]);
    }

    fn store_bytes(&mut self, value: Value, reg: u32) {
        self.b.ins().store(self.trusted, value, self.state, Self::v_offset(reg));
    }

    /// vslide1down.vx / vslide1up.vx within a single register.
    fn vslide1(&mut self, cfg: Vconfig, vd: u8, vs2: u8, rs1: u8, up: bool) -> bool {
        if cfg.regs() != 1 || (up && vd == vs2) {
            return false;
        }
        let vl = self.load_vl();
        let src = self.load_bytes(vs2 as u32);
        let old = self.load_bytes(vd as u32);
        let zero = self.vconst(I8X16, vec![0; 16]);
        let lanes = cfg.lanes();
        let shifted = if up {
            self.shuffle_lanes(cfg, src, zero, |i| if i >= 1 { Ok(i - 1) } else { Err(0) })
        } else {
            self.shuffle_lanes(cfg, src, zero, |i| if i + 1 < lanes { Ok(i + 1) } else { Err(0) })
        };
        // The scalar goes to lane 0 (up) or lane vl - 1 (down).
        let x = self.get(rs1);
        let ty = cfg.lane_type();
        let narrow = if ty == I64 { x } else { self.b.ins().ireduce(ty, x) };
        let splat = self.b.ins().splat(cfg.vector_type(), narrow);
        let le = MemFlagsData::new().with_endianness(cranelift_codegen::ir::Endianness::Little);
        let scalar_bytes = self.b.ins().bitcast(I8X16, le, splat);
        let (index, body) = self.byte_lanes(cfg, vl);
        let at = if up {
            self.b.ins().iconst(I8, 0)
        } else {
            let last = self.b.ins().iadd_imm_s(vl, -1);
            self.b.ins().ireduce(I8, last)
        };
        let at = self.b.ins().splat(I8X16, at);
        let is_at = self.b.ins().icmp(IntCC::Equal, index, at);
        let result = self.b.ins().bitselect(is_at, scalar_bytes, shifted);
        let out = self.b.ins().bitselect(body, result, old);
        self.store_bytes(out, vd as u32);
        true
    }

    /// vrgather.vi within a single register: every lane below vl gets lane `index` of vs2
    /// (or zero past VLMAX).
    fn vrgather_vi(&mut self, cfg: Vconfig, vd: u8, vs2: u8, index: u32) -> bool {
        if cfg.regs() != 1 || vd == vs2 {
            return false;
        }
        let vl = self.load_vl();
        let vlmax = cfg.vlmax();
        let src = self.load_bytes(vs2 as u32);
        let old = self.load_bytes(vd as u32);
        let zero = self.vconst(I8X16, vec![0; 16]);
        let result = self.shuffle_lanes(cfg, src, zero, |_| if index < vlmax { Ok(index) } else { Err(0) });
        let (_, body) = self.byte_lanes(cfg, vl);
        let out = self.b.ins().bitselect(body, result, old);
        self.store_bytes(out, vd as u32);
        true
    }

    /// Integer reductions (vredsum, vredand, vredor, vredxor, vredminu, vredmin, vredmaxu,
    /// vredmax), unmasked, over groups of up to two registers: vd[0] = op(vs1[0], vs2[i < vl]).
    fn vreduce(&mut self, cfg: Vconfig, funct6: u32, vd: u8, vs2: u8, vs1: u8) -> bool {
        let regs = cfg.regs();
        if regs > 2 || !Self::aligned(vs2, regs) {
            return false;
        }
        let ty = cfg.lane_type();
        let t = self.trusted;
        let vl = self.load_vl();
        let mut acc = self.b.ins().load(ty, t, self.state, Self::v_offset(vs1 as u32));
        let lanes = cfg.lanes().min(cfg.vlmax());
        for r in 0..regs {
            let v = self.load_vreg(cfg, vs2 as u32 + r);
            for l in 0..lanes {
                let x = self.b.ins().extractlane(v, l as u8);
                let ins = self.b.ins();
                let next = match funct6 {
                    0b000000 => ins.iadd(acc, x),
                    0b000001 => ins.band(acc, x),
                    0b000010 => ins.bor(acc, x),
                    0b000011 => ins.bxor(acc, x),
                    0b000100 => ins.umin(acc, x),
                    0b000101 => ins.smin(acc, x),
                    0b000110 => ins.umax(acc, x),
                    _ => ins.smax(acc, x),
                };
                let below =
                    self.b.ins().icmp_imm_s(IntCC::UnsignedGreaterThan, vl, (r * cfg.lanes() + l) as i64);
                acc = self.b.ins().select(below, next, acc);
            }
        }
        // Nothing is written when vl is 0.
        let old = self.b.ins().load(ty, t, self.state, Self::v_offset(vd as u32));
        let any = self.b.ins().icmp_imm_s(IntCC::NotEqual, vl, 0);
        let v = self.b.ins().select(any, acc, old);
        self.b.ins().store(t, v, self.state, Self::v_offset(vd as u32));
        true
    }

    /// The mask register `reg` as two 64-bit halves (VLEN = 128).
    fn load_mask(&mut self, reg: u8) -> (Value, Value) {
        let at = Self::v_offset(reg as u32);
        let lo = self.b.ins().load(I64, self.trusted, self.state, at);
        let hi = self.b.ins().load(I64, self.trusted, self.state, at + 8);
        (lo, hi)
    }

    /// Bits below `vl`, as two 64-bit halves.
    fn vl_bits(&mut self, vl: Value) -> (Value, Value) {
        let b = &mut self.b;
        let one = b.ins().iconst(I64, 1);
        let all = b.ins().iconst(I64, -1);
        let zero = b.ins().iconst(I64, 0);
        // Low half: vl >= 64 ? all : (1 << vl) - 1 (the shift amount is below 64 when used).
        let shifted = b.ins().ishl(one, vl);
        let partial = b.ins().iadd_imm_s(shifted, -1);
        let full_lo = b.ins().icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, vl, 64);
        let lo = b.ins().select(full_lo, all, partial);
        // High half: vl >= 128 ? all : vl > 64 ? (1 << (vl - 64)) - 1 : 0.
        let vl_hi = b.ins().iadd_imm_s(vl, -64);
        let shifted = b.ins().ishl(one, vl_hi);
        let partial = b.ins().iadd_imm_s(shifted, -1);
        let full_hi = b.ins().icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, vl, 128);
        let some_hi = b.ins().icmp_imm_s(IntCC::UnsignedGreaterThan, vl, 64);
        let hi = b.ins().select(some_hi, partial, zero);
        let hi = b.ins().select(full_hi, all, hi);
        (lo, hi)
    }

    /// Writes `bits` into mask register `vd` where `which` is set, leaving the rest.
    fn store_mask_bits(&mut self, vd: u8, bits: (Value, Value), which: (Value, Value)) {
        let (old_lo, old_hi) = self.load_mask(vd);
        let lo = self.b.ins().bitselect(which.0, bits.0, old_lo);
        let hi = self.b.ins().bitselect(which.1, bits.1, old_hi);
        let at = Self::v_offset(vd as u32);
        self.b.ins().store(self.trusted, lo, self.state, at);
        self.b.ins().store(self.trusted, hi, self.state, at + 8);
    }

    #[allow(clippy::too_many_arguments)]
    fn vector_compare(
        &mut self,
        cfg: Vconfig,
        funct6: u32,
        vd: u8,
        vs2: u8,
        operand: Operand,
        f3: u32,
        masked: bool,
    ) -> bool {
        let cc = match funct6 {
            0b011000 => IntCC::Equal,
            0b011001 => IntCC::NotEqual,
            0b011010 if f3 != 3 => IntCC::UnsignedLessThan,
            0b011011 if f3 != 3 => IntCC::SignedLessThan,
            0b011100 => IntCC::UnsignedLessThanOrEqual,
            0b011101 => IntCC::SignedLessThanOrEqual,
            0b011110 if f3 != 0 => IntCC::UnsignedGreaterThan,
            0b011111 if f3 != 0 => IntCC::SignedGreaterThan,
            _ => return false,
        };
        let regs = cfg.regs();
        let aligned = Self::aligned(vs2, regs)
            && match operand {
                Operand::V(r) => Self::aligned(r, regs),
                _ => true,
            };
        if !aligned {
            return false;
        }
        let vl = self.load_vl();
        let scalar = match operand {
            Operand::V(_) => None,
            _ => Some(self.splat_operand(cfg, operand)),
        };
        let zero = self.b.ins().iconst(I64, 0);
        let (mut lo, mut hi) = (zero, zero);
        let lanes = cfg.lanes();
        for r in 0..regs {
            let a = self.load_vreg(cfg, vs2 as u32 + r);
            let b = match operand {
                Operand::V(reg) => self.load_vreg(cfg, reg as u32 + r),
                _ => scalar.unwrap(),
            };
            let lanes_true = self.b.ins().icmp(cc, a, b);
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
        // Results exist for elements below vl (and active ones, when masked); with fractional
        // LMUL there are fewer lanes, and vl <= VLMAX keeps the extra ones out.
        let (mut w_lo, mut w_hi) = self.vl_bits(vl);
        if masked {
            let (m_lo, m_hi) = self.load_mask(0);
            w_lo = self.b.ins().band(w_lo, m_lo);
            w_hi = self.b.ins().band(w_hi, m_hi);
        }
        self.store_mask_bits(vd, (lo, hi), (w_lo, w_hi));
        true
    }

    fn vmask_logic(&mut self, funct6: u32, vd: u8, vs2: u8, vs1: u8) -> bool {
        let vl = self.load_vl();
        let (a_lo, a_hi) = self.load_mask(vs2);
        let (b_lo, b_hi) = self.load_mask(vs1);
        let mut op = |a: Value, b: Value| -> Value {
            let ins = self.b.ins();
            match funct6 {
                0b011000 => ins.band_not(a, b), // vmandn
                0b011001 => ins.band(a, b),     // vmand
                0b011010 => ins.bor(a, b),      // vmor
                0b011011 => ins.bxor(a, b),     // vmxor
                0b011100 => ins.bor_not(a, b),  // vmorn
                0b011101 => {
                    let x = ins.band(a, b); // vmnand
                    self.b.ins().bnot(x)
                }
                0b011110 => {
                    let x = ins.bor(a, b); // vmnor
                    self.b.ins().bnot(x)
                }
                _ => {
                    let x = ins.bxor(a, b); // vmxnor
                    self.b.ins().bnot(x)
                }
            }
        };
        let lo = op(a_lo, b_lo);
        let hi = op(a_hi, b_hi);
        let which = self.vl_bits(vl);
        self.store_mask_bits(vd, (lo, hi), which);
        true
    }

    /// vcpop.m (count) or vfirst.m (`first`), unmasked, into x[rd].
    fn vmask_count(&mut self, rd: u8, vs2: u8, first: bool) -> bool {
        let vl = self.load_vl();
        let (lo, hi) = self.load_mask(vs2);
        let (w_lo, w_hi) = self.vl_bits(vl);
        let lo = self.b.ins().band(lo, w_lo);
        let hi = self.b.ins().band(hi, w_hi);
        let result = if first {
            let ctz_lo = self.b.ins().ctz(lo);
            let ctz_hi = self.b.ins().ctz(hi);
            let hi_index = self.b.ins().iadd_imm_s(ctz_hi, 64);
            let none = self.b.ins().iconst(I64, -1);
            let in_hi = self.b.ins().icmp_imm_s(IntCC::NotEqual, hi, 0);
            let from_hi = self.b.ins().select(in_hi, hi_index, none);
            let in_lo = self.b.ins().icmp_imm_s(IntCC::NotEqual, lo, 0);
            self.b.ins().select(in_lo, ctz_lo, from_hi)
        } else {
            let a = self.b.ins().popcnt(lo);
            let b = self.b.ins().popcnt(hi);
            self.b.ins().iadd(a, b)
        };
        self.set(rd, result);
        true
    }

    fn vmv_x_s(&mut self, cfg: Vconfig, rd: u8, vs2: u8) -> bool {
        let at = Self::v_offset(vs2 as u32);
        let v = match cfg.sew {
            1 => self.b.ins().sload8(I64, self.trusted, self.state, at),
            2 => self.b.ins().sload16(I64, self.trusted, self.state, at),
            4 => self.b.ins().sload32(self.trusted, self.state, at),
            _ => self.b.ins().load(I64, self.trusted, self.state, at),
        };
        self.set(rd, v);
        true
    }

    fn vmv_s_x(&mut self, cfg: Vconfig, vd: u8, rs1: u8) -> bool {
        let vl = self.load_vl();
        let at = Self::v_offset(vd as u32);
        let ty = cfg.lane_type();
        let old = self.b.ins().load(ty, self.trusted, self.state, at);
        let x = self.get(rs1);
        let new = if ty == I64 { x } else { self.b.ins().ireduce(ty, x) };
        // Only written when vl > 0.
        let write = self.b.ins().icmp_imm_s(IntCC::NotEqual, vl, 0);
        let v = self.b.ins().select(write, new, old);
        self.b.ins().store(self.trusted, v, self.state, at);
        true
    }

    fn vid(&mut self, cfg: Vconfig, vd: u8) -> bool {
        let regs = cfg.regs();
        if !Self::aligned(vd, regs) {
            return false;
        }
        let vl = self.load_vl();
        let lanes = cfg.lanes();
        for r in 0..regs {
            let mut bytes = Vec::with_capacity(16);
            for i in 0..lanes {
                bytes.extend_from_slice(&((r * lanes + i) as u64).to_le_bytes()[..cfg.sew as usize]);
            }
            let ids = self.vconst(cfg.vector_type(), bytes);
            let old = self.load_vreg(cfg, vd as u32 + r);
            let body = self.body_mask(cfg, r, vl);
            let out = self.b.ins().bitselect(body, ids, old);
            self.store_vreg(out, vd as u32 + r);
        }
        true
    }

    /// Unit-stride loads (also fault-only-first) and stores of one register group, unmasked,
    /// when the whole group lies in one page that the TLB maps. Loads read the full group
    /// (harmless within the page) and keep lanes past vl; stores write only the bytes below
    /// vl. Anything else uses the helper.
    fn vector_memory(&mut self, cfg: Vconfig, insn: &Insn, index: u64, offset: i64) -> bool {
        let raw = insn.raw;
        let store = insn.op == Op::Vstore;
        let nf = (raw >> 29) & 7;
        let mew = (raw >> 28) & 1;
        let mop = (raw >> 26) & 3;
        let vm = (raw >> 25) & 1 == 1;
        let umop = (raw >> 20) & 0x1f;
        let eew: u32 = match (raw >> 12) & 7 {
            0 => 1,
            5 => 2,
            6 => 4,
            7 => 8,
            _ => return false,
        };
        let unit = umop == 0 || (umop == 0b10000 && !store);
        if nf != 0 || mew != 0 || mop != 0 || !vm || !unit {
            return false;
        }
        // EMUL = EEW / SEW * LMUL.
        let emul = cfg.lmul + eew.trailing_zeros() as i32 - cfg.sew.trailing_zeros() as i32;
        if !(-3..=3).contains(&emul) {
            return false;
        }
        // A fractional group is part of one register: storing the whole register would write
        // past it.
        if store && emul < 0 {
            return false;
        }
        let data = Vconfig { sew: eew, lmul: emul };
        let regs = data.regs();
        let vd = insn.rd;
        if !Self::aligned(vd, regs) {
            return false;
        }
        let bytes = regs * VLENB;
        let base = self.get(insn.rs1);
        let vl = self.load_vl();

        let fast = self.b.create_block();
        let host = self.b.append_block_param(fast, I64);
        let slow = self.b.create_block();
        let done = self.b.create_block();
        let access = if store { Access::Store } else { Access::Load };
        self.tlb_lookup(base, bytes.min(255) as u8, access, fast, slow);
        // tlb_lookup checks that `size` bytes fit in the page; groups larger than 255 bytes
        // cannot occur with 128-bit registers (8 x 16 = 128).
        debug_assert!(bytes <= 128);

        self.b.switch_to_block(fast);
        let guest = MemFlagsData::new().with_notrap();
        let ty = data.vector_type();
        if store {
            // Whole groups are stored a register at a time; a group cut short by vl byte by
            // byte, since nothing past vl may be written.
            let (whole, partial) = (self.b.create_block(), self.b.create_block());
            let full = self.b.ins().icmp_imm_s(IntCC::Equal, vl, cfg.vlmax() as i64);
            self.b.ins().brif(full, whole, &[], partial, &[]);
            self.b.switch_to_block(partial);
            let len = self.b.ins().imul_imm_s(vl, eew as i64);
            self.store_bytes_loop(vd as u32, host, len, done);
            self.b.switch_to_block(whole);
        }
        for r in 0..regs {
            let at = (r * VLENB) as i32;
            if store {
                let v = self.load_vreg(data, vd as u32 + r);
                self.b.ins().store(guest, v, host, at);
            } else {
                let v = self.b.ins().load(ty, guest, host, at);
                let old = self.load_vreg(data, vd as u32 + r);
                let body = self.body_mask(data, r, vl);
                let out = self.b.ins().bitselect(body, v, old);
                self.store_vreg(out, vd as u32 + r);
            }
        }
        self.b.ins().jump(done, &[]);

        self.b.switch_to_block(slow);
        self.exec_via_helper_branch(insn, index, offset, done);

        self.b.switch_to_block(done);
        true
    }
}
