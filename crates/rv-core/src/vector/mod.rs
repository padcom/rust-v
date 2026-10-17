//! RVV 1.0 vector unit, with Zvfh/Zvfhmin and Zvbb.
//!
//! The register file is a flat byte array (`HartState::v`), so a register group is simply a
//! contiguous range and element `i` of a group starting at `vN` lives at byte
//! `N * VLENB + i * EEW`. Tail and inactive elements are always left undisturbed, which the
//! specification allows for both agnostic and undisturbed policies.

mod alu;
mod fp;
mod mem;

use rv_isa::{Insn, Op};

use crate::csr::status;
use crate::hart::Hart;
use crate::trap::Exception;

/// Largest supported VLEN, in bytes.
pub const MAX_VLENB: usize = 64;
/// ELEN, in bytes.
const ELENB: usize = 8;

/// Decoded `vtype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Vtype {
    /// Selected element width, in bytes.
    pub sew: usize,
    /// log2(LMUL), -3..=3.
    pub lmul: i32,
}

pub(crate) const VTYPE_VILL: u64 = 1 << 63;

/// Validates a `vtype` value as written by `vsetvl{i}`. `None` if it would set `vill`.
/// VLMAX for a `vtype` value with `vlenb`-byte registers, or `None` if the value sets
/// `vill`. Lets compiled code execute VSETVLI/VSETIVLI with an immediate `vtype` natively.
pub fn vlmax_for(vtype: u64, vlenb: usize) -> Option<u64> {
    let vt = decode_vtype(vtype)?;
    let base = vlenb / vt.sew;
    Some(if vt.lmul >= 0 { base << vt.lmul } else { base >> -vt.lmul } as u64)
}

fn decode_vtype(value: u64) -> Option<Vtype> {
    // Only vma, vta, vsew and vlmul may be set.
    if value >> 8 != 0 {
        return None;
    }
    let vsew = (value >> 3) & 7;
    let lmul = match value & 7 {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        5 => -3,
        6 => -2,
        7 => -1,
        _ => return None,
    };
    if vsew > 3 {
        return None;
    }
    let sew = 1usize << vsew;
    // Fractional LMUL must still fit one SEW element in an ELEN-wide slice.
    if lmul < 0 && sew > ELENB >> -lmul {
        return None;
    }
    Some(Vtype { sew, lmul })
}

/// Number of registers in a group with log2(EMUL) `emul`.
pub(crate) const fn group_len(emul: i32) -> usize {
    if emul > 0 { 1 << emul } else { 1 }
}

impl Hart {
    #[inline(always)]
    pub fn vlenb(&self) -> usize {
        self.config.vlen / 8
    }

    pub(crate) fn vector_enabled(&self) -> bool {
        self.state.csr.mstatus & status::VS != 0
    }

    fn vector_dirty(&mut self) {
        self.state.csr.mstatus |= status::VS;
    }

    /// The current `vtype`, or an illegal-instruction exception if `vill` is set.
    pub(crate) fn vtype(&self, raw: u32) -> Result<Vtype, Exception> {
        let vt = self.state.csr.vtype;
        if vt & VTYPE_VILL != 0 {
            return Err(Exception::illegal(raw));
        }
        Ok(decode_vtype(vt).expect("vtype is validated when written"))
    }

    pub(crate) fn vlmax(&self, sew: usize, lmul: i32) -> usize {
        let base = self.vlenb() / sew;
        if lmul >= 0 { base << lmul } else { base >> -lmul }
    }

    /// Element `i` (of width `eew` bytes) of the register group starting at `reg`.
    #[inline(always)]
    pub(crate) fn velem(&self, reg: u8, i: usize, eew: usize) -> u64 {
        let off = reg as usize * self.vlenb() + i * eew;
        let mut v = 0u64;
        for (k, &b) in self.state.v[off..off + eew].iter().enumerate() {
            v |= (b as u64) << (8 * k);
        }
        v
    }

    #[inline(always)]
    pub(crate) fn set_velem(&mut self, reg: u8, i: usize, eew: usize, value: u64) {
        let off = reg as usize * self.vlenb() + i * eew;
        for (k, b) in self.state.v[off..off + eew].iter_mut().enumerate() {
            *b = (value >> (8 * k)) as u8;
        }
    }

    /// Bit `i` of mask register `reg`.
    #[inline(always)]
    pub(crate) fn vbit(&self, reg: u8, i: usize) -> bool {
        self.state.v[reg as usize * self.vlenb() + i / 8] >> (i % 8) & 1 != 0
    }

    #[inline(always)]
    pub(crate) fn set_vbit(&mut self, reg: u8, i: usize, bit: bool) {
        let b = &mut self.state.v[reg as usize * self.vlenb() + i / 8];
        *b = (*b & !(1 << (i % 8))) | ((bit as u8) << (i % 8));
    }

    /// Whether element `i` is active under the instruction's mask (`vm` = unmasked).
    #[inline(always)]
    pub(crate) fn vactive(&self, vm: bool, i: usize) -> bool {
        vm || self.vbit(0, i)
    }

    /// Checks that `reg` is a legal base for a group with log2(EMUL) `emul`.
    pub(crate) fn vcheck_group(&self, reg: u8, emul: i32, raw: u32) -> Result<(), Exception> {
        if !(-3..=3).contains(&emul) || !(reg as usize).is_multiple_of(group_len(emul)) {
            return Err(Exception::illegal(raw));
        }
        Ok(())
    }

    /// Executes a vector instruction.
    pub(crate) fn execute_vector(&mut self, insn: &Insn) -> Result<(), Exception> {
        if !self.vector_enabled() {
            return Err(Exception::illegal(insn.raw));
        }
        profile::count(insn);
        match insn.op {
            Op::Vsetvli | Op::Vsetivli | Op::Vsetvl => self.exec_vset(insn),
            Op::Vload | Op::Vstore => self.exec_vmem(insn)?,
            _ => self.exec_opv(insn)?,
        }
        self.state.csr.vstart = 0;
        self.vector_dirty();
        Ok(())
    }

    fn exec_vset(&mut self, insn: &Insn) {
        let (avl, value) = match insn.op {
            Op::Vsetvli => (None, insn.imm as u64 & 0x7ff),
            Op::Vsetivli => (Some(insn.rs1 as u64), insn.imm as u64 & 0x3ff),
            _ => (None, self.x(insn.rs2)),
        };
        let Some(vt) = decode_vtype(value) else {
            self.state.csr.vtype = VTYPE_VILL;
            self.state.csr.vl = 0;
            self.set_x(insn.rd, 0);
            return;
        };
        let vlmax = self.vlmax(vt.sew, vt.lmul) as u64;
        let vl = match avl {
            Some(avl) => avl.min(vlmax),
            None if insn.rs1 != 0 => self.x(insn.rs1).min(vlmax),
            None if insn.rd != 0 => vlmax,
            // rd = rs1 = x0: change vtype, keep vl.
            None => self.state.csr.vl.min(vlmax),
        };
        self.state.csr.vtype = value;
        self.state.csr.vl = vl;
        self.set_x(insn.rd, vl);
    }

    /// Reads `f[rs1]` as a SEW-wide scalar operand (NaN-boxing checked), for `.vf` forms.
    pub(crate) fn fscalar(&self, r: u8, sew: usize) -> u64 {
        let v = self.state.f[r as usize];
        match sew {
            8 => v,
            4 if v >> 32 == 0xffff_ffff => v & 0xffff_ffff,
            4 => rv_softfloat::canonical_nan::<rv_softfloat::F32>(),
            _ if v >> 16 == 0xffff_ffff_ffff => v & 0xffff,
            _ => rv_softfloat::canonical_nan::<rv_softfloat::F16>(),
        }
    }
}

#[inline(always)]
pub(crate) const fn mask(sew: usize) -> u64 {
    if sew >= 8 { u64::MAX } else { (1u64 << (8 * sew)) - 1 }
}

/// Sign-extends a SEW-wide value.
#[inline(always)]
pub(crate) const fn sext(v: u64, sew: usize) -> i64 {
    let shift = 64 - 8 * sew as u32;
    ((v << shift) as i64) >> shift
}

/// Counts of executed vector instructions by kind, collected when `RV_VECTOR_PROFILE` is set
/// (to see which ones deserve native code).
pub mod profile {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use rv_isa::{Insn, Op};

    /// Arithmetic (by funct3 and funct6), then loads and stores (by mop and width), then vset*.
    const KINDS: usize = 8 * 64 + 2 * 4 * 8 + 1;

    struct Profile {
        counts: Box<[AtomicU64]>,
        examples: Box<[AtomicU32]>,
    }

    fn profile() -> Option<&'static Profile> {
        static PROFILE: OnceLock<Option<Profile>> = OnceLock::new();
        PROFILE
            .get_or_init(|| {
                std::env::var_os("RV_VECTOR_PROFILE").map(|_| Profile {
                    counts: (0..KINDS).map(|_| AtomicU64::new(0)).collect(),
                    examples: (0..KINDS).map(|_| AtomicU32::new(0)).collect(),
                })
            })
            .as_ref()
    }

    fn kind(insn: &Insn) -> usize {
        let raw = insn.raw;
        match insn.op {
            Op::Vload | Op::Vstore => {
                let store = (insn.op == Op::Vstore) as usize;
                let mop = ((raw >> 26) & 3) as usize;
                8 * 64 + store * 32 + mop * 8 + ((raw >> 12) & 7) as usize
            }
            Op::Vsetvli | Op::Vsetivli | Op::Vsetvl => KINDS - 1,
            _ => ((raw >> 12) & 7) as usize * 64 + (raw >> 26) as usize,
        }
    }

    #[inline]
    pub(crate) fn count(insn: &Insn) {
        if let Some(p) = profile() {
            let k = kind(insn);
            p.counts[k].fetch_add(1, Ordering::Relaxed);
            p.examples[k].store(insn.raw, Ordering::Relaxed);
        }
    }

    /// The most executed kinds, with an example instruction of each.
    pub fn report(top: usize) -> Option<String> {
        let p = profile()?;
        let mut rows: Vec<(u64, u32)> = (0..KINDS)
            .map(|k| (p.counts[k].load(Ordering::Relaxed), p.examples[k].load(Ordering::Relaxed)))
            .filter(|&(n, _)| n > 0)
            .collect();
        rows.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let total: u64 = rows.iter().map(|r| r.0).sum();
        let mut out = format!("vector instructions: {total}\n");
        for (n, raw) in rows.into_iter().take(top) {
            out +=
                &format!("{:6.2}% {n:12}  e.g. {}\n", n as f64 * 100.0 / total as f64, rv_isa::decode(raw));
        }
        Some(out)
    }
}
