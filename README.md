# risc-vm

A cross-platform RISC-V system emulator, aiming at a 4-core RVA23 machine that runs Ubuntu
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
target/release/risc-vm --bios $G/fw_dynamic.bin --kernel $G/Image --initrd $G/initramfs.cpio
```

Press `Ctrl-A x` to quit; `poweroff` in the guest shell exits too. Useful options:

| Option | Meaning |
|---|---|
| `-m, --memory 1G` | RAM size |
| `--append "..."` | kernel command line |
| `--mmu sv39\|sv48\|sv57` | largest virtual-memory mode offered to the guest |
| `--gdb 1234` | wait for a GDB (or `lldb` `gdb-remote`) connection, starting halted |
| `--stats` | print instruction count and MIPS on exit |
| `--dump-dtb file.dtb` | save the generated device tree |

## Testing

```sh
cargo test --workspace                       # unit and protocol tests

scripts/build-riscv-tests.sh                 # needs riscv64-elf-gcc (or set RISCV_PREFIX)
cargo test --release -p rv-core -- --ignored riscv_tests

cargo test --release -p rv-machine -- --ignored boot_linux   # needs the guest images
```

## Layout

| Crate | Contents |
|---|---|
| `rv-isa` | instruction decoder and disassembler |
| `rv-core` | hart state, CSRs, traps, MMU, decoded-block cache, interpreter |
| `rv-elf` | minimal ELF loader |
| `rv-machine` | the board: CLINT, PLIC, UART, RTC, power control, device tree, GDB stub |
| `risc-vm` | command-line front end |
