//! Drives the GDB stub over TCP with a hand-written protocol client.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use rv_machine::gdb::GdbStub;
use rv_machine::{Machine, MachineConfig, Shutdown};

/// Counts a0 up to 100, then powers off through the SiFive test device:
///
/// ```text
///   li a0, 0
/// loop:
///   addi a0, a0, 1
///   li t0, 100
///   blt a0, t0, loop
///   lui t1, 0x100        # 0x100000: test device
///   li t2, 0x5555        # "pass"
///   sw t2, 0(t1)
/// 1: j 1b
/// ```
const PROGRAM: [u32; 9] = [
    0x00000513, 0x00150513, 0x06400293, 0xfe554ce3, 0x00100337, 0x000053b7, 0x55538393, 0x00732023,
    0x0000006f,
];

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn byte(&mut self) -> u8 {
        let mut b = [0];
        self.reader.read_exact(&mut b).unwrap();
        b[0]
    }

    fn request(&mut self, packet: &str) -> String {
        let sum = packet.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        self.writer.write_all(format!("${packet}#{sum:02x}").as_bytes()).unwrap();
        assert_eq!(self.byte(), b'+', "stub must acknowledge {packet:?}");
        while self.byte() != b'$' {}
        let mut reply = Vec::new();
        loop {
            match self.byte() {
                b'#' => break,
                b => reply.push(b),
            }
        }
        let (_, _) = (self.byte(), self.byte());
        self.writer.write_all(b"+").unwrap();
        String::from_utf8(reply).unwrap()
    }
}

fn le64(v: u64) -> String {
    v.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn gdb_session() {
    let bios: Vec<u8> = PROGRAM.iter().flat_map(|w| w.to_le_bytes()).collect();
    let config = MachineConfig { ram_size: 16 << 20, bios, ..MachineConfig::default() };
    let mut machine = Machine::new(config, Box::new(std::io::sink())).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || GdbStub::accept(&listener).unwrap().run(&mut machine).unwrap());

    let stream = TcpStream::connect(addr).unwrap();
    let mut gdb = Client { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream };

    assert!(gdb.request("qSupported:swbreak+").contains("qXfer:features:read+"));
    assert!(gdb.request("qXfer:features:read:target.xml:0,ffff").starts_with("l<?xml"));
    assert_eq!(gdb.request("?"), "S05");
    assert_eq!(gdb.request("p20"), le64(0x8000_0000), "starts halted at the reset vector");

    assert_eq!(gdb.request("s"), "S05");
    assert_eq!(gdb.request("p20"), le64(0x8000_0004));

    assert_eq!(gdb.request("Z0,80000010,4"), "OK");
    assert_eq!(gdb.request("c"), "T05swbreak:;");
    assert_eq!(gdb.request("p20"), le64(0x8000_0010));
    assert_eq!(gdb.request("pa"), le64(100), "a0 counted to 100");

    assert_eq!(gdb.request("m80000000,4"), "13050000");
    assert_eq!(gdb.request("M80000100,4:deadbeef"), "OK");
    assert_eq!(gdb.request("m80000100,4"), "deadbeef");
    assert_eq!(gdb.request("Pa=0700000000000000"), "OK");
    assert_eq!(gdb.request("pa"), le64(7));

    assert_eq!(gdb.request("z0,80000010,4"), "OK");
    assert_eq!(gdb.request("c"), "W00", "program powers off");
    assert_eq!(server.join().unwrap(), Shutdown::PowerOff);
}
