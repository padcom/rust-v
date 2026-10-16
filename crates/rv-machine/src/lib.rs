//! The risc-vm board. Its memory map and devices follow QEMU's `virt` machine so that
//! unmodified OpenSBI, U-Boot and Linux images run on it.

pub mod clint;
pub mod fdt;
pub mod plic;
pub mod power;
pub mod rtc;
pub mod uart;

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rv_core::{Bus, Exit, Hart, HartConfig, HartShared, HostClock, TimeSource};

pub use power::Shutdown;

pub mod layout {
    pub const TEST_BASE: u64 = 0x0010_0000;
    pub const RTC_BASE: u64 = 0x0010_1000;
    pub const CLINT_BASE: u64 = 0x0200_0000;
    pub const PLIC_BASE: u64 = 0x0c00_0000;
    pub const UART_BASE: u64 = 0x1000_0000;
    pub const RAM_BASE: u64 = 0x8000_0000;

    pub const UART_IRQ: u32 = 10;
    pub const RTC_IRQ: u32 = 11;

    pub const TIMEBASE_HZ: u64 = 10_000_000;

    /// Where the kernel goes; firmware occupies the first 2 MiB of RAM.
    pub const KERNEL_OFFSET: u64 = 0x20_0000;
}

use layout::*;

/// Extensions implemented by the hart, as advertised to the guest.
pub const ISA_EXTENSIONS: &[&str] = &["i", "m", "a", "c", "zicntr", "zicsr", "zifencei", "zihintpause"];

/// Instructions a hart runs between timer and shutdown checks.
const SLICE: u64 = 20_000;
/// Longest a hart sleeps in WFI before re-checking timers.
const MAX_IDLE: Duration = Duration::from_millis(10);

pub struct MachineConfig {
    pub ram_size: u64,
    /// M-mode firmware (e.g. OpenSBI `fw_dynamic.bin`), loaded at the start of RAM.
    pub bios: Vec<u8>,
    /// S-mode payload (e.g. a Linux `Image`), loaded at `KERNEL_OFFSET`.
    pub kernel: Option<Vec<u8>>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    /// Most capable `satp` mode (8 = Sv39, 9 = Sv48, 10 = Sv57).
    pub max_satp_mode: u64,
}

impl Default for MachineConfig {
    fn default() -> Self {
        MachineConfig {
            ram_size: 512 << 20,
            bios: Vec::new(),
            kernel: None,
            initrd: None,
            cmdline: "console=ttyS0 earlycon=sbi".into(),
            max_satp_mode: rv_core::csr::satp::MODE_SV48,
        }
    }
}

pub struct Machine {
    pub bus: Arc<Bus>,
    pub harts: Vec<Hart>,
    pub clint: Arc<clint::Clint>,
    pub plic: Arc<plic::Plic>,
    pub uart: Arc<uart::Uart>,
    pub power: Arc<power::PowerState>,
    /// The generated device tree, as handed to the firmware.
    pub dtb: Vec<u8>,
    pub stats: Stats,
}

/// Execution statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Host time spent executing guest code (excludes time parked in WFI).
    pub busy: Duration,
    /// Host time spent parked in WFI.
    pub idle: Duration,
}

impl Machine {
    /// Instructions retired by all harts.
    pub fn instructions(&self) -> u64 {
        self.harts.iter().map(|h| h.state.csr.minstret).sum()
    }
}

/// OpenSBI's `struct fw_dynamic_info` (version 2).
const FW_DYNAMIC_MAGIC: u64 = 0x4942_534f;
const FW_DYNAMIC_VERSION: u64 = 2;
const NEXT_MODE_S: u64 = 1;

#[derive(Debug)]
pub enum Error {
    Fdt(vm_fdt::Error),
    Layout(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Fdt(e) => write!(f, "device tree generation failed: {e}"),
            Error::Layout(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

const fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

impl Machine {
    pub fn new(config: MachineConfig, console: Box<dyn Write + Send>) -> Result<Self, Error> {
        let num_harts = 1;
        let ram_size = config.ram_size;
        let ram_end = RAM_BASE + ram_size;
        let time: Arc<dyn TimeSource> = Arc::new(HostClock::new(TIMEBASE_HZ));
        let shared: Vec<Arc<HartShared>> = (0..num_harts).map(|_| Arc::new(HartShared::default())).collect();

        let clint = Arc::new(clint::Clint::new(shared.clone(), time.clone()));
        let plic = Arc::new(plic::Plic::new(&shared));
        let uart = Arc::new(uart::Uart::new(console, plic.clone(), UART_IRQ));
        let power = Arc::new(power::PowerState::new(shared.clone()));

        let mut bus = Bus::new(RAM_BASE, ram_size);
        bus.add_device(TEST_BASE, power::SIZE, Arc::new(power::TestDevice::new(power.clone())));
        bus.add_device(RTC_BASE, rtc::SIZE, Arc::new(rtc::GoldfishRtc::default()));
        bus.add_device(CLINT_BASE, clint::SIZE, clint.clone());
        bus.add_device(PLIC_BASE, plic::SIZE, plic.clone());
        bus.add_device(UART_BASE, uart::SIZE, uart.clone());

        // Memory layout: firmware at the bottom, kernel at +2 MiB, initrd in the middle of RAM,
        // device tree and boot info in the top 2 MiB.
        let fits = |what: &str, start: u64, len: u64, limit: u64| {
            if start + len > limit {
                Err(Error::Layout(format!("{what} ({len} bytes at {start:#x}) does not fit in RAM")))
            } else {
                Ok(())
            }
        };
        let kernel_addr = RAM_BASE + KERNEL_OFFSET;
        let dtb_addr = ram_end - (2 << 20);
        let info_addr = dtb_addr - 0x1000;
        fits("firmware", RAM_BASE, config.bios.len() as u64, kernel_addr)?;
        bus.load_image(RAM_BASE, &config.bios);
        if let Some(kernel) = &config.kernel {
            fits("kernel", kernel_addr, kernel.len() as u64, info_addr)?;
            bus.load_image(kernel_addr, kernel);
        }
        let initrd = match &config.initrd {
            Some(data) => {
                let kernel_end = kernel_addr + config.kernel.as_ref().map_or(0, |k| k.len() as u64);
                let start = align_up((RAM_BASE + ram_size / 2).max(kernel_end + (16 << 20)), 2 << 20);
                fits("initrd", start, data.len() as u64, info_addr)?;
                bus.load_image(start, data);
                Some((start, start + data.len() as u64))
            }
            None => None,
        };

        let mmu_type = match config.max_satp_mode {
            8 => "riscv,sv39",
            9 => "riscv,sv48",
            _ => "riscv,sv57",
        };
        let dtb = fdt::build(&fdt::FdtParams {
            num_harts,
            ram_size,
            isa_extensions: ISA_EXTENSIONS,
            mmu_type,
            cmdline: &config.cmdline,
            initrd,
        })
        .map_err(Error::Fdt)?;
        fits("device tree", dtb_addr, dtb.len() as u64, ram_end)?;
        bus.load_image(dtb_addr, &dtb);

        let info = [FW_DYNAMIC_MAGIC, FW_DYNAMIC_VERSION, kernel_addr, NEXT_MODE_S, 0, u64::MAX];
        let info_bytes: Vec<u8> = info.iter().flat_map(|v| v.to_le_bytes()).collect();
        bus.load_image(info_addr, &info_bytes);

        let bus = Arc::new(bus);
        let harts = shared
            .iter()
            .enumerate()
            .map(|(id, sh)| {
                let cfg = HartConfig { hartid: id as u64, max_satp_mode: config.max_satp_mode };
                let mut hart = Hart::new(cfg, bus.clone(), sh.clone(), time.clone());
                hart.reset(RAM_BASE);
                // OpenSBI fw_dynamic boot protocol.
                hart.state.x[10] = id as u64;
                hart.state.x[11] = dtb_addr;
                hart.state.x[12] = info_addr;
                hart
            })
            .collect();

        Ok(Machine { bus, harts, clint, plic, uart, power, dtb, stats: Stats::default() })
    }

    /// Runs until the guest powers off or resets the machine.
    pub fn run(&mut self) -> Shutdown {
        let hart = &mut self.harts[0];
        loop {
            let started = Instant::now();
            let exit = hart.run(SLICE);
            self.stats.busy += started.elapsed();
            self.clint.update_timers();
            if let Some(reason) = self.power.reason() {
                return reason;
            }
            if exit == Exit::Wfi {
                let now = hart.mtime();
                let deadline = self.clint.mtimecmp(0);
                if deadline > now {
                    let ticks = deadline - now;
                    let wait =
                        Duration::from_nanos(ticks.saturating_mul(1_000_000_000 / TIMEBASE_HZ)).min(MAX_IDLE);
                    let started = Instant::now();
                    hart.shared.wait(wait);
                    self.stats.idle += started.elapsed();
                }
                self.clint.update_timers();
            }
        }
    }
}
