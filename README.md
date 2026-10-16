# rust-v

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
target/release/rust-v --bios $G/fw_dynamic.bin --kernel $G/Image --initrd $G/initramfs.cpio
```

Press `Ctrl-A x` to quit; `poweroff` in the guest shell exits too. Useful options:

| Option | Meaning |
|---|---|
| `-m, --memory 1G` | RAM size |
| `--vlen 256` | vector register length (128, 256 or 512 bits) |
| `--append "..."` | kernel command line |
| `--mmu sv39\|sv48\|sv57` | largest virtual-memory mode offered to the guest |
| `--jit` | compile hot code to native instructions (experimental) |
| `--gdb 1234` | wait for a GDB (or `lldb` `gdb-remote`) connection, starting halted |
| `--stats` | print instruction count and MIPS on exit |
| `--dump-dtb file.dtb` | save the generated device tree |

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
| `rv-conformance` | riscv-tests runner for interpreter and JIT |
| `rv-machine` | the board: CLINT, PLIC, UART, RTC, power control, device tree, GDB stub |
| `rust-v` | command-line front end |
