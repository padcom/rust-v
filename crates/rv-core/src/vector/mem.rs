//! Vector loads and stores.

use rv_isa::{Insn, Op};

use super::{group_len, mask};
use crate::hart::Hart;
use crate::mem::{PAGE_SIZE, host_read_bytes, host_write_bytes};
use crate::mmu::Access;
use crate::trap::Exception;

#[inline(always)]
const fn field(raw: u32, hi: u32, lo: u32) -> u32 {
    (raw >> lo) & ((1 << (hi - lo + 1)) - 1)
}

/// Addressing mode of a vector memory access.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unit,
    Strided(u64),
    /// Index register group and the index element width in bytes.
    Indexed(u8, usize),
}

impl Hart {
    pub(crate) fn exec_vmem(&mut self, insn: &Insn) -> Result<(), Exception> {
        let raw = insn.raw;
        let store = insn.op == Op::Vstore;
        let nf = field(raw, 31, 29) as usize + 1;
        let mew = field(raw, 28, 28);
        let mop = field(raw, 27, 26);
        let vm = field(raw, 25, 25) == 1;
        let umop = field(raw, 24, 20);
        let vd = insn.rd;
        let eew = match field(raw, 14, 12) {
            0 => 1,
            5 => 2,
            6 => 4,
            _ => 8,
        };
        if mew != 0 {
            return Err(Exception::illegal(raw));
        }
        let base = self.x(insn.rs1);

        // Whole-register accesses ignore vtype and vl.
        if mop == 0 && umop == 0b01000 {
            if !vm || !matches!(nf, 1 | 2 | 4 | 8) || (store && eew != 1) {
                return Err(Exception::illegal(raw));
            }
            if !(vd as usize).is_multiple_of(nf) {
                return Err(Exception::illegal(raw));
            }
            let evl = nf * self.vlenb() / eew;
            return self.vmem_elements(store, base, Mode::Unit, eew, (1, 0), vd, evl, true, false);
        }

        let vt = self.vtype(raw)?;
        let vl = self.state.csr.vl as usize;
        if mop == 0 && umop == 0b01011 {
            // vlm.v / vsm.v: a mask, accessed as bytes.
            if !vm || nf != 1 || eew != 1 {
                return Err(Exception::illegal(raw));
            }
            return self.vmem_elements(store, base, Mode::Unit, 1, (1, 0), vd, vl.div_ceil(8), true, false);
        }

        let (mode, data_eew) = match mop {
            0 => {
                let ok = umop == 0 || (umop == 0b10000 && !store);
                if !ok {
                    return Err(Exception::illegal(raw));
                }
                (Mode::Unit, eew)
            }
            2 => (Mode::Strided(self.x(insn.rs2)), eew),
            _ => (Mode::Indexed(insn.rs2, eew), vt.sew),
        };
        // EMUL = EEW / SEW * LMUL for the data (or for the index, when indexed).
        let log2 = |n: usize| n.trailing_zeros() as i32;
        let emul = vt.lmul + log2(data_eew) - log2(vt.sew);
        self.vcheck_group(vd, emul, raw)?;
        let regs = group_len(emul);
        if nf * regs > 8 || vd as usize + nf * regs > 32 {
            return Err(Exception::illegal(raw));
        }
        if let Mode::Indexed(vs2, ieew) = mode {
            self.vcheck_group(vs2, vt.lmul + log2(ieew) - log2(vt.sew), raw)?;
        }
        // A masked load must not overwrite the mask.
        if !store && !vm && vd == 0 {
            return Err(Exception::illegal(raw));
        }
        let fault_only_first = mop == 0 && umop == 0b10000;
        self.vmem_elements(store, base, mode, data_eew, (nf, regs), vd, vl, vm, fault_only_first)
    }

    /// The element loop shared by all vector memory accesses. `fields` is the segment field
    /// count and the number of registers between consecutive fields.
    fn vmem_elements(
        &mut self,
        store: bool,
        base: u64,
        mode: Mode,
        eew: usize,
        fields: (usize, usize),
        vd: u8,
        evl: usize,
        vm: bool,
        fault_only_first: bool,
    ) -> Result<(), Exception> {
        let (nf, field_regs) = fields;
        let mut start = self.state.csr.vstart as usize;
        if mode == Mode::Unit && nf == 1 && vm {
            // Contiguous and unmasked: copy a page at a time between RAM and the register group.
            // Whatever is left (MMIO, an element straddling pages, stores to code pages) goes
            // through the element loop below.
            let access = if store { Access::Store } else { Access::Load };
            let p = self.data_priv();
            let vlenb = self.vlenb();
            while start < evl {
                let addr = self.mask_pointer(base.wrapping_add((start * eew) as u64), p);
                let room = (PAGE_SIZE - (addr & (PAGE_SIZE - 1))) as usize;
                let n = (room / eew).min(evl - start);
                if n == 0 {
                    break;
                }
                let host = match self.data_page_ptr(addr, access, p) {
                    Ok(Some(host)) => host,
                    Ok(None) => break,
                    Err(e) => {
                        if fault_only_first && start > 0 {
                            self.state.csr.vl = start as u64;
                            return Ok(());
                        }
                        self.state.csr.vstart = start as u64;
                        return Err(e);
                    }
                };
                let off = vd as usize * vlenb + start * eew;
                let regs = &mut self.state.v[off..off + n * eew];
                // SAFETY: the TLB maps whole pages of RAM, and `n` elements fit in this one.
                unsafe {
                    if store {
                        host_write_bytes(host, regs);
                    } else {
                        host_read_bytes(host, regs);
                    }
                }
                start += n;
            }
        }
        for i in start..evl {
            if !self.vactive(vm, i) {
                continue;
            }
            for f in 0..nf {
                let offset = (f * eew) as u64;
                let addr = match mode {
                    Mode::Unit => base.wrapping_add(((i * nf + f) * eew) as u64),
                    Mode::Strided(stride) => {
                        base.wrapping_add((i as u64).wrapping_mul(stride)).wrapping_add(offset)
                    }
                    Mode::Indexed(vs2, ieew) => {
                        base.wrapping_add(self.velem(vs2, i, ieew)).wrapping_add(offset)
                    }
                };
                let reg = vd + (f * field_regs) as u8;
                let result = if store {
                    let v = self.velem(reg, i, eew);
                    self.store(addr, eew as u8, v)
                } else {
                    self.load(addr, eew as u8).map(|v| self.set_velem(reg, i, eew, v & mask(eew)))
                };
                if let Err(e) = result {
                    if fault_only_first && i > 0 {
                        // Trim vl instead of trapping.
                        self.state.csr.vl = i as u64;
                        return Ok(());
                    }
                    self.state.csr.vstart = i as u64;
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}
