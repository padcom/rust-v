//! Address translation and guest memory access.
//!
//! The software TLB maps a guest virtual page directly to a host pointer: for a hit,
//! `host = vaddr + entry.addend`. There is one table per (privilege, access type), so a hit
//! implies the access is permitted and no permission bits need checking on the fast path. This
//! is the layout the JIT will inline. Only RAM pages are ever entered, so MMIO always takes
//! the slow path.

use crate::csr::{satp, status};
use crate::hart::{Hart, Priv};
use crate::mem::{PAGE_SHIFT, PAGE_SIZE, host_load, host_store};
use crate::trap::{Exception, cause};

pub const TLB_ENTRIES: usize = 256;
const TAG_INVALID: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Load = 0,
    Store = 1,
    Fetch = 2,
}

impl Access {
    const fn page_fault(self) -> u64 {
        match self {
            Access::Load => cause::LOAD_PAGE_FAULT,
            Access::Store => cause::STORE_PAGE_FAULT,
            Access::Fetch => cause::INSN_PAGE_FAULT,
        }
    }

    const fn access_fault(self) -> u64 {
        match self {
            Access::Load => cause::LOAD_ACCESS,
            Access::Store => cause::STORE_ACCESS,
            Access::Fetch => cause::INSN_ACCESS,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct TlbEntry {
    /// Virtual page number, or `TAG_INVALID`.
    pub tag: u64,
    /// Host address of the page minus its guest virtual address.
    pub addend: u64,
}

/// Indexed as `entries[privilege][access][vpn % TLB_ENTRIES]`, privilege being U=0, S=1, M=2.
#[repr(C)]
pub struct Tlb {
    pub entries: [[[TlbEntry; TLB_ENTRIES]; 3]; 3],
}

impl Tlb {
    pub fn new() -> Self {
        Tlb { entries: [[[TlbEntry { tag: TAG_INVALID, addend: 0 }; TLB_ENTRIES]; 3]; 3] }
    }

    pub fn flush(&mut self) {
        for e in self.entries.as_flattened_mut().as_flattened_mut() {
            e.tag = TAG_INVALID;
        }
    }
}

impl Default for Tlb {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
const fn tlb_index(p: Priv) -> usize {
    match p {
        Priv::User => 0,
        Priv::Supervisor => 1,
        Priv::Machine => 2,
    }
}

#[inline(always)]
const fn crosses_page(vaddr: u64, size: u8) -> bool {
    (vaddr & (PAGE_SIZE - 1)) + size as u64 > PAGE_SIZE
}

impl Hart {
    /// Effective privilege for loads and stores (takes `mstatus.MPRV` into account).
    #[inline(always)]
    pub fn data_priv(&self) -> Priv {
        let s = &*self.state;
        if s.privilege == Priv::Machine && s.csr.mstatus & status::MPRV != 0 {
            Priv::from_bits(s.csr.mstatus >> status::MPP_SHIFT)
        } else {
            s.privilege
        }
    }

    #[inline(always)]
    fn tlb_lookup(&self, p: Priv, access: Access, vaddr: u64) -> Option<*mut u8> {
        let vpn = vaddr >> PAGE_SHIFT;
        let e = &self.state.tlb.entries[tlb_index(p)][access as usize][vpn as usize % TLB_ENTRIES];
        (e.tag == vpn).then(|| vaddr.wrapping_add(e.addend) as *mut u8)
    }

    /// Enters the page containing `vaddr` -> `paddr` into the TLB if it is RAM.
    fn tlb_fill(&mut self, p: Priv, access: Access, vaddr: u64, paddr: u64) {
        let page = paddr & !(PAGE_SIZE - 1);
        if let Some(host) = self.bus.ram_ptr(page, PAGE_SIZE) {
            let vpn = vaddr >> PAGE_SHIFT;
            self.state.tlb.entries[tlb_index(p)][access as usize][vpn as usize % TLB_ENTRIES] =
                TlbEntry { tag: vpn, addend: (host as u64).wrapping_sub(vaddr & !(PAGE_SIZE - 1)) };
        }
    }

    /// Translates a virtual address for an access at privilege `p`.
    pub fn translate(&mut self, vaddr: u64, access: Access, p: Priv) -> Result<u64, Exception> {
        let mode = self.state.csr.satp >> satp::MODE_SHIFT;
        if p == Priv::Machine || mode == satp::MODE_BARE {
            return Ok(vaddr);
        }
        self.walk(vaddr, access, p, mode)
    }

    fn walk(&mut self, vaddr: u64, access: Access, p: Priv, mode: u64) -> Result<u64, Exception> {
        let levels = match mode {
            satp::MODE_SV39 => 3,
            satp::MODE_SV48 => 4,
            _ => 5,
        };
        let page_fault = Exception::new(access.page_fault(), vaddr);

        // The address must be sign-extended from the top implemented bit.
        let unused = 64 - (PAGE_SHIFT + 9 * levels);
        if (((vaddr << unused) as i64) >> unused) as u64 != vaddr {
            return Err(page_fault);
        }

        let mstatus = self.state.csr.mstatus;
        let sum = mstatus & status::SUM != 0;
        let mxr = mstatus & status::MXR != 0;
        let mut table = (self.state.csr.satp & satp::PPN_MASK) << PAGE_SHIFT;

        for level in (0..levels).rev() {
            let vpn = (vaddr >> (PAGE_SHIFT + 9 * level)) & 0x1ff;
            let pte_addr = table + vpn * 8;
            let Some(off) = self.bus.ram_offset(pte_addr, 8) else {
                return Err(Exception::new(access.access_fault(), vaddr));
            };
            let pte = self.bus.ram().load(off, 8);

            let (v, r, w, x, u, a, d) = (
                pte & 1 != 0,
                pte & 2 != 0,
                pte & 4 != 0,
                pte & 8 != 0,
                pte & 0x10 != 0,
                pte & 0x40 != 0,
                pte & 0x80 != 0,
            );
            // Bits 63:54 are reserved until Svpbmt/Svnapot are implemented.
            if !v || (!r && w) || pte >> 54 != 0 {
                return Err(page_fault);
            }
            let ppn = (pte >> 10) & satp::PPN_MASK;

            if !r && !x {
                // Pointer to the next level.
                if level == 0 {
                    return Err(page_fault);
                }
                table = ppn << PAGE_SHIFT;
                continue;
            }

            let permitted = match access {
                Access::Fetch => x,
                Access::Load => r || (mxr && x),
                Access::Store => w,
            };
            let priv_ok = match p {
                Priv::User => u,
                Priv::Supervisor => !u || (sum && access != Access::Fetch),
                Priv::Machine => true,
            };
            if !permitted || !priv_ok {
                return Err(page_fault);
            }
            // Superpages must be aligned.
            let low_ppn_mask = (1u64 << (9 * level)) - 1;
            if ppn & low_ppn_mask != 0 {
                return Err(page_fault);
            }
            // Svade: A/D are managed by software.
            if !a || (access == Access::Store && !d) {
                return Err(page_fault);
            }
            let offset_mask = (1u64 << (PAGE_SHIFT + 9 * level)) - 1;
            return Ok(((ppn << PAGE_SHIFT) & !offset_mask) | (vaddr & offset_mask));
        }
        unreachable!("loop returns at level 0")
    }

    // ---- Loads and stores ----

    /// Loads `size` bytes from virtual address `vaddr`, zero-extended.
    #[inline]
    pub fn load(&mut self, vaddr: u64, size: u8) -> Result<u64, Exception> {
        let p = self.data_priv();
        if !crosses_page(vaddr, size) {
            if let Some(host) = self.tlb_lookup(p, Access::Load, vaddr) {
                // SAFETY: TLB entries only ever point at whole pages of guest RAM.
                return Ok(unsafe { host_load(host, size) });
            }
            return self.load_slow(vaddr, size, p);
        }
        let mut value = 0;
        for i in 0..size as u64 {
            value |= self.load_slow(vaddr.wrapping_add(i), 1, p)? << (8 * i);
        }
        Ok(value)
    }

    fn load_slow(&mut self, vaddr: u64, size: u8, p: Priv) -> Result<u64, Exception> {
        let paddr = self.translate(vaddr, Access::Load, p)?;
        if let Some(off) = self.bus.ram_offset(paddr, size as u64) {
            self.tlb_fill(p, Access::Load, vaddr, paddr);
            return Ok(self.bus.ram().load(off, size));
        }
        self.bus.read(paddr, size).ok_or(Exception::new(cause::LOAD_ACCESS, vaddr))
    }

    /// Stores the low `size` bytes of `value` at virtual address `vaddr`.
    #[inline]
    pub fn store(&mut self, vaddr: u64, size: u8, value: u64) -> Result<(), Exception> {
        let p = self.data_priv();
        if !crosses_page(vaddr, size) {
            if let Some(host) = self.tlb_lookup(p, Access::Store, vaddr) {
                // SAFETY: TLB entries only ever point at whole pages of guest RAM.
                unsafe { host_store(host, size, value) };
                return Ok(());
            }
            let paddr = self.translate(vaddr, Access::Store, p)?;
            return self.store_phys(vaddr, paddr, size, value, Some(p));
        }
        // Translate both pages before writing anything so a fault leaves memory untouched.
        let second = (vaddr | (PAGE_SIZE - 1)).wrapping_add(1);
        let first_paddr = self.translate(vaddr, Access::Store, p)?;
        let second_paddr = self.translate(second, Access::Store, p)?;
        for i in 0..size as u64 {
            let va = vaddr.wrapping_add(i);
            let pa = if va < second { first_paddr + i } else { second_paddr + (va - second) };
            self.store_phys(va, pa, 1, value >> (8 * i), None)?;
        }
        Ok(())
    }

    fn store_phys(
        &mut self,
        vaddr: u64,
        paddr: u64,
        size: u8,
        value: u64,
        fill: Option<Priv>,
    ) -> Result<(), Exception> {
        if let Some(off) = self.bus.ram_offset(paddr, size as u64) {
            if let Some(p) = fill {
                self.tlb_fill(p, Access::Store, vaddr, paddr);
            }
            self.bus.ram().store(off, size, value);
            return Ok(());
        }
        if self.bus.write(paddr, size, value) {
            Ok(())
        } else {
            Err(Exception::new(cause::STORE_ACCESS, vaddr))
        }
    }

    /// Fetches the 16-bit instruction parcel at `vaddr`.
    #[inline]
    pub fn fetch16(&mut self, vaddr: u64) -> Result<u16, Exception> {
        let p = self.state.privilege;
        if let Some(host) = self.tlb_lookup(p, Access::Fetch, vaddr) {
            // SAFETY: TLB entries only ever point at whole pages of guest RAM.
            return Ok(unsafe { host_load(host, 2) } as u16);
        }
        let paddr = self.translate(vaddr, Access::Fetch, p)?;
        // Instructions can only be fetched from RAM.
        let Some(off) = self.bus.ram_offset(paddr, 2) else {
            return Err(Exception::new(cause::INSN_ACCESS, vaddr));
        };
        self.tlb_fill(p, Access::Fetch, vaddr, paddr);
        Ok(self.bus.ram().load(off, 2) as u16)
    }

    /// Resolves an atomic access (LR, SC, AMO) to a host pointer and physical address.
    ///
    /// Atomics must be naturally aligned and target RAM.
    pub(crate) fn atomic_target(
        &mut self,
        vaddr: u64,
        size: u8,
        access: Access,
    ) -> Result<(*mut u8, u64), Exception> {
        let (misaligned, fault) = match access {
            Access::Load => (cause::LOAD_MISALIGNED, cause::LOAD_ACCESS),
            _ => (cause::STORE_MISALIGNED, cause::STORE_ACCESS),
        };
        if vaddr & (size as u64 - 1) != 0 {
            return Err(Exception::new(misaligned, vaddr));
        }
        let p = self.data_priv();
        let paddr = self.translate(vaddr, access, p)?;
        match self.bus.ram_ptr(paddr, size as u64) {
            Some(ptr) => Ok((ptr, paddr)),
            None => Err(Exception::new(fault, vaddr)),
        }
    }
}
