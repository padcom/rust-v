//! The rust-v board. Its memory map and devices follow QEMU's `virt` machine so that
//! unmodified OpenSBI, U-Boot and Linux images run on it.

pub mod clint;
pub mod display;
pub mod fdt;
pub mod gdb;
pub mod pci;
pub mod plic;
pub mod power;
pub mod rtc;
pub mod uart;
pub mod virtio;

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rv_core::{Bus, Exit, Hart, HartConfig, HartShared, HostClock, TimeSource};

pub use power::Shutdown;
pub use rv_core::vector::profile::report as vector_profile_report;
pub use rv_jit::Stats as JitStats;
pub use rv_net;

pub mod layout {
    pub const TEST_BASE: u64 = 0x0010_0000;
    pub const RTC_BASE: u64 = 0x0010_1000;
    pub const CLINT_BASE: u64 = 0x0200_0000;
    pub const PLIC_BASE: u64 = 0x0c00_0000;
    pub const PCIE_PIO_BASE: u64 = 0x0300_0000;
    pub const PCIE_PIO_SIZE: u64 = 0x1_0000;
    pub const UART_BASE: u64 = 0x1000_0000;
    pub const PCIE_ECAM_BASE: u64 = 0x3000_0000;
    pub const PCIE_MMIO_BASE: u64 = 0x4000_0000;
    pub const PCIE_MMIO_SIZE: u64 = 0x4000_0000;
    pub const RAM_BASE: u64 = 0x8000_0000;
    /// The 64-bit PCIe window: this size, aligned to its size above the end of RAM.
    pub const PCIE_MMIO_HIGH_SIZE: u64 = 16 << 30;

    pub const UART_IRQ: u32 = 10;
    pub const RTC_IRQ: u32 = 11;
    /// PCIe INTA..INTD.
    pub const PCIE_IRQ: u32 = 32;

    pub const TIMEBASE_HZ: u64 = 10_000_000;

    /// Where the kernel goes; firmware occupies the first 2 MiB of RAM.
    pub const KERNEL_OFFSET: u64 = 0x20_0000;
}

use layout::*;

/// Extensions implemented by the hart, as advertised to the guest.
#[rustfmt::skip]
pub const ISA_EXTENSIONS: &[&str] = &[
    "i", "m", "a", "f", "d", "c", "v", "zic64b", "zicbom", "zicbop", "zicboz", "ziccamoa", "ziccif",
    "zicclsm", "ziccrse", "zicntr", "zicond", "zicsr", "zifencei", "zihintntl", "zihintpause", "zihpm",
    "zimop", "za64rs", "zawrs", "zfa", "zfhmin", "zca", "zcb", "zcd", "zcmop", "zba", "zbb", "zbs", "zkt",
    "zvbb", "zvfh", "zvfhmin", "zvkt", "smnpm", "smstateen", "sscofpmf", "ssnpm", "ssstateen", "sstc",
    "svade", "svinval", "svnapot", "svpbmt",
];

/// Instructions a hart runs between timer and shutdown checks.
pub const SLICE: u64 = 20_000;
/// Most harts a machine can have.
pub const MAX_HARTS: usize = 64;
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
    /// Compile hot blocks to native code.
    pub jit: bool,
    /// Interpretations of a block before it gets compiled.
    pub jit_threshold: u32,
    /// Compile on background threads (otherwise the hart stalls while compiling).
    pub jit_background: bool,
    /// Size of each hart's code cache, in bytes.
    pub jit_cache_size: usize,
    /// Vector register length in bits.
    pub vlen: usize,
    /// Number of harts.
    pub harts: usize,
    /// Run each hart on its own host thread (otherwise round-robin on one thread).
    pub threaded: bool,
    /// virtio-blk disks, in PCI slot order.
    pub drives: Vec<Drive>,
    /// Add a virtio-rng device.
    pub rng: bool,
    /// Add a virtio-net device on a user-mode network with these settings.
    pub net: Option<rv_net::Config>,
    /// Add a virtio-gpu device showing on this display, with a virtio keyboard and tablet.
    pub display: Option<Arc<display::Display>>,
    /// Add a virtio-snd device with one playback stream.
    pub sound: bool,
}

/// A virtio-blk disk.
pub struct Drive {
    pub backend: Box<dyn virtio::blk::Backend>,
    pub read_only: bool,
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
            jit: true,
            jit_threshold: 16,
            jit_background: true,
            jit_cache_size: rv_jit::CODE_CACHE_SIZE,
            vlen: 128,
            harts: 4,
            threaded: true,
            drives: Vec::new(),
            rng: true,
            net: None,
            display: None,
            sound: false,
        }
    }
}

pub struct Machine {
    pub bus: Arc<Bus>,
    pub harts: Vec<Hart>,
    pub clint: Arc<clint::Clint>,
    pub plic: Arc<plic::Plic>,
    pub uart: Arc<uart::Uart>,
    pub pci: Arc<pci::PciHost>,
    /// Present when the machine has a display.
    pub gpu: Option<virtio::gpu::GpuHandle>,
    pub keyboard: Option<virtio::input::InputHandle>,
    pub tablet: Option<virtio::input::InputHandle>,
    /// Where the sound device's playback goes, when there is one.
    pub audio: Option<Arc<virtio::snd::AudioRing>>,
    /// Host addresses of the network's TCP forwards, in configuration order.
    pub forwarded: Vec<std::net::SocketAddr>,
    pub power: Arc<power::PowerState>,
    /// The generated device tree, as handed to the firmware.
    pub dtb: Vec<u8>,
    pub stats: Stats,
    /// JIT compilation statistics per hart, when the JIT is enabled.
    pub jit_stats: Option<Vec<Arc<rv_jit::Stats>>>,
    threaded: bool,
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
    Jit(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Fdt(e) => write!(f, "device tree generation failed: {e}"),
            Error::Layout(msg) => write!(f, "{msg}"),
            Error::Jit(msg) => write!(f, "JIT initialization failed: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

const fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

impl Machine {
    pub fn new(config: MachineConfig, console: Box<dyn Write + Send>) -> Result<Self, Error> {
        let num_harts = config.harts;
        if !(1..=MAX_HARTS).contains(&num_harts) {
            return Err(Error::Layout(format!("unsupported hart count {num_harts} (1 to {MAX_HARTS})")));
        }
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

        let pci = pci::PciHost::new(plic.clone(), PCIE_IRQ);
        let mmio_high = align_up(ram_end, PCIE_MMIO_HIGH_SIZE);
        bus.add_device(PCIE_PIO_BASE, PCIE_PIO_SIZE, Arc::new(pci::IoWindow));
        bus.add_device(PCIE_ECAM_BASE, pci::ECAM_SIZE, Arc::new(pci::Ecam(pci.clone())));
        for (base, size) in [(PCIE_MMIO_BASE, PCIE_MMIO_SIZE), (mmio_high, PCIE_MMIO_HIGH_SIZE)] {
            bus.add_device(base, size, Arc::new(pci::MemWindow { host: pci.clone(), base }));
        }
        let mut virtio_devices: Vec<Box<dyn virtio::VirtioDevice>> = Vec::new();
        for (i, drive) in config.drives.into_iter().enumerate() {
            let id = format!("rust-v-disk{i}");
            virtio_devices.push(Box::new(virtio::blk::Blk::new(drive.backend, drive.read_only, &id)));
        }
        let mut forwarded = Vec::new();
        if let Some(net_config) = config.net {
            let (mut net, rx) = virtio::net::Net::new(net_config.guest_mac);
            let backend = rv_net::UserNet::spawn(net_config, Box::new(move |frame| rx.receive(frame)))
                .map_err(|e| Error::Layout(format!("user-mode networking: {e}")))?;
            forwarded = backend.forwarded().to_vec();
            net.set_backend(Box::new(backend));
            virtio_devices.push(Box::new(net));
        }
        let (mut gpu, mut keyboard, mut tablet) = (None, None, None);
        if let Some(display) = config.display {
            let (dev, handle) = virtio::gpu::Gpu::new(display);
            gpu = Some(handle);
            virtio_devices.push(Box::new(dev));
            let (dev, handle) = virtio::input::Input::new(virtio::input::Kind::Keyboard);
            keyboard = Some(handle);
            virtio_devices.push(Box::new(dev));
            let (dev, handle) = virtio::input::Input::new(virtio::input::Kind::Tablet);
            tablet = Some(handle);
            virtio_devices.push(Box::new(dev));
        }
        let mut audio = None;
        if config.sound {
            let (dev, ring) = virtio::snd::Sound::new();
            audio = Some(ring);
            virtio_devices.push(Box::new(dev));
        }
        if config.rng {
            virtio_devices.push(Box::new(virtio::rng::Rng::default()));
        }
        for device in virtio_devices {
            let slot = pci.free_slot().ok_or_else(|| Error::Layout("out of PCI slots".into()))?;
            pci.attach(slot, Arc::new(virtio::VirtioPci::new(device, bus.memory(), pci.intx(slot))));
        }

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
            pcie_mmio_high: (mmio_high, PCIE_MMIO_HIGH_SIZE),
        })
        .map_err(Error::Fdt)?;
        fits("device tree", dtb_addr, dtb.len() as u64, ram_end)?;
        bus.load_image(dtb_addr, &dtb);

        let info = [FW_DYNAMIC_MAGIC, FW_DYNAMIC_VERSION, kernel_addr, NEXT_MODE_S, 0, u64::MAX];
        let info_bytes: Vec<u8> = info.iter().flat_map(|v| v.to_le_bytes()).collect();
        bus.load_image(info_addr, &info_bytes);

        let bus = Arc::new(bus);
        let mut jit_stats = None;
        let mut harts: Vec<Hart> = shared
            .iter()
            .enumerate()
            .map(|(id, sh)| {
                let cfg =
                    HartConfig { hartid: id as u64, max_satp_mode: config.max_satp_mode, vlen: config.vlen };
                let mut hart = Hart::new(cfg, bus.clone(), sh.clone(), time.clone());
                hart.reset(RAM_BASE);
                // OpenSBI fw_dynamic boot protocol.
                hart.state.x[10] = id as u64;
                hart.state.x[11] = dtb_addr;
                hart.state.x[12] = info_addr;
                hart
            })
            .collect();
        if config.jit {
            // Background compilation uses one pool shared by all harts; synchronous
            // compilation (for comparisons) gives each hart its own compiler.
            let pool = if config.jit_background {
                let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
                // Desktops compile a lot of code (guest JITs, browsers): use spare host cores.
                let workers = (2 * num_harts).min(cores.saturating_sub(num_harts)).max(1);
                // One cache as big as the per-hart caches together (virtual memory until used).
                let cache_size = config.jit_cache_size.saturating_mul(num_harts);
                let pool = rv_jit::pool::JitPool::new(num_harts, workers, cache_size).map_err(Error::Jit)?;
                jit_stats = Some(vec![pool.stats()]);
                Some(pool)
            } else {
                None
            };
            for (id, hart) in harts.iter_mut().enumerate() {
                let compiler: Box<dyn rv_core::jit::BlockCompiler> = match &pool {
                    Some(pool) => Box::new(pool.handle(id)),
                    None => {
                        let c =
                            rv_jit::Compiler::with_cache_size(config.jit_cache_size).map_err(Error::Jit)?;
                        jit_stats.get_or_insert_with(Vec::new).push(c.stats());
                        Box::new(c)
                    }
                };
                hart.compiler = Some(compiler);
                hart.jit_threshold = config.jit_threshold;
            }
        }

        Ok(Machine {
            bus,
            harts,
            clint,
            plic,
            uart,
            pci,
            gpu,
            keyboard,
            tablet,
            audio,
            forwarded,
            power,
            dtb,
            stats: Stats::default(),
            jit_stats,
            threaded: config.threaded,
        })
    }

    /// Runs until the guest powers off or resets the machine: each hart on its own host thread,
    /// or all of them round-robin on the calling thread if `config.threaded` was off.
    pub fn run(&mut self) -> Shutdown {
        if !self.threaded || self.harts.len() == 1 {
            return self.run_sequential();
        }
        let (clint, power) = (&*self.clint, &*self.power);
        let per_hart: Vec<Stats> = std::thread::scope(|scope| {
            let threads: Vec<_> = self
                .harts
                .iter_mut()
                .enumerate()
                .map(|(id, hart)| {
                    std::thread::Builder::new()
                        .name(format!("hart{id}"))
                        .spawn_scoped(scope, move || {
                            let mut stats = Stats::default();
                            loop {
                                let started = Instant::now();
                                let exit = hart.run(SLICE);
                                stats.busy += started.elapsed();
                                if service_hart(clint, power, id, hart, exit == Exit::Wfi, &mut stats) {
                                    return stats;
                                }
                            }
                        })
                        .expect("failed to spawn a hart thread")
                })
                .collect();
            threads.into_iter().map(|t| t.join().expect("hart thread panicked")).collect()
        });
        for s in per_hart {
            self.stats.busy += s.busy;
            self.stats.idle += s.idle;
        }
        self.power.reason().expect("hart threads only stop on shutdown")
    }

    /// Runs every hart in turn on the calling thread. Deterministic apart from timer interrupts,
    /// which follow the host clock.
    pub fn run_sequential(&mut self) -> Shutdown {
        loop {
            let mut exits = Vec::with_capacity(self.harts.len());
            for hart in &mut self.harts {
                let started = Instant::now();
                exits.push(hart.run(SLICE));
                self.stats.busy += started.elapsed();
            }
            if let Some(reason) = self.tick(&exits) {
                return reason;
            }
        }
    }

    /// Housekeeping after every hart ran a slice in sequential mode: timers, shutdown requests,
    /// and idling (for at most `MAX_IDLE`) when every hart is waiting in WFI.
    pub fn tick(&mut self, exits: &[Exit]) -> Option<Shutdown> {
        self.clint.update_timers();
        for hart in &mut self.harts {
            hart.update_timer_interrupts();
        }
        if let Some(reason) = self.power.reason() {
            return Some(reason);
        }
        if exits.iter().all(|&e| e == Exit::Wfi) {
            let now = self.harts[0].mtime();
            let deadline = (0..self.harts.len())
                .map(|h| self.clint.mtimecmp(h).min(self.harts[h].timer_deadline().unwrap_or(u64::MAX)))
                .min()
                .unwrap_or(u64::MAX);
            if deadline > now {
                let started = Instant::now();
                // Any hart's wake-up also ends the wait, since interrupts kick their target hart.
                self.harts[0].shared.wait(idle_time(deadline - now));
                self.stats.idle += started.elapsed();
            }
        }
        None
    }
}

/// How long to sleep for `ticks` of `mtime`, capped at `MAX_IDLE`.
fn idle_time(ticks: u64) -> Duration {
    Duration::from_nanos(ticks.saturating_mul(1_000_000_000 / TIMEBASE_HZ)).min(MAX_IDLE)
}

/// Per-hart housekeeping in threaded mode. Returns `true` when the machine shuts down.
fn service_hart(
    clint: &clint::Clint,
    power: &power::PowerState,
    id: usize,
    hart: &mut Hart,
    in_wfi: bool,
    stats: &mut Stats,
) -> bool {
    clint.update_timer(id);
    hart.update_timer_interrupts();
    if power.reason().is_some() {
        return true;
    }
    if in_wfi {
        let now = hart.mtime();
        let deadline = clint.mtimecmp(id).min(hart.timer_deadline().unwrap_or(u64::MAX));
        if deadline > now {
            let started = Instant::now();
            hart.shared.wait(idle_time(deadline - now));
            stats.idle += started.elapsed();
        }
        clint.update_timer(id);
        hart.update_timer_interrupts();
    }
    false
}
