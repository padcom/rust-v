//! Unit tests for behaviour that riscv-tests does not cover.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::csr::{self, envcfg, status};
use crate::mmu::Access;
use crate::trap::{cause, irq};
use crate::{Bus, Hart, HartConfig, HartShared, Priv, TimeSource};

const RAM: u64 = 0x8000_0000;

/// A clock the test controls.
#[derive(Default)]
struct ManualClock(AtomicU64);

impl TimeSource for ManualClock {
    fn mtime(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn hart() -> (Hart, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::default());
    let bus = Arc::new(Bus::new(RAM, 1 << 20));
    let mut hart = Hart::new(HartConfig::default(), bus, Arc::new(HartShared::default()), clock.clone());
    hart.reset(RAM);
    (hart, clock)
}

#[test]
fn pointer_masking_ignores_tag_bits() {
    let (mut hart, _) = hart();
    // U-mode with bare translation: PMLEN=7 zero-extends from bit 56.
    hart.csr_write(csr::SENVCFG, 2 << envcfg::PMM_SHIFT);
    hart.state.privilege = Priv::User;
    let tagged = (0x5a << 57) | (RAM + 0x100);
    hart.store(tagged, 8, 0x1122_3344_5566_7788).unwrap();
    assert_eq!(hart.load(RAM + 0x100, 8).unwrap(), 0x1122_3344_5566_7788);
    assert_eq!(hart.load(tagged, 8).unwrap(), 0x1122_3344_5566_7788);

    // Without masking the tagged address is outside RAM.
    hart.state.privilege = Priv::Machine;
    hart.csr_write(csr::SENVCFG, 0);
    hart.state.privilege = Priv::User;
    assert_eq!(hart.load(tagged, 8).unwrap_err().cause, cause::LOAD_ACCESS);
}

#[test]
fn pmm_reserved_value_is_ignored() {
    let (mut hart, _) = hart();
    hart.csr_write(csr::MENVCFG, 3 << envcfg::PMM_SHIFT);
    hart.csr_write(csr::MENVCFG, 1 << envcfg::PMM_SHIFT);
    assert_eq!(hart.state.csr.menvcfg & envcfg::PMM, 3 << envcfg::PMM_SHIFT);
}

#[test]
fn sstc_drives_stip() {
    let (mut hart, clock) = hart();
    let stip = 1 << irq::STI;
    let mip = |h: &Hart| h.shared.mip.load(Ordering::Relaxed);

    // Without STCE, stimecmp is not accessible from S-mode and STIP is software-controlled.
    hart.state.privilege = Priv::Supervisor;
    assert!(hart.csr_check(csr::STIMECMP, true, 0).is_err());
    hart.state.privilege = Priv::Machine;

    hart.csr_write(csr::MENVCFG, envcfg::STCE);
    hart.csr_write(csr::MCOUNTEREN, csr::COUNTEREN_TM);
    hart.csr_write(csr::STIMECMP, 100);
    assert_eq!(mip(&hart) & stip, 0);
    clock.0.store(100, Ordering::Relaxed);
    hart.update_timer_interrupts();
    assert_eq!(mip(&hart) & stip, stip, "time reached stimecmp");
    // Software cannot clear it through mip...
    hart.csr_write(csr::MIP, 0);
    assert_eq!(mip(&hart) & stip, stip);
    // ...only by moving stimecmp.
    hart.state.privilege = Priv::Supervisor;
    assert!(hart.csr_check(csr::STIMECMP, true, 0).is_ok());
    hart.csr_write(csr::STIMECMP, 200);
    assert_eq!(mip(&hart) & stip, 0);
    assert_eq!(hart.timer_deadline(), Some(200));
}

#[test]
fn stateen_gates_senvcfg() {
    let (mut hart, _) = hart();
    hart.state.privilege = Priv::Supervisor;
    assert!(hart.csr_check(csr::SENVCFG, false, 0).is_err());
    hart.state.privilege = Priv::Machine;
    hart.csr_write(csr::MSTATEEN0, u64::MAX);
    assert_eq!(hart.state.csr.mstateen[0], crate::csr::stateen::SE0 | crate::csr::stateen::ENVCFG);
    hart.state.privilege = Priv::Supervisor;
    assert!(hart.csr_check(csr::SENVCFG, false, 0).is_ok());
}

/// Builds a one-level-deep Sv39 mapping: root -> level-1 table -> level-0 leaf for `va`.
fn map_page(hart: &mut Hart, va: u64, leaf: u64) {
    let root = RAM + 0x1_0000;
    let l1 = RAM + 0x2_0000;
    let l0 = RAM + 0x3_0000;
    let ram = hart.bus.clone();
    let pte_ptr = |table: u64| ((table >> 12) << 10) | 1;
    ram.write(root + ((va >> 30) & 0x1ff) * 8, 8, pte_ptr(l1));
    ram.write(l1 + ((va >> 21) & 0x1ff) * 8, 8, pte_ptr(l0));
    ram.write(l0 + ((va >> 12) & 0x1ff) * 8, 8, leaf);
    hart.csr_write(csr::SATP, (8 << 60) | (root >> 12));
}

const LEAF_RWAD: u64 = 0b1100_0111; // D A - - - W R V

#[test]
fn svpbmt_requires_pbmte() {
    let (mut hart, _) = hart();
    let va = 0x4000_0000;
    let leaf = ((RAM + 0x5_0000) >> 12) << 10 | LEAF_RWAD | (1 << 62); // PBMT = IO
    map_page(&mut hart, va, leaf);
    assert_eq!(hart.translate(va, Access::Load, Priv::Supervisor).unwrap_err().cause, cause::LOAD_PAGE_FAULT);
    hart.csr_write(csr::MENVCFG, envcfg::PBMTE);
    assert_eq!(hart.translate(va + 8, Access::Load, Priv::Supervisor).unwrap(), RAM + 0x5_0008);
    // PBMT = 3 is reserved.
    map_page(&mut hart, va, leaf | (3 << 61));
    assert!(hart.translate(va, Access::Load, Priv::Supervisor).is_err());
}

#[test]
fn svnapot_64k_pages() {
    let (mut hart, _) = hart();
    let va = 0x4000_3000; // page 3 within a 64 KiB region
    let napot_ppn = ((RAM + 0x10_0000 - 0x1_0000) >> 12) | 0b1000;
    map_page(&mut hart, va, (napot_ppn << 10) | LEAF_RWAD | (1 << 63));
    assert_eq!(hart.translate(va + 4, Access::Load, Priv::Supervisor).unwrap(), RAM + 0xf_0000 + 0x3004);
    // Any other ppn[3:0] encoding is reserved.
    map_page(&mut hart, va, ((napot_ppn ^ 0b1100) << 10) | LEAF_RWAD | (1 << 63));
    assert!(hart.translate(va, Access::Load, Priv::Supervisor).is_err());
}

#[test]
fn fp_disabled_traps() {
    let (mut hart, _) = hart();
    let fadd = rv_isa::decode(0x003170d3);
    assert_eq!(hart.execute(&fadd).unwrap_err().cause, cause::ILLEGAL_INSN);
    hart.state.csr.mstatus |= status::FS;
    hart.execute(&fadd).unwrap();
}

#[test]
fn vector_smoke() {
    let (mut hart, _) = hart();
    hart.state.csr.mstatus |= status::VS;
    // vsetvli t0, a0, e32, m1, ta, ma; vle32.v v1, (a1); vadd.vv v2, v1, v1;
    // vredsum.vs v3, v1, v0; vse32.v v2, (a2); vmv.x.s a3, v3
    let program = [0x0d0572d7u32, 0x0205e087, 0x02108157, 0x021021d7, 0x02066127, 0x423026d7];
    for (i, w) in program.iter().enumerate() {
        hart.bus.write(RAM + 4 * i as u64, 4, *w as u64);
    }
    for (i, v) in [1u64, 2, 3, 4].iter().enumerate() {
        hart.bus.write(RAM + 0x100 + 4 * i as u64, 4, *v);
    }
    hart.state.x[10] = 100; // AVL larger than VLMAX
    hart.state.x[11] = RAM + 0x100;
    hart.state.x[12] = RAM + 0x200;
    for _ in 0..program.len() {
        hart.step();
    }
    assert_eq!(hart.state.pc, RAM + 4 * program.len() as u64, "no traps");
    assert_eq!(hart.state.x[5], 4, "vl = VLMAX = VLEN / SEW");
    let out: Vec<u64> = (0..4).map(|i| hart.bus.read(RAM + 0x200 + 4 * i, 4).unwrap()).collect();
    assert_eq!(out, [2, 4, 6, 8]);
    assert_eq!(hart.state.x[13], 10);
}

#[test]
fn vfrec7_register_group() {
    let (mut hart, _) = hart();
    hart.state.csr.mstatus |= status::VS | status::FS;
    let program = [0x0020d073u32, 0x00a2f357, 0x4e829257];
    for (i, w) in program.iter().enumerate() {
        hart.bus.write(RAM + 4 * i as u64, 4, *w as u64);
    }
    let input: [u64; 12] = [0xbe1d, 0, 0x8047, 1, 0xceff, 2, 0xb0be, 4, 0x4e07, 8, 0xfffc, 0x10];
    for (i, &v) in input.iter().enumerate() {
        hart.set_velem(8, i, 2, v);
    }
    hart.state.x[5] = 16;
    for _ in 0..program.len() {
        hart.step();
    }
    assert_eq!(
        hart.state.pc,
        RAM + 12,
        "trap: mcause {} mepc {:#x}",
        hart.state.csr.mcause,
        hart.state.csr.mepc
    );
    let got: Vec<u64> = (8..12).map(|i| hart.velem(4, i, 2)).collect();
    assert_eq!(got, [0x2950, 0x7bff, 0x7e00, 0x7bff]);
}
