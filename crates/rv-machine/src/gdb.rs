//! GDB remote serial protocol stub.
//!
//! Supports registers (x0-x31, pc, privilege and the main trap CSRs), memory access through
//! the current address translation, continue, single-step, software/hardware breakpoints and
//! Ctrl-C. Breakpoints are checked by single-stepping, which is slow but keeps the fast path
//! free of debugger checks.

use std::collections::BTreeSet;
use std::io::{self, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::Instant;

use rv_core::mmu::Access;
use rv_core::{Exit, Hart};

use crate::{Machine, SLICE, Shutdown};

/// Registers past x0-x31 and pc, in remote register-number order starting at 33.
const EXTRA_REGS: &[(&str, Option<u16>)] = &[
    ("priv", None),
    ("mstatus", Some(0x300)),
    ("mepc", Some(0x341)),
    ("mcause", Some(0x342)),
    ("mtval", Some(0x343)),
    ("mtvec", Some(0x305)),
    ("sstatus", Some(0x100)),
    ("sepc", Some(0x141)),
    ("scause", Some(0x142)),
    ("stval", Some(0x143)),
    ("stvec", Some(0x105)),
    ("satp", Some(0x180)),
];

const ABI: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "fp", "s1", "a0", "a1", "a2", "a3", "a4", "a5", "a6",
    "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
];

fn target_xml() -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\"?><!DOCTYPE target SYSTEM \"gdb-target.dtd\"><target version=\"1.0\">\
         <architecture>riscv:rv64</architecture><feature name=\"org.gnu.gdb.riscv.cpu\">",
    );
    for (i, name) in ABI.iter().enumerate() {
        let ty = match i {
            1 => "code_ptr",
            2 | 8 => "data_ptr",
            _ => "int",
        };
        xml += &format!("<reg name=\"{name}\" bitsize=\"64\" type=\"{ty}\" regnum=\"{i}\"/>");
    }
    xml += "<reg name=\"pc\" bitsize=\"64\" type=\"code_ptr\" regnum=\"32\"/></feature>";
    xml += "<feature name=\"org.gnu.gdb.riscv.virtual\"><reg name=\"priv\" bitsize=\"64\" regnum=\"33\"/></feature>";
    xml += "<feature name=\"org.gnu.gdb.riscv.csr\">";
    for (i, (name, _)) in EXTRA_REGS.iter().enumerate().skip(1) {
        xml += &format!("<reg name=\"{name}\" bitsize=\"64\" regnum=\"{}\" group=\"system\"/>", 33 + i);
    }
    xml + "</feature></target>"
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s, 16).ok()
}

enum Resume {
    Continue,
    Step,
}

/// Why the target stopped, as reported to GDB.
#[derive(Clone, Copy)]
enum Stop {
    Trap,
    Breakpoint,
    Interrupt,
}

impl Stop {
    fn reply(self) -> &'static str {
        match self {
            Stop::Trap => "S05",
            Stop::Breakpoint => "T05swbreak:;",
            Stop::Interrupt => "S02",
        }
    }
}

struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    no_ack: bool,
}

enum Incoming {
    Packet(String),
    Interrupt,
}

impl Connection {
    fn read_byte(&mut self) -> io::Result<u8> {
        let mut b = [0];
        self.reader.read_exact(&mut b)?;
        Ok(b[0])
    }

    fn recv(&mut self) -> io::Result<Incoming> {
        loop {
            match self.read_byte()? {
                0x03 => return Ok(Incoming::Interrupt),
                b'$' => break,
                _ => continue,
            }
        }
        let mut data = Vec::new();
        loop {
            match self.read_byte()? {
                b'#' => break,
                b => data.push(b),
            }
        }
        let checksum = [self.read_byte()?, self.read_byte()?];
        if !self.no_ack {
            let expected = data.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            let ok = std::str::from_utf8(&checksum).ok().and_then(|s| u8::from_str_radix(s, 16).ok())
                == Some(expected);
            self.writer.write_all(if ok { b"+" } else { b"-" })?;
        }
        Ok(Incoming::Packet(String::from_utf8_lossy(&data).into_owned()))
    }

    fn send(&mut self, data: &str) -> io::Result<()> {
        let sum = data.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        self.writer.write_all(format!("${data}#{sum:02x}").as_bytes())?;
        if !self.no_ack {
            // Wait for the acknowledgement.
            while self.read_byte()? != b'+' {}
        }
        Ok(())
    }

    /// Whether GDB sent Ctrl-C while the target was running.
    fn interrupted(&mut self) -> bool {
        let _ = self.writer.set_nonblocking(true);
        let mut b = [0];
        let got = matches!(self.writer.peek(&mut b), Ok(1));
        let _ = self.writer.set_nonblocking(false);
        got && b[0] == 0x03 && self.read_byte().is_ok()
    }
}

pub struct GdbStub {
    conn: Connection,
    breakpoints: BTreeSet<u64>,
}

fn read_reg(hart: &mut Hart, n: usize) -> Option<u64> {
    match n {
        0..32 => Some(hart.state.x[n]),
        32 => Some(hart.state.pc),
        _ => match EXTRA_REGS.get(n - 33)? {
            (_, None) => Some(hart.state.privilege as u64),
            (_, Some(csr)) => hart.debug_read_csr(*csr),
        },
    }
}

fn read_mem(hart: &mut Hart, addr: u64, len: u64) -> Option<Vec<u8>> {
    let p = hart.state.privilege;
    (0..len)
        .map(|i| {
            let va = addr.wrapping_add(i);
            let pa = hart.translate(va, Access::Load, p).ok()?;
            hart.bus.read(pa, 1).map(|b| b as u8)
        })
        .collect()
}

fn write_mem(hart: &mut Hart, addr: u64, data: &[u8]) -> bool {
    let p = hart.state.privilege;
    data.iter().enumerate().all(|(i, &b)| {
        let va = addr.wrapping_add(i as u64);
        // Use load permissions so that breakpoints can be planted in read-only code.
        hart.translate(va, Access::Load, p).is_ok_and(|pa| hart.bus.write(pa, 1, b as u64))
    })
}

impl GdbStub {
    /// Listens on `addr` and blocks until GDB connects.
    pub fn listen(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        eprint!("rust-v: waiting for gdb on {}\r\n", listener.local_addr()?);
        Self::accept(&listener)
    }

    /// Blocks until GDB connects to `listener`.
    pub fn accept(listener: &TcpListener) -> io::Result<Self> {
        let (stream, peer) = listener.accept()?;
        eprint!("rust-v: gdb connected from {peer}\r\n");
        stream.set_nodelay(true)?;
        let conn = Connection { reader: BufReader::new(stream.try_clone()?), writer: stream, no_ack: false };
        Ok(GdbStub { conn, breakpoints: BTreeSet::new() })
    }

    /// Runs the machine under debugger control, starting halted.
    pub fn run(mut self, machine: &mut Machine) -> io::Result<Shutdown> {
        let mut stop: Option<Stop> = None;
        loop {
            let resume = match self.serve(machine, stop.take())? {
                Some(r) => r,
                None => {
                    // Detached: continue without the debugger.
                    return Ok(machine.run_sequential());
                }
            };
            let outcome = match resume {
                Resume::Step => {
                    for hart in &mut machine.harts {
                        hart.step();
                    }
                    machine.tick(&[Exit::Budget]).map(Err).unwrap_or(Ok(Stop::Trap))
                }
                Resume::Continue => self.continue_until_stop(machine),
            };
            match outcome {
                Ok(s) => stop = Some(s),
                Err(shutdown) => {
                    let code = if shutdown == Shutdown::PowerOff { 0 } else { 1 };
                    let _ = self.conn.send(&format!("W{code:02x}"));
                    return Ok(shutdown);
                }
            }
        }
    }

    /// Runs all harts round-robin. Breakpoints and Ctrl-C apply to hart 0, which is the one
    /// the debugger sees.
    fn continue_until_stop(&mut self, machine: &mut Machine) -> Result<Stop, Shutdown> {
        loop {
            let started = Instant::now();
            let exits: Vec<Exit> = if self.breakpoints.is_empty() {
                machine.harts.iter_mut().map(|h| h.run(SLICE)).collect()
            } else {
                for _ in 0..SLICE {
                    for hart in &mut machine.harts {
                        hart.step();
                    }
                    if self.breakpoints.contains(&machine.harts[0].state.pc) {
                        machine.stats.busy += started.elapsed();
                        return Ok(Stop::Breakpoint);
                    }
                    if machine.harts.iter().all(|h| h.wfi) {
                        break;
                    }
                }
                machine.harts.iter().map(|h| if h.wfi { Exit::Wfi } else { Exit::Budget }).collect()
            };
            machine.stats.busy += started.elapsed();
            if let Some(shutdown) = machine.tick(&exits) {
                return Err(shutdown);
            }
            if self.conn.interrupted() {
                return Ok(Stop::Interrupt);
            }
        }
    }

    /// Handles packets while halted. Returns how to resume, or `None` on detach.
    fn serve(&mut self, machine: &mut Machine, stop: Option<Stop>) -> io::Result<Option<Resume>> {
        if let Some(stop) = stop {
            self.conn.send(stop.reply())?;
        }
        loop {
            let packet = match self.conn.recv()? {
                Incoming::Interrupt => {
                    self.conn.send(Stop::Interrupt.reply())?;
                    continue;
                }
                Incoming::Packet(p) => p,
            };
            let hart = &mut machine.harts[0];
            let reply: String = match packet.as_str() {
                "?" => Stop::Trap.reply().into(),
                "c" | "vCont;c" => return Ok(Some(Resume::Continue)),
                "s" | "vCont;s" => return Ok(Some(Resume::Step)),
                p if p.starts_with("vCont;s") => return Ok(Some(Resume::Step)),
                p if p.starts_with("vCont;c") => return Ok(Some(Resume::Continue)),
                "vCont?" => "vCont;c;C;s;S".into(),
                "D" => {
                    self.conn.send("OK")?;
                    return Ok(None);
                }
                "k" => std::process::exit(0),
                "g" => {
                    let regs: Vec<u8> =
                        (0..33).flat_map(|n| read_reg(hart, n).unwrap().to_le_bytes()).collect();
                    hex(&regs)
                }
                p if p.starts_with('G') => match unhex(&p[1..]) {
                    Some(bytes) if bytes.len() >= 33 * 8 => {
                        for (n, chunk) in bytes.as_chunks::<8>().0.iter().take(33).enumerate() {
                            let v = u64::from_le_bytes(*chunk);
                            match n {
                                0 => {}
                                32 => hart.state.pc = v,
                                _ => hart.state.x[n] = v,
                            }
                        }
                        "OK".into()
                    }
                    _ => "E01".into(),
                },
                p if p.starts_with('p') => {
                    match parse_hex(&p[1..]).and_then(|n| read_reg(hart, n as usize)) {
                        Some(v) => hex(&v.to_le_bytes()),
                        None => "E01".into(),
                    }
                }
                p if p.starts_with('P') => {
                    let parsed = p[1..].split_once('=').and_then(|(n, v)| {
                        let v = unhex(v).filter(|b| b.len() == 8)?;
                        Some((parse_hex(n)?, u64::from_le_bytes(v.try_into().ok()?)))
                    });
                    match parsed {
                        Some((n @ 1..32, v)) => {
                            hart.state.x[n as usize] = v;
                            "OK".into()
                        }
                        Some((32, v)) => {
                            hart.state.pc = v;
                            "OK".into()
                        }
                        Some((0, _)) => "OK".into(),
                        _ => "E01".into(),
                    }
                }
                p if p.starts_with('m') => {
                    let parsed =
                        p[1..].split_once(',').and_then(|(a, l)| Some((parse_hex(a)?, parse_hex(l)?)));
                    match parsed.and_then(|(a, l)| read_mem(hart, a, l.min(0x1000))) {
                        Some(bytes) => hex(&bytes),
                        None => "E14".into(),
                    }
                }
                p if p.starts_with('M') => {
                    let parsed = p[1..].split_once(':').and_then(|(head, data)| {
                        let (a, _) = head.split_once(',')?;
                        Some((parse_hex(a)?, unhex(data)?))
                    });
                    match parsed {
                        Some((a, data)) if write_mem(hart, a, &data) => "OK".into(),
                        _ => "E14".into(),
                    }
                }
                p if p.starts_with("Z0,")
                    || p.starts_with("Z1,")
                    || p.starts_with("z0,")
                    || p.starts_with("z1,") =>
                {
                    match p[3..].split(',').next().and_then(parse_hex) {
                        Some(addr) => {
                            if p.starts_with('Z') {
                                self.breakpoints.insert(addr);
                            } else {
                                self.breakpoints.remove(&addr);
                            }
                            "OK".into()
                        }
                        None => "E01".into(),
                    }
                }
                p if p.starts_with("qSupported") => {
                    "PacketSize=4000;qXfer:features:read+;QStartNoAckMode+;swbreak+;hwbreak+".into()
                }
                "QStartNoAckMode" => {
                    self.conn.send("OK")?;
                    self.conn.no_ack = true;
                    continue;
                }
                p if p.starts_with("qXfer:features:read:target.xml:") => {
                    let xml = target_xml();
                    let range = p.rsplit(':').next().and_then(|r| r.split_once(','));
                    match range.and_then(|(o, l)| Some((parse_hex(o)? as usize, parse_hex(l)? as usize))) {
                        Some((off, len)) => {
                            let chunk = xml.get(off.min(xml.len())..(off + len).min(xml.len())).unwrap_or("");
                            let more = off + len < xml.len();
                            format!("{}{chunk}", if more { 'm' } else { 'l' })
                        }
                        None => "E01".into(),
                    }
                }
                "qAttached" => "1".into(),
                "qC" => "QC1".into(),
                "qfThreadInfo" => "m1".into(),
                "qsThreadInfo" => "l".into(),
                p if p.starts_with('H') => "OK".into(),
                p if p.starts_with('T') => "OK".into(),
                _ => String::new(),
            };
            self.conn.send(&reply)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex(&[0x00, 0xab, 0x10]), "00ab10");
        assert_eq!(unhex("00ab10"), Some(vec![0x00, 0xab, 0x10]));
        assert_eq!(unhex("0"), None);
    }

    #[test]
    fn target_description_is_well_formed() {
        let xml = target_xml();
        assert!(xml.contains("<reg name=\"pc\""));
        assert!(xml.contains("regnum=\"44\""), "last CSR gets regnum 33 + 11");
        assert!(xml.ends_with("</target>"));
    }
}
