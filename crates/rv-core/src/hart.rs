//! Hart architectural state, traps and CSR access.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::block::BlockCache;
use crate::bus::Bus;
use crate::csr::{self, envcfg, misa, satp, stateen, status};
use crate::jit::BlockCompiler;
use crate::mmu::Tlb;
use crate::trap::{Exception, INTERRUPT_BIT, cause, irq};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priv {
    User = 0,
    Supervisor = 1,
    Machine = 3,
}

impl Priv {
    pub const fn from_bits(bits: u64) -> Priv {
        match bits & 3 {
            0 => Priv::User,
            1 => Priv::Supervisor,
            _ => Priv::Machine,
        }
    }
}

/// Source of the platform timer (`mtime`), shared by all harts and the timer device.
pub trait TimeSource: Send + Sync {
    fn mtime(&self) -> u64;
}

/// `mtime` derived from the host's monotonic clock.
pub struct HostClock {
    start: Instant,
    freq_hz: u64,
    /// Added to the elapsed time (so that a restored machine's time continues where it was).
    base: AtomicU64,
}

impl HostClock {
    pub fn new(freq_hz: u64) -> Self {
        HostClock { start: Instant::now(), freq_hz, base: AtomicU64::new(0) }
    }

    fn elapsed(&self) -> u64 {
        (self.start.elapsed().as_nanos() * self.freq_hz as u128 / 1_000_000_000) as u64
    }

    /// Makes the clock read `mtime` now, counting on from there.
    pub fn set(&self, mtime: u64) {
        self.base.store(mtime.wrapping_sub(self.elapsed()), Ordering::SeqCst);
    }
}

impl TimeSource for HostClock {
    fn mtime(&self) -> u64 {
        self.base.load(Ordering::Relaxed).wrapping_add(self.elapsed())
    }
}

/// Plain CSR storage. Fields with WARL constraints are kept legal by [`Hart::csr_write`].
#[derive(Clone, Debug)]
#[repr(C)]
pub struct Csrs {
    /// `mstatus` without the read-only UXL/SXL/SD fields (those are composed on read).
    pub mstatus: u64,
    pub misa: u64,
    pub medeleg: u64,
    pub mideleg: u64,
    pub mie: u64,
    pub mtvec: u64,
    pub mcounteren: u64,
    pub menvcfg: u64,
    pub mcountinhibit: u64,
    pub mscratch: u64,
    pub mepc: u64,
    pub mcause: u64,
    pub mtval: u64,
    pub stvec: u64,
    pub scounteren: u64,
    pub senvcfg: u64,
    pub sscratch: u64,
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub satp: u64,
    pub mcycle: u64,
    pub minstret: u64,
    /// `frm` in bits 7:5, `fflags` in bits 4:0.
    pub fcsr: u64,
    pub stimecmp: u64,
    pub mstateen: [u64; 4],
    pub vstart: u64,
    pub vxsat: u64,
    pub vxrm: u64,
    pub vl: u64,
    pub vtype: u64,
    pub pmpcfg: [u64; 8],
    pub pmpaddr: [u64; 64],
}

impl Default for Csrs {
    fn default() -> Self {
        Csrs {
            mstatus: 0,
            misa: 0,
            medeleg: 0,
            mideleg: 0,
            mie: 0,
            mtvec: 0,
            mcounteren: 0,
            menvcfg: 0,
            mcountinhibit: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            stvec: 0,
            scounteren: 0,
            senvcfg: 0,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            satp: 0,
            mcycle: 0,
            minstret: 0,
            fcsr: 0,
            stimecmp: u64::MAX,
            mstateen: [0; 4],
            vstart: 0,
            vxsat: 0,
            vxrm: 0,
            vl: 0,
            vtype: crate::vector::VTYPE_VILL,
            pmpcfg: [0; 8],
            pmpaddr: [0; 64],
        }
    }
}

/// Architectural state of a hart that the interpreter and JIT-compiled code operate on.
///
/// `#[repr(C)]` so that generated code can address fields at fixed offsets.
#[repr(C)]
pub struct HartState {
    pub x: [u64; 32],
    pub pc: u64,
    pub f: [u64; 32],
    /// Vector registers v0..v31, `VLENB` bytes each (the array is sized for the maximum).
    pub v: [u8; 32 * crate::vector::MAX_VLENB],
    pub privilege: Priv,
    pub csr: Csrs,
    pub tlb: Tlb,
    pub jit: crate::jit::JitState,
}

/// The part of a hart that other threads (devices, other harts) touch.
pub struct HartShared {
    /// Hardware-driven and software-written interrupt-pending bits.
    pub mip: AtomicU64,
    /// Non-zero when the hart should leave its execution loop and look at `mip` & co.
    pub exit_request: AtomicU32,
    wake_lock: Mutex<bool>,
    wake: Condvar,
}

impl Default for HartShared {
    fn default() -> Self {
        HartShared {
            mip: AtomicU64::new(0),
            exit_request: AtomicU32::new(0),
            wake_lock: Mutex::new(false),
            wake: Condvar::new(),
        }
    }
}

impl HartShared {
    /// Raises or lowers interrupt-pending bits and makes the hart notice.
    pub fn set_irq(&self, bits: u64, level: bool) {
        if level {
            self.mip.fetch_or(bits, Ordering::SeqCst);
        } else {
            self.mip.fetch_and(!bits, Ordering::SeqCst);
        }
        self.kick();
    }

    /// Asks the hart to leave its execution loop, waking it from WFI.
    pub fn kick(&self) {
        self.exit_request.store(1, Ordering::SeqCst);
        let mut kicked = self.wake_lock.lock().unwrap();
        *kicked = true;
        self.wake.notify_all();
    }

    /// Blocks until [`kick`](Self::kick) is called or `timeout` elapses.
    pub fn wait(&self, timeout: Duration) {
        let kicked = self.wake_lock.lock().unwrap();
        let (mut kicked, _) = self.wake.wait_timeout_while(kicked, timeout, |k| !*k).unwrap();
        *kicked = false;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HartConfig {
    pub hartid: u64,
    /// Most capable `satp` mode supported (8 = Sv39, 9 = Sv48, 10 = Sv57).
    pub max_satp_mode: u64,
    /// Vector register length in bits: a power of two from 128 to `8 * MAX_VLENB`.
    pub vlen: usize,
}

impl Default for HartConfig {
    fn default() -> Self {
        HartConfig { hartid: 0, max_satp_mode: satp::MODE_SV57, vlen: 128 }
    }
}

/// An LR reservation. SC is implemented as a compare-and-swap against the value LR observed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reservation {
    pub paddr: u64,
    pub size: u8,
    pub value: u64,
}

pub struct Hart {
    pub state: Box<HartState>,
    pub shared: Arc<HartShared>,
    pub bus: Arc<Bus>,
    pub config: HartConfig,
    time: Arc<dyn TimeSource>,
    pub(crate) reservation: Option<Reservation>,
    /// Set by WFI; cleared once an interrupt becomes pending.
    pub wfi: bool,
    /// Set when an instruction wrote `minstret` so that its own retirement is not counted.
    pub(crate) suppress_instret: bool,
    pub(crate) blocks: BlockCache,
    /// Native-code compiler for hot blocks, if enabled.
    pub compiler: Option<Box<dyn BlockCompiler>>,
    /// Interpretations of a block before it is handed to the compiler.
    pub jit_threshold: u32,
    /// Background compilations not yet installed.
    pub(crate) jit_pending: usize,
    /// Incremented whenever the code cache is flushed.
    pub(crate) jit_cache_epoch: u32,
    /// Exception raised inside a compiled block, picked up by `run_compiled`.
    pub(crate) jit_exception: Option<Exception>,
    /// Pointer-masking length (PMLEN) for data accesses at U, S and M; 0 when disabled.
    pub(crate) pm_len: [u8; 3],
}

const MISA: u64 = misa::MXL_64
    | misa::ext(b'I')
    | misa::ext(b'M')
    | misa::ext(b'A')
    | misa::ext(b'C')
    | misa::ext(b'D')
    | misa::ext(b'F')
    | misa::ext(b'S')
    | misa::ext(b'U')
    | misa::ext(b'V');

// `misa` cannot express multi-letter extensions; those are advertised through the device tree.

/// SEIP is left out: it follows the PLIC's line only. Were it writable, a CSRRS/CSRRC of `mip`
/// (OpenSBI sets SSIP that way for every S-mode IPI) that read it as 0 just before the PLIC
/// raised it would write the 0 back; the PLIC, whose output had not changed, would never raise
/// it again, and the hart would stop taking external interrupts. (The architecture's software
/// SEIP bit, which CSRRS/CSRRC see on their own, is not needed by OpenSBI or Linux.)
const MIP_M_WRITABLE: u64 = (1 << irq::SSI) | (1 << irq::STI) | (1 << irq::LCOFI);
const MIE_WRITABLE: u64 = (1 << irq::SSI)
    | (1 << irq::MSI)
    | (1 << irq::STI)
    | (1 << irq::MTI)
    | (1 << irq::SEI)
    | (1 << irq::MEI)
    | (1 << irq::LCOFI);
const MIDELEG_WRITABLE: u64 = (1 << irq::SSI) | (1 << irq::STI) | (1 << irq::SEI) | (1 << irq::LCOFI);
/// All synchronous exceptions except ECALL from M-mode (and reserved causes).
const MEDELEG_WRITABLE: u64 = 0xb3ff;
const MENVCFG_WRITABLE: u64 =
    envcfg::FIOM | envcfg::CBIE | envcfg::CBCFE | envcfg::CBZE | envcfg::PMM | envcfg::PBMTE | envcfg::STCE;
const SENVCFG_WRITABLE: u64 = envcfg::FIOM | envcfg::CBIE | envcfg::CBCFE | envcfg::CBZE | envcfg::PMM;

impl Hart {
    pub fn new(
        config: HartConfig,
        bus: Arc<Bus>,
        shared: Arc<HartShared>,
        time: Arc<dyn TimeSource>,
    ) -> Self {
        assert!(
            config.vlen.is_power_of_two() && (128..=8 * crate::vector::MAX_VLENB).contains(&config.vlen),
            "unsupported VLEN {}",
            config.vlen
        );
        let state = Box::new(HartState {
            x: [0; 32],
            pc: 0,
            f: [0; 32],
            v: [0; 32 * crate::vector::MAX_VLENB],
            privilege: Priv::Machine,
            csr: Csrs { misa: MISA, ..Csrs::default() },
            tlb: Tlb::new(),
            jit: crate::jit::JitState::default(),
        });
        let mut hart = Hart {
            state,
            shared,
            bus,
            config,
            time,
            reservation: None,
            wfi: false,
            suppress_instret: false,
            blocks: BlockCache::default(),
            compiler: None,
            jit_threshold: 16,
            jit_pending: 0,
            jit_cache_epoch: 0,
            jit_exception: None,
            pm_len: [0; 3],
        };
        hart.state.jit.exit_request = &hart.shared.exit_request as *const AtomicU32 as u64;
        hart
    }

    /// Resets the hart to M-mode at `pc`.
    pub fn reset(&mut self, pc: u64) {
        let s = &mut *self.state;
        s.x = [0; 32];
        s.f = [0; 32];
        s.v.fill(0);
        s.pc = pc;
        s.privilege = Priv::Machine;
        s.csr = Csrs { misa: MISA, ..Csrs::default() };
        s.tlb.flush();
        s.jit.jump_cache.fill(Default::default());
        self.reservation = None;
        self.wfi = false;
        self.blocks.clear();
        self.pm_len = [0; 3];
    }

    #[inline(always)]
    pub fn x(&self, r: u8) -> u64 {
        self.state.x[r as usize]
    }

    #[inline(always)]
    pub fn set_x(&mut self, r: u8, value: u64) {
        if r != 0 {
            self.state.x[r as usize] = value;
        }
    }

    pub fn mtime(&self) -> u64 {
        self.time.mtime()
    }

    // ---- Traps ----

    /// Enters the trap handler for `cause` (with `INTERRUPT_BIT` set for interrupts).
    pub fn take_trap(&mut self, cause: u64, tval: u64) {
        // The next block is the handler, not the successor of the last exit.
        self.state.jit.chain_from = 0;
        let is_interrupt = cause & INTERRUPT_BIT != 0;
        let code = cause & !INTERRUPT_BIT;
        let s = &mut *self.state;
        let deleg = if is_interrupt { s.csr.mideleg } else { s.csr.medeleg };
        let to_s = s.privilege <= Priv::Supervisor && code < 64 && (deleg >> code) & 1 != 0;
        let pc = s.pc;
        let ms = s.csr.mstatus;

        let (tvec, new_status) = if to_s {
            s.csr.sepc = pc;
            s.csr.scause = cause;
            s.csr.stval = tval;
            let spie = if ms & status::SIE != 0 { status::SPIE } else { 0 };
            let spp = if s.privilege == Priv::Supervisor { status::SPP } else { 0 };
            s.privilege = Priv::Supervisor;
            (s.csr.stvec, (ms & !(status::SIE | status::SPIE | status::SPP)) | spie | spp)
        } else {
            s.csr.mepc = pc;
            s.csr.mcause = cause;
            s.csr.mtval = tval;
            let mpie = if ms & status::MIE != 0 { status::MPIE } else { 0 };
            let mpp = (s.privilege as u64) << status::MPP_SHIFT;
            s.privilege = Priv::Machine;
            (s.csr.mtvec, (ms & !(status::MIE | status::MPIE | status::MPP)) | mpie | mpp)
        };
        s.csr.mstatus = new_status;
        let vectored = tvec & 1 != 0 && is_interrupt;
        s.pc = (tvec & !3) + if vectored { 4 * code } else { 0 };
    }

    /// The highest-priority interrupt that is pending and enabled at the current privilege.
    pub fn pending_interrupt(&self) -> Option<u64> {
        let s = &*self.state;
        let pending = self.shared.mip.load(Ordering::Relaxed) & s.csr.mie;
        if pending == 0 {
            return None;
        }
        let m_enabled = s.privilege < Priv::Machine || s.csr.mstatus & status::MIE != 0;
        let s_enabled = s.privilege < Priv::Supervisor
            || (s.privilege == Priv::Supervisor && s.csr.mstatus & status::SIE != 0);
        let m_pending = if m_enabled { pending & !s.csr.mideleg } else { 0 };
        let s_pending = if s_enabled { pending & s.csr.mideleg } else { 0 };
        // M-level interrupts always win over S-level ones.
        for set in [m_pending, s_pending] {
            if let Some(&n) = irq::PRIORITY.iter().find(|&&n| set & (1 << n) != 0) {
                return Some(n | INTERRUPT_BIT);
            }
        }
        None
    }

    pub(crate) fn mret(&mut self) {
        let s = &mut *self.state;
        let ms = s.csr.mstatus;
        let mpp = Priv::from_bits(ms >> status::MPP_SHIFT);
        let mut new = ms & !(status::MIE | status::MPP);
        new |= status::MPIE;
        if ms & status::MPIE != 0 {
            new |= status::MIE;
        }
        if mpp != Priv::Machine {
            new &= !status::MPRV;
        }
        s.csr.mstatus = new;
        s.privilege = mpp;
        s.pc = s.csr.mepc;
    }

    pub(crate) fn sret(&mut self) {
        let s = &mut *self.state;
        let ms = s.csr.mstatus;
        let spp = if ms & status::SPP != 0 { Priv::Supervisor } else { Priv::User };
        let mut new = ms & !(status::SIE | status::SPP | status::MPRV);
        new |= status::SPIE;
        if ms & status::SPIE != 0 {
            new |= status::SIE;
        }
        s.csr.mstatus = new;
        s.privilege = spp;
        s.pc = s.csr.sepc;
    }

    // ---- CSRs ----

    fn read_mstatus(&self) -> u64 {
        let ms = self.state.csr.mstatus | status::UXL_SXL_64;
        let dirty =
            ms & status::FS == status::FS || ms & status::VS == status::VS || ms & status::XS == status::XS;
        if dirty { ms | status::SD } else { ms }
    }

    fn write_mstatus(&mut self, value: u64) {
        let old = self.state.csr.mstatus;
        let mut new = (old & !status::M_WRITABLE) | (value & status::M_WRITABLE);
        // MPP is WARL: H-mode (2) is not supported, keep the old value.
        if (new & status::MPP) >> status::MPP_SHIFT == 2 {
            new = (new & !status::MPP) | (old & status::MPP);
        }
        self.state.csr.mstatus = new;
        if (old ^ new) & (status::SUM | status::MXR) != 0 {
            self.state.tlb.flush();
        }
    }

    /// Whether the counter at bit `n` of the counteren registers is accessible.
    fn counter_accessible(&self, n: u16) -> bool {
        let bit = 1u64 << n;
        match self.state.privilege {
            Priv::Machine => true,
            Priv::Supervisor => self.state.csr.mcounteren & bit != 0,
            Priv::User => self.state.csr.mcounteren & self.state.csr.scounteren & bit != 0,
        }
    }

    /// Reads a CSR for a debugger, ignoring privilege. `None` if it does not exist.
    pub fn debug_read_csr(&mut self, csr: u16) -> Option<u64> {
        // Counter accessibility depends on the current privilege; evaluate as M-mode.
        let saved = std::mem::replace(&mut self.state.privilege, Priv::Machine);
        let value = self.csr_read(csr, 0).ok();
        self.state.privilege = saved;
        value
    }

    /// Checks privilege and read-only rules for an access to `csr`.
    pub(crate) fn csr_check(&self, csr: u16, write: bool, raw: u32) -> Result<(), Exception> {
        let min_priv = (csr >> 8) & 3;
        let read_only = (csr >> 10) & 3 == 3;
        if (self.state.privilege as u16) < min_priv || (write && read_only) {
            return Err(Exception::illegal(raw));
        }
        if self.state.privilege == Priv::Supervisor {
            let c = &self.state.csr;
            let denied = match csr {
                csr::SATP => c.mstatus & status::TVM != 0,
                csr::STIMECMP => c.menvcfg & envcfg::STCE == 0 || c.mcounteren & csr::COUNTEREN_TM == 0,
                csr::SENVCFG => c.mstateen[0] & stateen::ENVCFG == 0,
                csr::SSTATEEN0..=csr::SSTATEEN3 => {
                    c.mstateen[(csr - csr::SSTATEEN0) as usize] & stateen::SE0 == 0
                }
                _ => false,
            };
            if denied {
                return Err(Exception::illegal(raw));
            }
        }
        Ok(())
    }

    pub(crate) fn csr_read(&self, csr: u16, raw: u32) -> Result<u64, Exception> {
        let c = &self.state.csr;
        let mip = self.shared.mip.load(Ordering::Relaxed);
        Ok(match csr {
            csr::FFLAGS | csr::FRM | csr::FCSR => {
                if !self.fp_enabled() {
                    return Err(Exception::illegal(raw));
                }
                match csr {
                    csr::FFLAGS => c.fcsr & 0x1f,
                    csr::FRM => (c.fcsr >> 5) & 7,
                    _ => c.fcsr & 0xff,
                }
            }
            csr::VSTART | csr::VXSAT | csr::VXRM | csr::VCSR | csr::VL | csr::VTYPE | csr::VLENB => {
                if !self.vector_enabled() {
                    return Err(Exception::illegal(raw));
                }
                match csr {
                    csr::VSTART => c.vstart,
                    csr::VXSAT => c.vxsat,
                    csr::VXRM => c.vxrm,
                    csr::VCSR => (c.vxrm << 1) | c.vxsat,
                    csr::VL => c.vl,
                    csr::VTYPE => c.vtype,
                    _ => self.vlenb() as u64,
                }
            }
            csr::CYCLE | csr::TIME | csr::INSTRET | csr::HPMCOUNTER3..=csr::HPMCOUNTER31 => {
                if !self.counter_accessible(csr - csr::CYCLE) {
                    return Err(Exception::illegal(raw));
                }
                // Compiled code adds what it retired when it returns to the dispatcher.
                let pending = self.state.jit.retired;
                match csr {
                    csr::CYCLE => c.mcycle.wrapping_add(pending),
                    csr::TIME => self.mtime(),
                    csr::INSTRET => c.minstret.wrapping_add(pending),
                    _ => 0,
                }
            }

            csr::SSTATUS => self.read_mstatus() & status::S_VISIBLE,
            csr::SIE => c.mie & c.mideleg,
            csr::STVEC => c.stvec,
            csr::SCOUNTEREN => c.scounteren,
            csr::SENVCFG => c.senvcfg,
            csr::SSCRATCH => c.sscratch,
            csr::SEPC => c.sepc,
            csr::SCAUSE => c.scause,
            csr::STVAL => c.stval,
            csr::SIP => mip & c.mideleg,
            csr::SATP => c.satp,
            csr::STIMECMP => c.stimecmp,
            csr::SSTATEEN0..=csr::SSTATEEN3 => 0,
            // No counter can overflow: the HPM counters are hard-wired to zero.
            csr::SCOUNTOVF => 0,
            csr::MSTATEEN0..=csr::MSTATEEN3 => c.mstateen[(csr - csr::MSTATEEN0) as usize],

            csr::MVENDORID | csr::MARCHID | csr::MIMPID | csr::MCONFIGPTR => 0,
            csr::MHARTID => self.config.hartid,
            csr::MSTATUS => self.read_mstatus(),
            csr::MISA => c.misa,
            csr::MEDELEG => c.medeleg,
            csr::MIDELEG => c.mideleg,
            csr::MIE => c.mie,
            csr::MTVEC => c.mtvec,
            csr::MCOUNTEREN => c.mcounteren,
            csr::MENVCFG => c.menvcfg,
            csr::MCOUNTINHIBIT => c.mcountinhibit,
            csr::MHPMEVENT3..=csr::MHPMEVENT31 => 0,
            csr::MSCRATCH => c.mscratch,
            csr::MEPC => c.mepc,
            csr::MCAUSE => c.mcause,
            csr::MTVAL => c.mtval,
            csr::MIP => mip,
            csr::PMPCFG0..=csr::PMPCFG15 => {
                let n = csr - csr::PMPCFG0;
                // On RV64 only the even-numbered pmpcfg registers exist.
                if !n.is_multiple_of(2) {
                    return Err(Exception::illegal(raw));
                }
                c.pmpcfg[n as usize / 2]
            }
            csr::PMPADDR0..=csr::PMPADDR63 => c.pmpaddr[(csr - csr::PMPADDR0) as usize],
            csr::MSECCFG => 0,
            // Sdtrig with no usable triggers: tselect is hard-wired to 0 and the slot has type 0
            // ("none"). Software (and riscv-tests) probe these rather than expecting a trap.
            csr::TSELECT | csr::TDATA1 | csr::TDATA2 | csr::TDATA3 => 0,
            csr::TINFO => 1,
            csr::MCYCLE => c.mcycle.wrapping_add(self.state.jit.retired),
            csr::MINSTRET => c.minstret.wrapping_add(self.state.jit.retired),
            csr::MHPMCOUNTER3..=csr::MHPMCOUNTER31 => 0,
            _ => return Err(Exception::illegal(raw)),
        })
    }

    /// Writes a CSR. Assumes [`csr_check`](Self::csr_check) passed and the CSR exists.
    pub(crate) fn csr_write(&mut self, csr: u16, value: u64) {
        let c = &mut self.state.csr;
        match csr {
            csr::VSTART | csr::VXSAT | csr::VXRM | csr::VCSR => {
                match csr {
                    // vstart only needs to hold element indices up to VLMAX.
                    csr::VSTART => c.vstart = value & 0xffff,
                    csr::VXSAT => c.vxsat = value & 1,
                    csr::VXRM => c.vxrm = value & 3,
                    _ => {
                        c.vxsat = value & 1;
                        c.vxrm = (value >> 1) & 3;
                    }
                }
                c.mstatus |= status::VS;
            }
            csr::FFLAGS | csr::FRM | csr::FCSR => {
                c.fcsr = match csr {
                    csr::FFLAGS => (c.fcsr & !0x1f) | (value & 0x1f),
                    csr::FRM => (c.fcsr & !0xe0) | ((value & 7) << 5),
                    _ => value & 0xff,
                };
                c.mstatus |= status::FS;
            }
            csr::SSTATUS => {
                let merged = (self.state.csr.mstatus & !status::S_WRITABLE) | (value & status::S_WRITABLE);
                self.write_mstatus(merged);
            }
            csr::SIE => {
                let mask = c.mideleg & MIE_WRITABLE;
                c.mie = (c.mie & !mask) | (value & mask);
            }
            csr::STVEC => c.stvec = legal_tvec(c.stvec, value),
            csr::SCOUNTEREN => c.scounteren = value & 0xffff_ffff,
            csr::SENVCFG => {
                c.senvcfg = legal_envcfg(c.senvcfg, value & SENVCFG_WRITABLE);
                self.update_pointer_masking();
            }
            csr::SSCRATCH => c.sscratch = value,
            csr::SEPC => c.sepc = value & !1,
            csr::SCAUSE => c.scause = value,
            csr::STVAL => c.stval = value,
            csr::SIP => {
                let mask = c.mideleg & ((1 << irq::SSI) | (1 << irq::LCOFI));
                self.write_mip(mask, value);
            }
            csr::SATP => self.write_satp(value),
            csr::STIMECMP => {
                c.stimecmp = value;
                self.update_timer_interrupts();
            }
            csr::MSTATEEN0 => c.mstateen[0] = value & (stateen::SE0 | stateen::ENVCFG),
            csr::MSTATEEN1..=csr::MSTATEEN3 => {
                c.mstateen[(csr - csr::MSTATEEN0) as usize] = value & stateen::SE0;
            }

            csr::MSTATUS => self.write_mstatus(value),
            csr::MISA => {} // WARL, not writable
            csr::MEDELEG => c.medeleg = value & MEDELEG_WRITABLE,
            csr::MIDELEG => c.mideleg = value & MIDELEG_WRITABLE,
            csr::MIE => c.mie = value & MIE_WRITABLE,
            csr::MTVEC => c.mtvec = legal_tvec(c.mtvec, value),
            csr::MCOUNTEREN => c.mcounteren = value & 0xffff_ffff,
            csr::MENVCFG => {
                let old = c.menvcfg;
                c.menvcfg = legal_envcfg(old, value & MENVCFG_WRITABLE);
                if (old ^ c.menvcfg) & envcfg::PBMTE != 0 {
                    self.state.tlb.flush();
                }
                self.update_pointer_masking();
                self.update_timer_interrupts();
            }
            csr::MCOUNTINHIBIT => c.mcountinhibit = value & 0xffff_fffd,
            csr::MSCRATCH => c.mscratch = value,
            csr::MEPC => c.mepc = value & !1,
            csr::MCAUSE => c.mcause = value,
            csr::MTVAL => c.mtval = value,
            csr::MIP => {
                // With Sstc enabled, STIP reflects the stimecmp comparator and is read-only.
                let stip = if c.menvcfg & envcfg::STCE != 0 { 1 << irq::STI } else { 0 };
                self.write_mip(MIP_M_WRITABLE & !stip, value);
            }
            csr::PMPCFG0..=csr::PMPCFG15 => c.pmpcfg[(csr - csr::PMPCFG0) as usize / 2] = value,
            csr::PMPADDR0..=csr::PMPADDR63 => {
                c.pmpaddr[(csr - csr::PMPADDR0) as usize] = value & ((1 << 54) - 1);
            }
            // Inside compiled code, the retirements it has not reported yet are added later.
            csr::MCYCLE => c.mcycle = value.wrapping_sub(self.state.jit.retired),
            csr::MINSTRET => {
                c.minstret = value.wrapping_sub(self.state.jit.retired);
                self.suppress_instret = true;
            }
            // Read-only zero or writes ignored.
            _ => {}
        }
    }

    fn write_mip(&mut self, mask: u64, value: u64) {
        let mip = &self.shared.mip;
        let mut cur = mip.load(Ordering::Relaxed);
        loop {
            let new = (cur & !mask) | (value & mask);
            match mip.compare_exchange_weak(cur, new, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    fn write_satp(&mut self, value: u64) {
        let mode = value >> satp::MODE_SHIFT;
        let supported = mode == satp::MODE_BARE
            || (satp::MODE_SV39..=self.config.max_satp_mode.min(satp::MODE_SV57)).contains(&mode);
        // Writes selecting an unsupported mode have no effect at all.
        if !supported {
            return;
        }
        self.state.csr.satp = value & ((0xf << satp::MODE_SHIFT) | satp::ASID_MASK | satp::PPN_MASK);
        self.state.tlb.flush();
    }

    /// Sstc: drives STIP from `time >= stimecmp` while `menvcfg.STCE` is set.
    pub fn update_timer_interrupts(&mut self) {
        if self.state.csr.menvcfg & envcfg::STCE == 0 {
            return;
        }
        let pending = self.mtime() >= self.state.csr.stimecmp;
        let current = self.shared.mip.load(Ordering::Relaxed) & (1 << irq::STI) != 0;
        if pending != current {
            self.shared.set_irq(1 << irq::STI, pending);
        }
    }

    /// The `time` value at which this hart's own (Sstc) timer fires, if enabled.
    pub fn timer_deadline(&self) -> Option<u64> {
        (self.state.csr.menvcfg & envcfg::STCE != 0).then_some(self.state.csr.stimecmp)
    }

    /// Recomputes the pointer-masking length for each data-access privilege.
    fn update_pointer_masking(&mut self) {
        let pmlen = |envcfg: u64| match (envcfg & envcfg::PMM) >> envcfg::PMM_SHIFT {
            2 => 7,
            3 => 16,
            _ => 0,
        };
        // senvcfg controls U-mode (Ssnpm), menvcfg controls S-mode (Smnpm); M-mode masking
        // (Smmpm) is not implemented.
        self.pm_len = [pmlen(self.state.csr.senvcfg), pmlen(self.state.csr.menvcfg), 0];
    }

    /// The exception cause an ECALL raises at the current privilege.
    pub(crate) fn ecall_cause(&self) -> u64 {
        match self.state.privilege {
            Priv::User => cause::ECALL_U,
            Priv::Supervisor => cause::ECALL_S,
            Priv::Machine => cause::ECALL_M,
        }
    }
}

/// Keeps the WARL fields of `menvcfg`/`senvcfg` legal: CBIE's reserved value 10 reads as 00,
/// and PMM's reserved value 01 leaves the field unchanged.
fn legal_envcfg(old: u64, mut value: u64) -> u64 {
    if value & envcfg::CBIE == 2 << 4 {
        value &= !envcfg::CBIE;
    }
    if (value & envcfg::PMM) >> envcfg::PMM_SHIFT == 1 {
        value = (value & !envcfg::PMM) | (old & envcfg::PMM);
    }
    value
}

/// `xtvec` is WARL: only direct (0) and vectored (1) modes exist; base must be 4-byte aligned.
fn legal_tvec(old: u64, value: u64) -> u64 {
    if value & 3 >= 2 { (value & !3) | (old & 3) } else { value }
}
