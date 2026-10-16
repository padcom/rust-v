//! Boots OpenSBI + Linux + the busybox initramfs built by `scripts/build-guest-images.sh`.
//!
//! Ignored by default because it needs the guest images:
//! `cargo test -p rv-machine --release -- --ignored boot_linux`

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rv_machine::{Drive, Machine, MachineConfig, Shutdown};

/// Console sink that the test can inspect while the machine runs.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// Waits until the console output contains `needle` after byte offset `from`.
    fn wait_for(&self, needle: &str, from: usize, timeout: Duration) -> usize {
        let start = Instant::now();
        loop {
            let text = self.text();
            if let Some(pos) = text.get(from..).and_then(|t| t.find(needle)) {
                return from + pos + needle.len();
            }
            assert!(start.elapsed() < timeout, "timed out waiting for {needle:?}; console:\n{text}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn image(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/guest").join(name);
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run scripts/build-guest-images.sh)", path.display()))
}

#[test]
#[ignore = "needs scripts/build-guest-images.sh"]
fn boot_linux() {
    boot(false, rv_jit_cache_default());
}

#[test]
#[ignore = "needs scripts/build-guest-images.sh"]
fn boot_linux_jit() {
    boot(true, rv_jit_cache_default());
}

/// A cache small enough to fill up many times during boot, exercising flushes while all four
/// harts compile in the background.
#[test]
#[ignore = "needs scripts/build-guest-images.sh"]
fn boot_linux_jit_small_cache() {
    boot(true, 1 << 20);
}

fn rv_jit_cache_default() -> usize {
    MachineConfig::default().jit_cache_size
}

fn boot(jit: bool, jit_cache_size: usize) {
    let console = Capture::default();
    let mut disk = vec![0u8; 4 << 20];
    disk[..16].copy_from_slice(b"RUST-V DISK TEST");
    let config = MachineConfig {
        drives: vec![Drive { backend: Box::new(disk), read_only: false }],
        bios: image("fw_dynamic.bin"),
        kernel: Some(image("Image")),
        initrd: Some(image("initramfs.cpio")),
        jit,
        jit_cache_size,
        // Compile aggressively so that as much of the boot as possible runs as native code.
        jit_threshold: 2,
        ..MachineConfig::default()
    };
    let mut machine = Machine::new(config, Box::new(console.clone())).unwrap();
    let uart = machine.uart.clone();
    let runner = std::thread::spawn(move || machine.run());

    let timeout = Duration::from_secs(120);
    let mut pos = console.wait_for("OpenSBI v", 0, timeout);
    pos = console.wait_for("Linux version", pos, timeout);
    pos = console.wait_for("rust-v: busybox userland ready", pos, timeout);
    pos = console.wait_for("# ", pos, timeout);

    uart.push_input(b"echo hello from the guest\n");
    pos = console.wait_for("hello from the guest\r\n", pos, timeout);
    uart.push_input(b"cat /proc/cpuinfo\n");
    pos = console.wait_for("rv64imafdc", pos, timeout);
    // Floating point in userspace: Newton's iteration for sqrt(2), formatted by glibc.
    uart.push_input(
        b"awk 'BEGIN { x = 1; for (i = 0; i < 8; i++) x = x / 2 + 1 / x; printf \"x=%.12f\\n\", x }'\n",
    );
    pos = console.wait_for("x=1.414213562373", pos, timeout);
    // All four harts are online and run work in parallel.
    uart.push_input(b"echo cpus=$(nproc)\n");
    pos = console.wait_for("cpus=4", pos, timeout);
    uart.push_input(
        b"for j in 1 2 3 4; do (i=0; while [ $i -lt 2000 ]; do i=$((i+1)); done; echo w$j=$i) & done; wait; echo all-$((2+2))\n",
    );
    // The marker is computed so that the echoed command line does not match it.
    pos = console.wait_for("all-4\r\n", pos, timeout);
    let text = console.text();
    for j in 1..=4 {
        assert!(text.contains(&format!("w{j}=2000")), "worker {j} did not finish");
    }
    // virtio-blk: read, then write and read back past the page cache.
    uart.push_input(b"head -c 16 /dev/vda; echo\n");
    pos = console.wait_for("RUST-V DISK TEST", pos, timeout);
    uart.push_input(
        b"echo written-$((1+1)) | dd of=/dev/vda bs=512 seek=8 conv=sync,fsync 2>/dev/null; \
          echo 3 > /proc/sys/vm/drop_caches; dd if=/dev/vda bs=512 skip=8 count=1 2>/dev/null | head -c 9; echo\n",
    );
    pos = console.wait_for("written-2", pos, timeout);
    // virtio-rng.
    uart.push_input(
        b"echo rng-$(cat /sys/class/misc/hw_random/rng_current)-$(head -c 8 /dev/hwrng | wc -c)\n",
    );
    pos = console.wait_for("rng-virtio_rng.0-8", pos, timeout);
    uart.push_input(b"poweroff\n");
    console.wait_for("reboot: Power down", pos, timeout);

    assert_eq!(runner.join().unwrap(), Shutdown::PowerOff);
}
