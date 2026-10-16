//! Multi-hart atomics and memory-ordering tests. The program (`smp.S`, assembled with GNU as)
//! runs on every hart from reset: an AMO counter, an LR/SC counter and a message-passing litmus
//! test between harts 0 and 1, then hart 0 reports the verdict through the test device.

use std::time::Duration;

use rv_machine::{Machine, MachineConfig, Shutdown};

#[rustfmt::skip]
const PROGRAM: &[u32] = &[0x00080437, 0x0014041b, 0x00c41413, 0x000052b7, 0xe202829b, 0x00100313, 0x0064202f, 0xfff28293, 0xfe029ce3, 0x04040493, 0x000052b7, 0xe202829b, 0x1004a32f, 0x00130313, 0x1864a3af, 0xfe039ae3, 0xfff28293, 0xfe0296e3, 0x08040913, 0x0c040993, 0x10040a13, 0x00100293, 0x00005e37, 0xe20e0e1b, 0x02050663, 0x00100e93, 0x05d51263, 0x00592023, 0x0110000f, 0x0059a023, 0x000a2303, 0xfe531ee3, 0x00128293, 0xfe5e54e3, 0x0240006f, 0x0009a303, 0xfe531ee3, 0x0220000f, 0x00092383, 0x06539463, 0x005a2023, 0x00128293, 0xfe5e52e3, 0x14040a93, 0x00100313, 0x006aa02f, 0x04051e63, 0x000aa303, 0x00400393, 0xfe731ce3, 0x000143b7, 0x8803839b, 0x00042303, 0x00731c63, 0x04042303, 0x00731e63, 0x00005337, 0x5553031b, 0x0240006f, 0x00013337, 0x3333031b, 0x0180006f, 0x00023337, 0x3333031b, 0x00c0006f, 0x00033337, 0x3333031b, 0x001003b7, 0x0063a023, 0x10500073, 0xffdff06f, ];

fn run(threaded: bool, jit: bool) -> Shutdown {
    run_image(PROGRAM.iter().flat_map(|w| w.to_le_bytes()).collect(), threaded, jit)
}

fn run_image(bios: Vec<u8>, threaded: bool, jit: bool) -> Shutdown {
    let config =
        MachineConfig { ram_size: 16 << 20, bios, harts: 4, threaded, jit, ..MachineConfig::default() };
    let mut machine = Machine::new(config, Box::new(std::io::sink())).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(machine.run()));
    rx.recv_timeout(Duration::from_secs(120)).expect("SMP test did not finish")
}

fn verdict(shutdown: Shutdown) {
    match shutdown {
        Shutdown::PowerOff => {}
        Shutdown::Fail(1) => panic!("AMO counter lost updates"),
        Shutdown::Fail(2) => panic!("LR/SC counter lost updates"),
        Shutdown::Fail(3) => panic!("message passing observed stale data after fences"),
        Shutdown::Fail(4) => panic!("FENCE.I did not make another hart's code change visible"),
        other => panic!("unexpected shutdown {other:?}"),
    }
}

#[test]
fn smp_threaded() {
    verdict(run(true, false));
}

#[test]
fn smp_sequential() {
    verdict(run(false, false));
}

#[test]
fn smp_threaded_jit() {
    verdict(run(true, true));
}

/// Cross-modifying code (`xmc.S`, built with `riscv64-elf-as` + `objcopy -O binary`): hart 1
/// rewrites a function hart 0 has already executed, through a store-TLB entry created before
/// the page held code; after FENCE.I hart 0 must run the new code.
#[test]
fn cross_modifying_code_needs_only_fence_i() {
    let image = include_bytes!("xmc.bin").to_vec();
    verdict(run_image(image.clone(), true, false));
    verdict(run_image(image, true, true));
}
