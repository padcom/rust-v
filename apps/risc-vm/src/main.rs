use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use rv_machine::{Machine, MachineConfig, Shutdown};

/// RISC-V RVA23 system emulator.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// M-mode firmware loaded at the start of RAM (e.g. OpenSBI fw_dynamic.bin).
    #[arg(long)]
    bios: PathBuf,
    /// S-mode kernel image loaded at RAM + 2 MiB (e.g. a Linux Image).
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
    /// Compile hot code to native instructions (experimental).
    #[arg(long)]
    jit: bool,
    /// Interpretations of a block before the JIT compiles it.
    #[arg(long, default_value_t = 16, hide = true)]
    jit_threshold: u32,
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

fn parse_size(s: &str) -> Result<u64, String> {
    let (digits, shift) = match s.char_indices().last() {
        Some((i, 'K' | 'k')) => (&s[..i], 10),
        Some((i, 'M' | 'm')) => (&s[..i], 20),
        Some((i, 'G' | 'g')) => (&s[..i], 30),
        _ => (s, 0),
    };
    let n: u64 = digits.parse().map_err(|_| format!("invalid size `{s}`"))?;
    n.checked_shl(shift).filter(|v| v % 4096 == 0 && *v > 0).ok_or(format!("invalid size `{s}`"))
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
                            eprintln!("\r\nrisc-vm: terminated");
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

fn run(args: Args) -> Result<Shutdown, String> {
    let config = MachineConfig {
        ram_size: args.memory,
        bios: read(&args.bios)?,
        kernel: args.kernel.as_ref().map(read).transpose()?,
        initrd: args.initrd.as_ref().map(read).transpose()?,
        cmdline: args.append,
        max_satp_mode: match args.mmu.as_str() {
            "sv39" => 8,
            "sv48" => 9,
            _ => 10,
        },
        jit: args.jit,
        jit_threshold: args.jit_threshold,
    };
    let mut machine = Machine::new(config, Box::new(std::io::stdout())).map_err(|e| e.to_string())?;
    if let Some(path) = &args.dump_dtb {
        std::fs::write(path, &machine.dtb).map_err(|e| format!("{}: {e}", path.display()))?;
    }

    let interactive = std::io::stdin().is_terminal();
    let _raw = if interactive { RawTerminal::enable() } else { None };
    if interactive {
        eprint!("risc-vm: press Ctrl-A x to quit\r\n");
    }
    spawn_console_input(machine.uart.clone(), interactive);
    let reason = match &args.gdb {
        Some(addr) => {
            let addr = if addr.contains(':') { addr.clone() } else { format!("127.0.0.1:{addr}") };
            let stub = rv_machine::gdb::GdbStub::listen(&addr).map_err(|e| format!("gdb: {addr}: {e}"))?;
            stub.run(&mut machine).map_err(|e| format!("gdb: {e}"))?
        }
        None => machine.run(),
    };
    let _ = std::io::stdout().flush();
    if args.stats {
        let insns = machine.instructions();
        let busy = machine.stats.busy.as_secs_f64();
        eprint!(
            "\r\nrisc-vm: {insns} instructions, {busy:.2}s busy, {:.2}s idle, {:.1} MIPS\r\n",
            machine.stats.idle.as_secs_f64(),
            insns as f64 / busy / 1e6
        );
        if let Some(jit) = &machine.jit_stats {
            use std::sync::atomic::Ordering::Relaxed;
            let blocks = jit.blocks.load(Relaxed).max(1);
            eprint!(
                "risc-vm: JIT compiled {} blocks ({} instructions, {} KiB code, {} rejected), {:.1} us/block, {:.2}s total\r\n",
                jit.blocks.load(Relaxed),
                jit.instructions.load(Relaxed),
                jit.code_bytes.load(Relaxed) / 1024,
                jit.rejected.load(Relaxed),
                jit.nanos.load(Relaxed) as f64 / blocks as f64 / 1e3,
                jit.nanos.load(Relaxed) as f64 / 1e9,
            );
        }
    }
    Ok(reason)
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(Shutdown::PowerOff) => ExitCode::SUCCESS,
        Ok(Shutdown::Reset) => {
            eprintln!("risc-vm: guest requested reset (not supported yet), exiting");
            ExitCode::SUCCESS
        }
        Ok(Shutdown::Fail(code)) => {
            eprintln!("risc-vm: guest reported failure {code}");
            ExitCode::from(code.clamp(1, 255) as u8)
        }
        Err(e) => {
            eprintln!("risc-vm: {e}");
            ExitCode::FAILURE
        }
    }
}
