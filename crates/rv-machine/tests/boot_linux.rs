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

/// How long to wait for each step; `RUST_V_TEST_TIMEOUT` (seconds) raises it on slow or
/// emulated hosts.
fn test_timeout() -> Duration {
    Duration::from_secs(std::env::var("RUST_V_TEST_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(120))
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

/// Serves one HTTP response with body `{tag}-from-host` on a loopback port; returns the port.
fn serve_http_once(tag: &str) -> u16 {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = format!("{tag}-from-host");
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
            line.clear();
        }
        let mut stream = reader.into_inner();
        let _ = write!(stream, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    });
    port
}

fn rv_jit_cache_default() -> usize {
    MachineConfig::default().jit_cache_size
}

fn boot(jit: bool, jit_cache_size: usize) {
    let console = Capture::default();
    let mut disk = vec![0u8; 4 << 20];
    // Any free host port, forwarded to the guest's port 8080.
    let forward: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    disk[..16].copy_from_slice(b"RUST-V DISK TEST");
    // A host directory shared over virtio-9p.
    let share_dir =
        std::env::temp_dir().join(format!("rust-v-boot-share-{}-{jit}-{jit_cache_size}", std::process::id()));
    let _ = std::fs::remove_dir_all(&share_dir);
    std::fs::create_dir_all(&share_dir).unwrap();
    std::fs::write(share_dir.join("from-host.txt"), "shared-by-host").unwrap();
    let config = MachineConfig {
        shares: vec![rv_machine::Share {
            path: share_dir.clone(),
            tag: "hostshare".into(),
            read_only: false,
            uid: 1000,
            gid: 1000,
        }],
        drives: vec![Drive { backend: Box::new(disk), read_only: false }],
        net: Some(rv_machine::rv_net::Config { tcp_forwards: vec![(forward, 8080)], ..Default::default() }),
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
    let shared = share_dir.clone();
    let forward = machine.forwarded[0];
    let uart = machine.uart.clone();
    let runner = std::thread::spawn(move || machine.run());

    let timeout = test_timeout();
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
    // virtio-net with user-mode networking: DHCP, ping, DNS and HTTP to a server on the host,
    // reached through the gateway address.
    let port = serve_http_once("net-ok");
    uart.push_input(
        b"ip link set eth0 up; udhcpc -i eth0 -q -n -s /bin/true 2>&1 | grep -o 'lease of [0-9.]*'\n",
    );
    pos = console.wait_for("lease of 10.0.2.15", pos, timeout);
    uart.push_input(b"ip addr add 10.0.2.15/24 dev eth0; ip route add default via 10.0.2.2; ping -c 1 10.0.2.2 >/dev/null && echo ping-$((40+2))\n");
    pos = console.wait_for("ping-42", pos, timeout);
    uart.push_input(b"nslookup localhost 10.0.2.3 | grep -o 'Address: 127[.0-9]*'\n");
    pos = console.wait_for("Address: 127.0.0.1", pos, timeout);
    uart.push_input(format!("wget -q -O - http://10.0.2.2:{port}/; echo\n").as_bytes());
    pos = console.wait_for("net-ok-from-host", pos, timeout);
    // A host connection to the forwarded port reaches a guest listener.
    uart.push_input(b"(echo fwd-$((5*5)) | nc -l -p 8080 >/dev/null) & sleep 1; echo nc-$((4*4))\n");
    pos = console.wait_for("nc-16", pos, timeout);
    // Until nc listens (it may take longer than the second on a slow host), the guest resets
    // the connection and the reply is empty: try again.
    let deadline = std::time::Instant::now() + timeout;
    let reply = loop {
        let mut stream = std::net::TcpStream::connect(forward).unwrap();
        stream.set_read_timeout(Some(timeout)).unwrap();
        let mut reply = String::new();
        let _ = std::io::Read::read_to_string(&mut stream, &mut reply);
        if !reply.is_empty() || std::time::Instant::now() > deadline {
            break reply;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(reply, "fwd-25\n");
    // virtio-9p: read a host file, write one back.
    uart.push_input(
        b"mkdir -p /mnt/h && mount -t 9p -o trans=virtio,version=9p2000.L hostshare /mnt/h && \
          cat /mnt/h/from-host.txt && echo; echo written-by-guest > /mnt/h/from-guest.txt && sync; echo 9p-$((3+4))\n",
    );
    pos = console.wait_for("9p-7", pos, timeout);
    assert!(console.text().contains("shared-by-host"), "guest did not read the shared file");
    assert_eq!(std::fs::read_to_string(shared.join("from-guest.txt")).unwrap(), "written-by-guest\n");
    std::fs::remove_dir_all(&shared).unwrap();
    uart.push_input(b"poweroff\n");
    console.wait_for("reboot: Power down", pos, timeout);

    assert_eq!(runner.join().unwrap(), Shutdown::PowerOff);
}

/// A snapshot of a booted machine, restored into a new one, carries on where it was: the
/// shell, its variables, the disk's in-memory changes.
#[test]
#[ignore = "needs scripts/build-guest-images.sh"]
fn snapshot_and_restore() {
    let config = || MachineConfig {
        bios: image("fw_dynamic.bin"),
        kernel: Some(image("Image")),
        initrd: Some(image("initramfs.cpio")),
        drives: vec![Drive {
            backend: Box::new(rv_machine::virtio::blk::Overlay::new(Box::new(vec![0u8; 1 << 20]))),
            read_only: false,
        }],
        // Every kind of device, so that all of their state goes through a snapshot.
        net: Some(rv_machine::rv_net::Config::default()),
        display: Some(rv_machine::display::Display::new(640, 480)),
        sound: true,
        shares: vec![rv_machine::Share {
            path: std::env::temp_dir(),
            tag: "tmp".into(),
            read_only: true,
            uid: 0,
            gid: 0,
        }],
        ..MachineConfig::default()
    };
    let timeout = test_timeout();
    let path = std::env::temp_dir().join(format!("rust-v-test-{}.snapshot", std::process::id()));

    let console = Capture::default();
    let mut machine = Machine::new(config(), Box::new(console.clone())).unwrap();
    let (uart, snapshots) = (machine.uart.clone(), machine.snapshot_handle());
    let power = machine.power.clone();
    let runner = std::thread::spawn(move || machine.run());
    let mut pos = console.wait_for("# ", 0, timeout);
    uart.push_input(
        b"export MARK=$((40+2)); echo disk-$((1+1)) | dd of=/dev/vda conv=fsync 2>/dev/null; \
          mkdir -p /mnt/t && mount -t 9p -o trans=virtio,version=9p2000.L tmp /mnt/t; echo ready-$((2*5))\n",
    );
    pos = console.wait_for("ready-10", pos, timeout);
    snapshots.save(&path).unwrap();
    // The machine keeps running after the snapshot.
    uart.push_input(b"echo after-$((3*3))\n");
    console.wait_for("after-9", pos, timeout);
    power.request(Shutdown::PowerOff);
    runner.join().unwrap();

    let console = Capture::default();
    let mut machine = Machine::new(config(), Box::new(console.clone())).unwrap();
    machine.restore(&path).unwrap();
    let uart = machine.uart.clone();
    let runner = std::thread::spawn(move || machine.run());
    uart.push_input(
        b"echo mark=$MARK; head -c 6 /dev/vda; echo; ls /mnt/t > /dev/null && echo share-$((5+5))\n",
    );
    let pos = console.wait_for("mark=42", 0, timeout);
    let pos = console.wait_for("disk-2", pos, timeout);
    let pos = console.wait_for("share-10", pos, timeout);
    uart.push_input(b"poweroff\n");
    console.wait_for("reboot: Power down", pos, timeout);
    assert_eq!(runner.join().unwrap(), Shutdown::PowerOff);
    std::fs::remove_file(path).unwrap();
}
