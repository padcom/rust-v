//! JIT-specific regression tests on bare-metal programs.

use std::time::Duration;

use rv_machine::{Machine, MachineConfig, Shutdown};

fn run(bios: &[u8], jit_threshold: u32) -> Shutdown {
    let config = MachineConfig {
        ram_size: 16 << 20,
        bios: bios.to_vec(),
        harts: 1,
        jit: true,
        jit_threshold,
        ..MachineConfig::default()
    };
    let mut machine = Machine::new(config, Box::new(std::io::sink())).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(machine.run()));
    rx.recv_timeout(Duration::from_secs(20)).expect("program did not finish")
}

/// `straddle.S`: a loop body falls through to a loop branch that straddles a page boundary. The
/// chain link from the body must not bypass the branch.
#[test]
fn chaining_does_not_skip_page_straddling_instructions() {
    for threshold in [1, 2, 16] {
        assert_eq!(
            run(include_bytes!("straddle.bin"), threshold),
            Shutdown::PowerOff,
            "threshold {threshold}"
        );
    }
}
