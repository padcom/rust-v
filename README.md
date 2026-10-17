# rust-v

A cross-platform RISC-V system emulator for a 4-core RVA23 machine, aiming to run Ubuntu
Desktop. See [PLAN.md](PLAN.md) for the roadmap and current status.

## Building

Requires a stable Rust toolchain.

```sh
cargo build --release
```

## Running Linux

Guest images for bring-up (OpenSBI, a Linux kernel and a minimal initramfs) are built inside a
Debian container, so you need `podman` or `docker`:

```sh
scripts/build-guest-images.sh        # writes third_party/guest/{fw_dynamic.bin,Image,initramfs.cpio}

G=third_party/guest
target/release/rust-v --bios $G/fw_dynamic.bin --kernel $G/Image --initrd $G/initramfs.cpio
```

Press `Ctrl-A x` to quit; `poweroff` in the guest shell exits too. Useful options:

| Option | Meaning |
|---|---|
| `-m, --memory 1G` | RAM size |
| `--smp 4` | number of harts (CPUs), each on its own host thread |
| `--single-thread` | run all harts round-robin on one thread (reproducible) |
| `--vlen 256` | vector register length (128, 256 or 512 bits) |
| `--append "..."` | kernel command line |
| `--mmu sv39\|sv48\|sv57` | largest virtual-memory mode offered to the guest |
| `--no-jit` | interpret only (the JIT compiles hot code by default) |
| `--gdb 1234` | wait for a GDB (or `lldb` `gdb-remote`) connection, starting halted |
| `--stats` | print instruction count and MIPS on exit |
| `--dump-dtb file.dtb` | save the generated device tree |

## Running Ubuntu Server

The official Ubuntu RISC-V preinstalled server image boots through OpenSBI, U-Boot and GRUB:

```sh
scripts/build-guest-images.sh   # OpenSBI (and the bring-up kernel)
scripts/build-uboot.sh          # third_party/guest/u-boot.bin
scripts/fetch-ubuntu.sh         # downloads and checks the image, grows it to 16 GiB

G=third_party/guest
target/release/rust-v --bios $G/fw_dynamic.bin --kernel $G/u-boot.bin -m 4G \
  --disk third_party/ubuntu/ubuntu-26.10-server-riscv64.img --forward 2222:22
```

Log in as `ubuntu` / `ubuntu` (you are asked to choose a new password). The guest gets a
network through user-mode NAT; `--forward` exposes guest ports on the host's loopback.
`scripts/ubuntu-smoke.py` boots the image with writes kept in memory, logs in, runs
`apt-get update` and reports how long each step took.

Devices and networking options:

| Option | Meaning |
|---|---|
| `--disk PATH[,ro][,snapshot]` | raw image as a virtio-blk disk; `snapshot` keeps writes in memory |
| `--forward [ADDR:]PORT:GUEST_PORT` | forward a host TCP port into the guest (repeatable) |
| `--no-net` | no network card |

The virtual network matches QEMU's: the guest is 10.0.2.15, the gateway 10.0.2.2 (which
reaches the host's loopback interface) and the DNS server 10.0.2.3.

## Running Ubuntu Desktop

The official Ubuntu RISC-V desktop live ISO boots the same way, with a display:

```sh
scripts/fetch-ubuntu.sh desktop   # third_party/ubuntu/ubuntu-26.10-desktop-riscv64.iso

G=third_party/guest
target/release/rust-v --bios $G/fw_dynamic.bin --kernel $G/u-boot.bin -m 8G \
  --disk third_party/ubuntu/ubuntu-26.10-desktop-riscv64.iso,ro --display window
```

The window shows the guest's virtio-gpu output scaled to fit; keyboard and mouse go to the
guest through virtio-input (the mouse is an absolute tablet, so it needs no grabbing), and
sound plays through the host's default output. GNOME renders with llvmpipe, so the first boot
of the live session takes several minutes. `scripts/desktop-smoke.py` boots the ISO headless
with a serial console added and takes screenshots.

| Option | Meaning |
|---|---|
| `--display window\|headless\|none` | desktop window; devices without a window; serial only (default) |
| `--resolution 1920x1080` | initial display size (the guest is offered the window size after resizing) |
| `--screenshot shot.png` | save the display every two seconds while it changes |
| `--no-sound` | no sound card (one comes with a display) |

## Testing

```sh
cargo test --workspace                       # unit and protocol tests

scripts/build-riscv-tests.sh                 # needs riscv64-elf-gcc (or set RISCV_PREFIX)
cargo test --release -p rv-conformance -- --ignored    # interpreter and JIT

cargo test --release -p rv-machine -- --ignored boot_linux   # needs the guest images

scripts/build-testfloat-vectors.sh           # Berkeley TestFloat vectors (container)
cargo test --release -p rv-softfloat -- --ignored

scripts/build-vector-tests.sh                # Spike + riscv-vector-tests (container, slow)
cargo test --release -p rv-conformance -- --ignored vector_tests
```

## Layout

| Crate | Contents |
|---|---|
| `rv-isa` | instruction decoder and disassembler |
| `rv-softfloat` | IEEE 754 binary16/32/64 with RISC-V rounding, flags and NaNs |
| `rv-core` | hart state, CSRs, traps, MMU, FPU, vector unit, decoded-block cache, interpreter |
| `rv-elf` | minimal ELF loader |
| `rv-jit` | Cranelift-based block compiler |
| `rv-net` | user-mode networking: DHCP, DNS, UDP NAT, TCP through smoltcp |
| `rv-conformance` | riscv-tests runner for interpreter and JIT |
| `rv-machine` | the board: CLINT, PLIC, UART, RTC, power control, PCIe, virtio devices, device tree, GDB stub |
| `rust-v` | command-line front end |
