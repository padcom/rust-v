//! Hart architectural state, traps and CSR access.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::bus::Bus;
use crate::csr::{self, misa, satp, status};
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
}

impl HostClock {
    pub fn new(freq_hz: u64) -> Self {
        HostClock { start: Instant::now(), freq_hz }
    }
}

impl TimeSource for HostClock {
    fn mtime(&self) -> u64 {
        (self.start.elapsed().as_nanos() * self.freq_hz as u128 / 1_000_000_000) as u64
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
    pub privilege: Priv,
    pub csr: Csrs,
    pub tlb: Tlb,
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
}

impl Default for HartConfig {
    fn default() -> Self {
        HartConfig { hartid: 0, max_satp_mode: satp::MODE_SV57 }
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
}

const MISA: u64 = misa::MXL_64
    | misa::ext(b'I')
    | misa::ext(b'M')
    | misa::ext(b'A')
    | misa::ext(b'C')
    | misa::ext(b'S')
    | misa::ext(b'U');

const MIP_M_WRITABLE: u64 = (1 << irq::SSI) | (1 << irq::STI) | (1 << irq::SEI);
const MIE_WRITABLE: u64 =
    (1 << irq::SSI) | (1 << irq::MSI) | (1 << irq::STI) | (1 << irq::MTI) | (1 << irq::SEI) | (1 << irq::MEI);
const MIDELEG_WRITABLE: u64 = (1 << irq::SSI) | (1 << irq::STI) | (1 << irq::SEI);
/// All synchronous exceptions except ECALL from M-mode (and reserved causes).
const MEDELEG_WRITABLE: u64 = 0xb3ff;
const MENVCFG_WRITABLE: u64 = 1; // FIOM
const SENVCFG_WRITABLE: u64 = 1; // FIOM

impl Hart {
    pub fn new(
        config: HartConfig,
        bus: Arc<Bus>,
        shared: Arc<HartShared>,
        time: Arc<dyn TimeSource>,
    ) -> Self {
        let state = Box::new(HartState {
            x: [0; 32],
            pc: 0,
            f: [0; 32],
            privilege: Priv::Machine,
            csr: Csrs { misa: MISA, ..Csrs::default() },
            tlb: Tlb::new(),
        });
        Hart { state, shared, bus, config, time, reservation: None, wfi: false, suppress_instret: false }
    }

    /// Resets the hart to M-mode at `pc`.
    pub fn reset(&mut self, pc: u64) {
        let s = &mut *self.state;
        s.x = [0; 32];
        s.f = [0; 32];
        s.pc = pc;
        s.privilege = Priv::Machine;
        s.csr = Csrs { misa: MISA, ..Csrs::default() };
        s.tlb.flush();
        self.reservation = None;
        self.wfi = false;
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

    /// Checks privilege and read-only rules for an access to `csr`.
    pub(crate) fn csr_check(&self, csr: u16, write: bool, raw: u32) -> Result<(), Exception> {
        let min_priv = (csr >> 8) & 3;
        let read_only = (csr >> 10) & 3 == 3;
        if (self.state.privilege as u16) < min_priv || (write && read_only) {
            return Err(Exception::illegal(raw));
        }
        if csr == csr::SATP
            && self.state.privilege == Priv::Supervisor
            && self.state.csr.mstatus & status::TVM != 0
        {
            return Err(Exception::illegal(raw));
        }
        Ok(())
    }

    pub(crate) fn csr_read(&self, csr: u16, raw: u32) -> Result<u64, Exception> {
        let c = &self.state.csr;
        let mip = self.shared.mip.load(Ordering::Relaxed);
        Ok(match csr {
            csr::CYCLE | csr::TIME | csr::INSTRET | csr::HPMCOUNTER3..=csr::HPMCOUNTER31 => {
                if !self.counter_accessible(csr - csr::CYCLE) {
                    return Err(Exception::illegal(raw));
                }
                match csr {
                    csr::CYCLE => c.mcycle,
                    csr::TIME => self.mtime(),
                    csr::INSTRET => c.minstret,
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
            // A single trigger slot of type 0 ("none"), i.e. no usable triggers.
            csr::TSELECT | csr::TDATA1 | csr::TDATA2 | csr::TDATA3 => 0,
            csr::TINFO => 1,
            csr::MCYCLE => c.mcycle,
            csr::MINSTRET => c.minstret,
            csr::MHPMCOUNTER3..=csr::MHPMCOUNTER31 => 0,
            _ => return Err(Exception::illegal(raw)),
        })
    }

    /// Writes a CSR. Assumes [`csr_check`](Self::csr_check) passed and the CSR exists.
    pub(crate) fn csr_write(&mut self, csr: u16, value: u64) {
        let c = &mut self.state.csr;
        match csr {
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
            csr::SENVCFG => c.senvcfg = value & SENVCFG_WRITABLE,
            csr::SSCRATCH => c.sscratch = value,
            csr::SEPC => c.sepc = value & !1,
            csr::SCAUSE => c.scause = value,
            csr::STVAL => c.stval = value,
            csr::SIP => {
                let mask = c.mideleg & (1 << irq::SSI);
                self.write_mip(mask, value);
            }
            csr::SATP => self.write_satp(value),

            csr::MSTATUS => self.write_mstatus(value),
            csr::MISA => {} // WARL, not writable
            csr::MEDELEG => c.medeleg = value & MEDELEG_WRITABLE,
            csr::MIDELEG => c.mideleg = value & MIDELEG_WRITABLE,
            csr::MIE => c.mie = value & MIE_WRITABLE,
            csr::MTVEC => c.mtvec = legal_tvec(c.mtvec, value),
            csr::MCOUNTEREN => c.mcounteren = value & 0xffff_ffff,
            csr::MENVCFG => c.menvcfg = value & MENVCFG_WRITABLE,
            csr::MCOUNTINHIBIT => c.mcountinhibit = value & 0xffff_fffd,
            csr::MSCRATCH => c.mscratch = value,
            csr::MEPC => c.mepc = value & !1,
            csr::MCAUSE => c.mcause = value,
            csr::MTVAL => c.mtval = value,
            csr::MIP => self.write_mip(MIP_M_WRITABLE, value),
            csr::PMPCFG0..=csr::PMPCFG15 => c.pmpcfg[(csr - csr::PMPCFG0) as usize / 2] = value,
            csr::PMPADDR0..=csr::PMPADDR63 => {
                c.pmpaddr[(csr - csr::PMPADDR0) as usize] = value & ((1 << 54) - 1);
            }
            csr::MCYCLE => c.mcycle = value,
            csr::MINSTRET => {
                c.minstret = value;
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

    /// The exception cause an ECALL raises at the current privilege.
    pub(crate) fn ecall_cause(&self) -> u64 {
        match self.state.privilege {
            Priv::User => cause::ECALL_U,
            Priv::Supervisor => cause::ECALL_S,
            Priv::Machine => cause::ECALL_M,
        }
    }
}

/// `xtvec` is WARL: only direct (0) and vectored (1) modes exist; base must be 4-byte aligned.
fn legal_tvec(old: u64, value: u64) -> u64 {
    if value & 3 >= 2 { (value & !3) | (old & 3) } else { value }
}
