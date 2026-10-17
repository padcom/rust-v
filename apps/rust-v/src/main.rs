use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

mod audio;
mod config;
mod gui;
mod image;
mod keymap;
mod screenshot;
use rv_machine::{Machine, MachineConfig, Shutdown};

/// RISC-V RVA23 system emulator.
///
/// `rust-v --disk ubuntu.img` boots a disk image with the bundled firmware (OpenSBI and
/// U-Boot). `--config vm.toml` reads options from a file (command-line options win).
/// `rust-v image --help` lists the disk image tools.
#[derive(Parser)]
#[command(version, about, args_override_self = true)]
struct Args {
    /// M-mode firmware loaded at the start of RAM. Default: the bundled OpenSBI
    /// (fw_dynamic.bin) from the firmware directory.
    #[arg(long)]
    bios: Option<PathBuf>,
    /// S-mode payload loaded at RAM + 2 MiB (a Linux Image, or U-Boot). Default with --disk:
    /// the bundled U-Boot, which boots the disk through its EFI boot loader.
    #[arg(long)]
    kernel: Option<PathBuf>,
    /// Initial ramdisk.
    #[arg(long)]
    initrd: Option<PathBuf>,
    /// Kernel command line.
    #[arg(long, default_value = "console=ttyS0 earlycon=sbi")]
    append: String,
    /// RAM size, e.g. 512M or 4G.
    #[arg(short, long, default_value = "512M", value_parser = parse_size)]
    memory: u64,
    /// Largest supported virtual-memory mode.
    #[arg(long, default_value = "sv48", value_parser = ["sv39", "sv48", "sv57"])]
    mmu: String,
    /// Number of harts (CPUs).
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=64))]
    smp: u16,
    /// Run all harts on one host thread, round-robin (slower, but reproducible).
    #[arg(long)]
    single_thread: bool,
    /// Vector register length in bits (128, 256 or 512).
    #[arg(long, default_value_t = 128, value_parser = parse_vlen)]
    vlen: usize,
    /// Interpret only: do not compile hot code to native instructions.
    #[arg(long)]
    no_jit: bool,
    /// Compile hot code to native instructions (the default; kept for compatibility).
    #[arg(long, hide = true)]
    jit: bool,
    /// Size of each hart's code cache in MiB.
    #[arg(long, hide = true)]
    jit_cache_mib: Option<usize>,
    /// Compile on the hart's own thread instead of a background thread (for comparison).
    #[arg(long, hide = true)]
    jit_sync: bool,
    /// Interpretations of a block before the JIT compiles it.
    #[arg(long, default_value_t = 16, hide = true)]
    jit_threshold: u32,
    /// Attach a raw disk image as a virtio-blk drive (repeatable). Options: `,ro` for read-only,
    /// `,snapshot` to keep writes in memory and leave the image unchanged.
    #[arg(long, value_name = "PATH[,ro][,snapshot]")]
    disk: Vec<String>,
    /// Where the guest display goes: `window` adds a GPU, keyboard and tablet shown in a
    /// desktop window; `none` (the default) is serial console only.
    /// `headless` adds the devices without a window (see --screenshot).
    #[arg(long, default_value = "none", value_parser = ["none", "window", "headless"])]
    display: String,
    /// Save the guest display to this PNG file every two seconds while it changes.
    #[arg(long)]
    screenshot: Option<PathBuf>,
    /// Initial display resolution, e.g. 1920x1080.
    #[arg(long, default_value = "1280x800", value_parser = parse_resolution)]
    resolution: (u32, u32),
    /// Do not add a sound card (one is added along with a display).
    #[arg(long)]
    no_sound: bool,
    /// Share a host directory with the guest over virtio-9p (repeatable). Mount it in the
    /// guest with `mount -t 9p -o trans=virtio,version=9p2000.L TAG /mnt`. Options: tag=NAME
    /// (default share0, share1, ...), ro, uid=N and gid=N (owner shown in the guest, 1000).
    #[arg(long, value_name = "DIR[,tag=NAME][,ro][,uid=N][,gid=N]")]
    share: Vec<String>,
    /// Do not add a network card.
    #[arg(long)]
    no_net: bool,
    /// Forward a host TCP port to a guest port, e.g. 2222:22 (repeatable).
    #[arg(long, value_name = "[HOST_ADDR:]HOST_PORT:GUEST_PORT", value_parser = parse_forward)]
    forward: Vec<(std::net::SocketAddr, u16)>,
    /// Wait for a GDB connection on this address (e.g. 1234 or 0.0.0.0:1234) and start halted.
    #[arg(long, value_name = "[HOST:]PORT")]
    gdb: Option<String>,
    /// Print execution statistics on exit.
    #[arg(long)]
    stats: bool,
    /// Write the generated device tree blob to this file.
    #[arg(long)]
    dump_dtb: Option<PathBuf>,
}

pub(crate) fn parse_size(s: &str) -> Result<u64, String> {
    let (digits, shift) = match s.char_indices().last() {
        Some((i, 'K' | 'k')) => (&s[..i], 10),
        Some((i, 'M' | 'm')) => (&s[..i], 20),
        Some((i, 'G' | 'g')) => (&s[..i], 30),
        _ => (s, 0),
    };
    let n: u64 = digits.parse().map_err(|_| format!("invalid size `{s}`"))?;
    n.checked_shl(shift).filter(|v| v % 4096 == 0 && *v > 0).ok_or(format!("invalid size `{s}`"))
}

fn parse_resolution(s: &str) -> Result<(u32, u32), String> {
    let err = || format!("invalid resolution `{s}` (expected WIDTHxHEIGHT)");
    let (w, h) = s.split_once('x').ok_or_else(err)?;
    let (w, h): (u32, u32) = (w.parse().map_err(|_| err())?, h.parse().map_err(|_| err())?);
    if !(320..=8192).contains(&w) || !(200..=8192).contains(&h) {
        return Err(err());
    }
    Ok((w, h))
}

fn parse_forward(s: &str) -> Result<(std::net::SocketAddr, u16), String> {
    let err = || format!("invalid forward `{s}` (expected [HOST_ADDR:]HOST_PORT:GUEST_PORT)");
    let (host, guest) = s.rsplit_once(':').ok_or_else(err)?;
    let guest: u16 = guest.parse().map_err(|_| err())?;
    let host = if host.contains(':') { host.to_string() } else { format!("127.0.0.1:{host}") };
    Ok((host.parse().map_err(|_| err())?, guest))
}

fn parse_vlen(s: &str) -> Result<usize, String> {
    match s.parse() {
        Ok(v @ (128 | 256 | 512)) => Ok(v),
        _ => Err(format!("unsupported VLEN `{s}` (use 128, 256 or 512)")),
    }
}

fn read(path: &PathBuf) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Puts the host terminal in raw mode for the lifetime of the guard.
struct RawTerminal;

impl RawTerminal {
    fn enable() -> Option<Self> {
        crossterm::terminal::enable_raw_mode().ok().map(|_| RawTerminal)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

const ESCAPE: u8 = 0x01; // Ctrl-A

/// Forwards host stdin to the guest UART. In raw mode, Ctrl-A x quits and Ctrl-A Ctrl-A sends
/// a literal Ctrl-A.
fn spawn_console_input(uart: Arc<rv_machine::uart::Uart>, raw: bool) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 256];
        let mut escaped = false;
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut out = Vec::with_capacity(n);
            for &b in &buf[..n] {
                if raw && escaped {
                    escaped = false;
                    match b {
                        b'x' | b'X' => {
                            let _ = crossterm::terminal::disable_raw_mode();
                            eprintln!("\r\nrust-v: terminated");
                            std::process::exit(0);
                        }
                        ESCAPE => out.push(ESCAPE),
                        _ => {}
                    }
                } else if raw && b == ESCAPE {
                    escaped = true;
                } else {
                    out.push(b);
                }
            }
            uart.push_input(&out);
        }
    });
}

fn open_disk(spec: &str) -> Result<rv_machine::Drive, String> {
    let mut parts = spec.split(',');
    let path = parts.next().unwrap_or_default();
    let (mut read_only, mut snapshot) = (false, false);
    for option in parts {
        match option {
            "ro" => read_only = true,
            "snapshot" => snapshot = true,
            _ => return Err(format!("{spec}: unknown disk option `{option}` (use ro or snapshot)")),
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(!read_only && !snapshot)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    let file = rv_machine::virtio::blk::FileBackend::new(file).map_err(|e| format!("{path}: {e}"))?;
    let backend: Box<dyn rv_machine::virtio::blk::Backend> = if snapshot {
        Box::new(rv_machine::virtio::blk::Overlay::new(Box::new(file)))
    } else {
        Box::new(file)
    };
    Ok(rv_machine::Drive { backend, read_only })
}

fn parse_share(index: usize, spec: &str) -> Result<rv_machine::Share, String> {
    let mut parts = spec.split(',');
    let path = PathBuf::from(parts.next().unwrap_or_default());
    let mut share =
        rv_machine::Share { path, tag: format!("share{index}"), read_only: false, uid: 1000, gid: 1000 };
    for option in parts {
        let bad = || format!("{spec}: unknown share option `{option}`");
        match option.split_once('=') {
            None if option == "ro" => share.read_only = true,
            Some(("tag", v)) if !v.is_empty() && v.len() <= 31 => share.tag = v.to_string(),
            Some(("uid", v)) => share.uid = v.parse().map_err(|_| bad())?,
            Some(("gid", v)) => share.gid = v.parse().map_err(|_| bad())?,
            _ => return Err(bad()),
        }
    }
    Ok(share)
}

fn run_machine(machine: &mut Machine, gdb: Option<&str>) -> Result<Shutdown, String> {
    match gdb {
        Some(addr) => {
            let addr = if addr.contains(':') { addr.to_string() } else { format!("127.0.0.1:{addr}") };
            let stub = rv_machine::gdb::GdbStub::listen(&addr).map_err(|e| format!("gdb: {addr}: {e}"))?;
            stub.run(machine).map_err(|e| format!("gdb: {e}"))
        }
        None => Ok(machine.run()),
    }
}

/// The firmware file `name` from the firmware directories.
fn bundled(name: &str) -> Result<PathBuf, String> {
    config::find_firmware(name).ok_or_else(|| {
        let dirs: Vec<String> = config::firmware_dirs().iter().map(|d| d.display().to_string()).collect();
        format!("{name} not found (looked in {}); pass it explicitly or set RUST_V_FIRMWARE", dirs.join(", "))
    })
}

fn run(args: Args) -> Result<Shutdown, String> {
    let bios = match &args.bios {
        Some(path) => path.clone(),
        None => bundled("fw_dynamic.bin")?,
    };
    let kernel = match &args.kernel {
        Some(path) => Some(path.clone()),
        None if !args.disk.is_empty() && args.initrd.is_none() => Some(bundled("u-boot.bin")?),
        None => None,
    };
    let config = MachineConfig {
        ram_size: args.memory,
        bios: read(&bios)?,
        kernel: kernel.as_ref().map(read).transpose()?,
        initrd: args.initrd.as_ref().map(read).transpose()?,
        cmdline: args.append,
        max_satp_mode: match args.mmu.as_str() {
            "sv39" => 8,
            "sv48" => 9,
            _ => 10,
        },
        jit: !args.no_jit,
        jit_threshold: args.jit_threshold,
        jit_background: !args.jit_sync,
        jit_cache_size: args.jit_cache_mib.map_or(MachineConfig::default().jit_cache_size, |m| m << 20),
        vlen: args.vlen,
        harts: args.smp as usize,
        threaded: !args.single_thread,
        drives: args.disk.iter().map(|d| open_disk(d)).collect::<Result<_, _>>()?,
        rng: true,
        net: (!args.no_net)
            .then(|| rv_machine::rv_net::Config { tcp_forwards: args.forward.clone(), ..Default::default() }),
        display: (args.display != "none")
            .then(|| rv_machine::display::Display::new(args.resolution.0, args.resolution.1)),
        sound: args.display != "none" && !args.no_sound,
        shares: args.share.iter().enumerate().map(|(i, s)| parse_share(i, s)).collect::<Result<_, _>>()?,
    };
    if let (Some(display), Some(path)) = (&config.display, &args.screenshot) {
        screenshot::spawn(display.clone(), path.clone(), std::time::Duration::from_secs(2));
    }
    let window = config.display.clone().filter(|_| args.display == "window");
    let mut machine = Machine::new(config, Box::new(std::io::stdout())).map_err(|e| e.to_string())?;
    if let Some(path) = &args.dump_dtb {
        std::fs::write(path, &machine.dtb).map_err(|e| format!("{}: {e}", path.display()))?;
    }

    // Without a host output the device still plays, into silence, at the right pace.
    let _audio = match machine.audio.clone().map(audio::start) {
        Some(Err(e)) => {
            eprintln!("rust-v: no sound output: {e}");
            None
        }
        other => other.and_then(Result::ok),
    };

    let interactive = std::io::stdin().is_terminal();
    let _raw = if interactive { RawTerminal::enable() } else { None };
    if interactive {
        eprint!("rust-v: press Ctrl-A x to quit\r\n");
    }
    spawn_console_input(machine.uart.clone(), interactive);
    let reason = match window {
        // The window's event loop must own the main thread (macOS requires it), so the
        // machine runs on another one.
        Some(display) => {
            let (event_loop, waker) = gui::event_loop()?;
            let devices = gui::Devices {
                display: display.clone(),
                gpu: machine.gpu.clone().expect("display without a GPU"),
                keyboard: machine.keyboard.clone().expect("display without a keyboard"),
                tablet: machine.tablet.clone().expect("display without a tablet"),
            };
            let redraw = waker.clone();
            display.set_waker(move || redraw.redraw());
            let quit = waker.clone();
            let gdb = args.gdb.clone();
            let runner = std::thread::Builder::new()
                .name("machine".into())
                .spawn(move || {
                    let reason = run_machine(&mut machine, gdb.as_deref());
                    quit.quit();
                    (machine, reason)
                })
                .map_err(|e| e.to_string())?;
            gui::run(devices, event_loop, waker);
            if !runner.is_finished() {
                // The window was closed while the guest was still running.
                drop(_raw);
                eprintln!("\r\nrust-v: window closed");
                std::process::exit(0);
            }
            let (finished, reason) = runner.join().map_err(|_| "machine thread panicked".to_string())?;
            machine = finished;
            reason?
        }
        None => run_machine(&mut machine, args.gdb.as_deref())?,
    };
    let _ = std::io::stdout().flush();
    if args.stats {
        let insns = machine.instructions();
        let busy = machine.stats.busy.as_secs_f64();
        eprint!(
            "\r\nrust-v: {insns} instructions, {busy:.2}s busy, {:.2}s idle, {:.1} MIPS\r\n",
            machine.stats.idle.as_secs_f64(),
            insns as f64 / busy / 1e6
        );
        if let Some(report) = rv_machine::vector_profile_report(30) {
            eprint!("{}", report.replace('\n', "\r\n"));
        }
        if let Some(per_hart) = &machine.jit_stats {
            use std::sync::atomic::Ordering::Relaxed;
            let sum = |f: fn(&rv_machine::JitStats) -> &std::sync::atomic::AtomicU64| {
                per_hart.iter().map(|s| f(s).load(Relaxed)).sum::<u64>()
            };
            let blocks = sum(|s| &s.blocks);
            let nanos = sum(|s| &s.nanos);
            eprint!(
                "rust-v: JIT compiled {blocks} blocks ({} instructions, {} KiB code, {} rejected, {} cache flushes), {:.1} us/block, {:.2}s total\r\n",
                sum(|s| &s.instructions),
                sum(|s| &s.code_bytes) / 1024,
                sum(|s| &s.rejected),
                sum(|s| &s.flushes),
                nanos as f64 / blocks.max(1) as f64 / 1e3,
                nanos as f64 / 1e9,
            );
        }
    }
    Ok(reason)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("image") {
        return match image::main(std::iter::once("rust-v image".to_string()).chain(argv.into_iter().skip(2)))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("rust-v: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let argv = match config::expand(argv) {
        Ok(argv) => argv,
        Err(e) => {
            eprintln!("rust-v: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run(Args::parse_from(argv)) {
        Ok(Shutdown::PowerOff) => ExitCode::SUCCESS,
        Ok(Shutdown::Reset) => {
            eprintln!("rust-v: guest requested reset (not supported yet), exiting");
            ExitCode::SUCCESS
        }
        Ok(Shutdown::Fail(code)) => {
            eprintln!("rust-v: guest reported failure {code}");
            ExitCode::from(code.clamp(1, 255) as u8)
        }
        Err(e) => {
            eprintln!("rust-v: {e}");
            ExitCode::FAILURE
        }
    }
}
