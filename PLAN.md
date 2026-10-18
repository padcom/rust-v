# rust-v — RVA23 system emulator: project plan

Goal: a cross-platform (macOS / Linux / Windows; x86-64 and AArch64 hosts) emulator
of a 4-core RISC-V RVA23 machine that boots the latest Ubuntu Desktop (RISC-V) and
is fast enough to use GNOME and a web browser.

## Status

| Phase | State |
|---|---|
| 0: Foundations | ✅ workspace, CI matrix, riscv-tests build |
| 1: Single-hart interpreter | ✅ RV64IMAC + M/S/U, Sv39/48/57, all 198 rv64 ui/um/ua/uc/mi/si riscv-tests pass. Spike lockstep and PMP enforcement still to do |
| 2: Board + Linux boot | ✅ OpenSBI 1.6 + Linux 6.12 boot to an init shell in ~1.7 s; decode cache (98 → 199 MIPS); GDB stub. The shell is a freestanding mini-init until F/D land (Debian userland is hard-float) |
| 2.5: JIT spike | ✅ Cranelift block JIT behind `--jit`: CoreMark 666 → 2272 it/s (3.4×), riscv-tests and Linux boot pass under the JIT. Findings and Phase 5 decisions in [docs/jit-spike.md](docs/jit-spike.md): chaining first (68% of hot time is dispatch), compile off-thread (~15 µs per guest instruction) |
| 3: Full RVA23 ISA | ✅ F/D/Zfa/Zfhmin on a TestFloat-verified softfloat (busybox/glibc userland runs); Zba/Zbb/Zbs/Zicond/Zcb/Zimop/Zcmop/Zawrs/Zicbo*/Svinval; Sstc, Svpbmt, Svnapot, Smstateen/Ssstateen, Smnpm/Ssnpm, Sscofpmf; RVV 1.0 + Zvfh/Zvfhmin/Zvbb (2993/2993 Spike-checked riscv-vector-tests). H deferred to Phase 10 |
| 4: SMP | ✅ 4 harts on host threads (or round-robin with `--single-thread`); OpenSBI HSM bring-up, IPIs, remote fences; atomics/LR-SC/message-passing litmus and cross-modifying-code tests; Linux reports 4 CPUs and scales 3.9× on a parallel workload |
| 5: Full JIT | ✅ on by default: tail-call block chaining, inline indirect-jump cache, background compilation per hart, own code cache (MAP_JIT on Apple Silicon) with flush-on-full, interpreter helper for FP/atomics/vector. CoreMark ~6400 it/s (2.1 GIPS, ~10× interpreter); Linux to init in 1.1 s on 4 harts. Lockstep interp-vs-JIT mode replaced by running every conformance suite in both modes (and with a tiny code cache) |
| 6: Virtio + Ubuntu Server | ✅ PCIe host bridge (ECAM, INTx on the PLIC), modern virtio-pci, virtio-blk (raw images, `,snapshot` copy-on-write), virtio-net on user-mode networking (DHCP, DNS through the host resolver, UDP NAT, TCP via smoltcp, host port forwarding), virtio-rng. OpenSBI → U-Boot → GRUB → the official Ubuntu 26.10 RISC-V server image: first boot (cloud-init) to the login prompt in ~55 s, `apt-get update` works. Along the way: privileged block terminators compiled, a block cache big enough for a distribution, page-at-a-time vector memory access. Deferred: virtio-console (Phase 7, with the other desktop devices), AIA/MSI-X, qcow2, an I/O thread for disks |
| 7: Desktop I/O + Ubuntu Desktop | ✅ virtio-gpu 2D (resources, scanout, flush, hardware cursor), virtio-input keyboard and absolute tablet, virtio-snd playback (cpal); a winit/softbuffer window with aspect-correct scaling, HiDPI and resize-to-guest, plus headless mode with PNG screenshots. The Ubuntu 26.10 desktop live ISO boots to GNOME (Wayland, llvmpipe) and runs Firefox (Wikipedia renders). Clipboard and virtio-console deferred |
| 8: Performance | 🟡 first round, see [docs/performance.md](docs/performance.md): shared code cache and compiler pool, two compile tiers with loop regions, native vector code (vtype-specialised), page-at-a-time vector memory. CoreMark 6650 → 10250 it/s; desktop vector work in the interpreter 18.9 G → 3.8 G instructions. Second round: GPU acceleration through virglrenderer (`--gpu virgl`, Linux and macOS hosts): kmscube 29 → 60 fps, GNOME Shell about a third less CPU; boot and Firefox unchanged. Milestone (comfortable browsing) not reached: FP vector JIT and a cheaper first tier remain, and Venus (Vulkan) for a fuller GPU path |
| 9: Product polish | ✅ (mostly) release packages per OS (`scripts/package.sh`, tagged CI builds), snapshots (Ctrl-A s, `--restore`), virtio-9p shared folders, `--config` TOML files, `rust-v image` create/resize/clone/info, clipboard sharing through the guest's SPICE agent (virtio-console). Clipboard checked by unit tests only so far |

---

## 1. Key technical decisions

### 1.1 Why speed forces the design

The host CPU is x86-64 or ARM64, so hardware virtualization of a RISC-V guest is
impossible. Every guest instruction has to be emulated in software.

| Technique                           | Typical guest throughput / core | Enough for a browser? |
|-------------------------------------|---------------------------------|-----------------------|
| Plain interpreter (switch dispatch) | 50–200 MIPS                     | No (boot takes minutes, GNOME barely draws) |
| Optimized interpreter (pre-decoded, threaded) | 200–500 MIPS          | Not really            |
| **Dynamic binary translation (JIT)** | **0.5–2+ GIPS**                | **Yes, roughly QEMU-TCG class or better** |

So we need a **JIT** with an inline soft-MMU TLB fast path, block chaining, and one host
thread per guest hart. That rules out garbage-collected or managed runtimes (Go, Java,
C#, JS/WASM) for the core. Their GC pauses, limited control over memory layout and
atomics, and the difficulty of emitting and patching native code all count against them.

### 1.2 Language: **Rust**

| Option | Pros | Cons |
|--------|------|------|
| **Rust** (chosen) | Native speed, no GC; precise control over memory and atomics (`std::sync::atomic` maps onto the C++11 model, which RVWMO lowers into cleanly); first-class cross-compilation to all 3 OSes × 2 archs; **Cranelift** is a production-grade, pure-Rust JIT backend for x86-64 and AArch64; `dynasm-rs` for hand-written fast paths; good crates (SDL3/winit, cpal, smoltcp, vm-fdt, virtio-queue); memory safety in the ~80% of code that isn't the JIT | Learning curve; `unsafe` is still needed in the JIT and guest-memory paths |
| C / C++ | Maximum control, what QEMU is written in | Memory-safety bugs in a huge codebase; build and portability work across Windows/macOS/Linux is much harder |
| Zig | Nice C interop, good cross-compiling | Immature ecosystem, no off-the-shelf JIT backend |

**JIT backend strategy (tiered):**
- **Tier 0:** fast pre-decoding interpreter. It is also the *reference implementation*,
  and it is used for rare or complex instructions and for single-stepping or debugging.
- **Tier 1:** Cranelift-compiled basic blocks / superblocks for hot code. This gives us
  register allocation and good code for both host ISAs without writing two backends.
- **Later (if profiling says so):** hand-written `dynasm-rs` stubs for the hottest
  paths (TLB lookup, block chaining trampolines, common vector ops).

### 1.3 Platform specifics we must handle early
- **macOS / Apple Silicon:** JIT pages need `MAP_JIT` + `pthread_jit_write_protect_np`
  (W^X toggling per thread) and the `com.apple.security.cs.allow-jit` entitlement,
  plus `sys_icache_invalidate` after writing code.
- **Windows:** `VirtualAlloc`/`VirtualProtect` and `FlushInstructionCache`. Use no
  POSIX signals: the design must not depend on SIGSEGV tricks for the MMU.
- **Linux:** `mmap` + `__builtin___clear_cache` equivalent on AArch64.
- The design therefore uses a **portable soft-MMU** (inline TLB check in generated code),
  not host-page-fault tricks.

### 1.4 Machine model: “QEMU `virt`-compatible”
We copy the memory map and device set of QEMU's `virt` board. That lets us use the
unmodified upstream firmware and kernels that Ubuntu already ships and tests against:
- **Firmware:** OpenSBI (M-mode) → U-Boot (S-mode) or EDK2 `RiscVVirt` (UEFI) → GRUB → Ubuntu kernel.
- **Interrupts:** ACLINT (MSWI/MTIMER/SSWI) + PLIC first; AIA (APLIC + IMSIC) later for MSI-capable PCIe.
- **Timer:** `Sstc` (`stimecmp`), which RVA23 mandates.
- **Devices:** NS16550 UART, goldfish RTC, syscon (poweroff/reboot), PCIe ECAM host bridge
  with **virtio-pci** devices: blk, net, rng, console, input (keyboard + tablet), gpu, sound.
- **Device tree** generated at startup (`vm-fdt` crate) and handed to OpenSBI.

### 1.5 RVA23 (RVA23U64 + RVA23S64) feature set to implement
Ubuntu 25.10 and later **require RVA23**. All userspace is built assuming these
extensions, so *all of them* must work before Ubuntu userland will run:

- **Base:** RV64I, M, A (Zaamo/Zalrsc), F, D, C, Zicsr, Zifencei, Zicntr, Zihpm
- **Bitmanip / misc:** Zba, Zbb, Zbs, Zicond, Zimop, Zcmop, Zcb, Zfa, Zfhmin, Zawrs,
  Zihintpause, Zihintntl, Zic64b, Zicbom, Zicbop, Zicboz, Ziccif/Ziccrse/Ziccamoa/Zicclsm/Za64rs, Zkt
- **Vector (mandatory in RVA23):** V 1.0 (VLEN=128 to start, later 256), Zvfhmin, Zvbb, Zvkt.
  glibc's string and memory functions and compiler auto-vectorization **will** use RVV, so
  vector performance directly affects browser speed.
- **Supervisor:** Sv39/Sv48 (Sv57 optional), Svpbmt, Svnapot, Svinval, Svade, Sstc, Sscofpmf,
  Ssstateen, Sstvala, Sstvecd, Ssu64xl, Sscounterenw, Ssccptr, Ssnpm/Supm (pointer masking)
- **Hypervisor (H):** mandatory in the RVA23S64 profile but **not needed to run Ubuntu**.
  Deferred to the last phase (a later stretch).

### 1.6 Multi-core (4 harts) design
- One host thread per hart (MTTCG-style). Guest RAM is a single shared `mmap` region.
- **Memory model:** RVWMO is weaker than x86-TSO and close to ARMv8. Guest `FENCE` maps to the
  appropriate host fence (often a no-op on x86, `dmb` on ARM), and AMOs map to host atomics.
- **LR/SC:** implemented as load + recorded value, with SC done as a host CAS (as in QEMU).
  We accept the ABA corner case, which Linux tolerates.
- **Cross-hart events:** IPIs (ACLINT SSWI / SBI IPI), remote `SFENCE.VMA` / TLB shootdowns,
  and `FENCE.I` code invalidation are delivered through per-hart atomic “exit request”
  flags checked at block boundaries plus a mailbox.
- **Self-modifying code / JIT cache coherence:** a per-page “contains translated code”
  bitmap. Guest stores to such pages invalidate translations, and `FENCE.I` flushes per hart.
- **Idle:** `WFI` parks the host thread on a condvar, so idle cores burn no host CPU.

---

## 2. Project structure (Cargo workspace)

```
rust-v/
  crates/
    rv-isa/        decoder, instruction definitions, disassembler (shared by interp + JIT)
    rv-core/       hart state, CSRs, privilege/trap logic, MMU (Sv39/48/57), interpreter
    rv-vector/     RVV 1.0 implementation (interpreter + JIT helpers)
    rv-jit/        block translator, Cranelift backend, code cache, chaining
    rv-machine/    board: memory map, bus, ACLINT, PLIC/AIA, UART, RTC, PCIe, FDT
    rv-virtio/     virtio-pci transport + blk/net/rng/console/input/gpu/sound
    rv-frontend/   window + input + audio (SDL3 or winit+wgpu, cpal)
    rv-net/        user-mode networking (smoltcp-based NAT, or libslirp binding)
  apps/
    rust-v/       CLI + GUI launcher
  tests/           riscv-tests, riscv-arch-test, differential tests, boot tests
  firmware/        scripts to fetch/build OpenSBI, U-Boot / EDK2
```

---

## 3. Phased roadmap

Every phase ends with a **demoable milestone** and automated tests in CI
(GitHub Actions matrix: macOS-arm64, Linux-x86_64, Linux-arm64, Windows-x86_64).

### Phase 0: Foundations (≈1–2 weeks)
- Cargo workspace, CI on all 4 host targets, formatting and clippy.
- Fetch the specs: unprivileged ISA, privileged ISA, RVV 1.0, RVA23 profile, SBI, virtio 1.2.
- Build reference tools: **Spike** (golden model for differential testing), QEMU (for
  comparing device behavior), and a RISC-V cross toolchain.
- **Milestone:** `cargo test` green on all hosts; a “hello” bare-metal ELF loader skeleton.

### Phase 1: Single-hart interpreter, RV64IMAC + privileged arch (≈3–5 weeks)
- **JIT-shaped interfaces from day one.** These are design decisions only, with no code
  generation yet. They are cheap now and expensive to change later:
  - Hart state is a flat `#[repr(C)]` struct (x/f/v registers, PC, hot CSRs) at fixed
    offsets, so generated code can address it directly.
  - The software TLB is a plain array that generated code can index inline. The fast path
    is kept separate from the slow page-table walk.
  - Exceptions are precise and restartable from (PC, instret), never from deep inside a
    Rust call stack.
  - Each hart has a single atomic “exit request” word for interrupts, IPIs and remote fences.
  - Ideally, instruction semantics are written **once** in a small internal IR or builder
    API. The interpreter executes it and the JIT later lowers it to Cranelift, so the two
    tiers can't drift apart.
- Decoder (16/32-bit), RV64I/M/A/C, Zicsr, Zifencei.
- M/S/U modes, traps, interrupts, delegation, `mstatus`/`sstatus` and friends, `mret`/`sret`, `WFI`.
- MMU: Sv39 → Sv48 → Sv57, A/D bits (Svade), PMP (minimal), software TLB.
- **Tests:** riscv-tests (`rv64ui/um/ua/uc/mi/si`) pass. Lockstep comparison against Spike
  (`--log-commits`) for the first N million instructions.
- **Milestone:** all riscv-tests pass; ELF test programs print over a HTIF/UART stub.

### Phase 2: Minimal board + Linux boot, single core (≈3–4 weeks)
- RAM, ACLINT, PLIC, NS16550 UART, syscon, goldfish RTC, generated FDT.
- Boot **OpenSBI fw_dynamic → Linux (defconfig) → busybox initramfs** on 1 hart.
- GDB stub (remote serial protocol) for debugging guests. This pays for itself many times over.
- **Decoded-instruction cache.** Instructions are pre-decoded once into compact ops and
  grouped into decoded basic blocks, keyed by (physical PC, mode bits) and dispatched
  through a handler table. Expected speedup: 2–5× over decode-every-time.
  - Code invalidation is built here: a per-page “contains decoded code” bitmap,
    invalidation on guest stores to those pages, and `FENCE.I`. The JIT reuses this
    machinery and these translation units later.
- **Milestone:** a Linux shell on the serial console; boot-time baseline recorded with and
  without the decode cache.

### Phase 2.5: JIT spike (≈2–3 weeks, time-boxed)
- A throwaway-quality Cranelift JIT covering only the RV64I ALU ops, loads, stores and
  branches. It runs user-mode benchmarks (CoreMark, Dhrystone) on one hart.
- Its purpose is to **de-risk the JIT architecture early**:
  - Measure Cranelift compile latency against code quality.
  - Validate the inline TLB fast path and the hart-state layout from Phase 1.
  - Validate W^X toggling on macOS (`MAP_JIT`) and code-cache flushing on ARM64.
- If Cranelift compiles too slowly, we decide now whether we need a cheap baseline JIT
  tier (`dynasm-rs` templates) in front of it.
- **Milestone:** a measured speedup over the decode-cache interpreter on CoreMark, and a
  short written report on findings and design adjustments.

### Phase 3: Full RVA23 ISA (≈6–10 weeks, the biggest ISA chunk)
- F, D (correct NaN-boxing, rounding modes, exception flags; use softfloat-equivalent
  semantics where host FP differs), Zfa, Zfhmin.
- Bitmanip (Zba/Zbb/Zbs), Zicond, Zcb, Zimop/Zcmop, Zawrs, Zicbo*, Sstc, Svpbmt,
  Svnapot, Svinval, Ssstateen, Sscofpmf, pointer masking, counters.
- **Vector 1.0** (VLEN=128, ELEN=64): configuration, all load/store forms incl.
  segment/fault-only-first, integer/fixed-point/FP/mask/permutation, Zvfhmin, Zvbb, Zvkt.
- **Tests:** riscv-arch-test (ACT) for all supported extensions; differential fuzzing vs
  Spike (random instruction streams, compare architectural state).
- **Milestone:** Linux booted with full RVA23 ISA string; RVV-using userland (e.g. glibc
  test suite or RVA23-built busybox) runs correctly.

### Phase 4: SMP, 4 harts (≈2–3 weeks)
- Thread per hart, shared RAM, atomics/LR-SC on host atomics, SBI HSM (hart start/stop),
  IPIs, remote fences, per-hart `WFI` parking, deterministic “single-thread round-robin”
  mode for debugging.
- **Tests:** litmus tests (herd/litmus7-style RVWMO tests), Linux SMP boot,
  `stress-ng` in guest, kernel lock torture.
- **Milestone:** Linux boots on 4 cores; `nproc` = 4; parallel `make` works in the guest.

### Phase 5: Full JIT (≈8–12 weeks, the performance core)
Why it comes here: the ISA is complete and verified (Phase 3), so we JIT a stable target
and have a mature interpreter as the test oracle. SMP exists (Phase 4), so the JIT is
multi-threaded from its first design, covering cross-thread invalidation, remote `FENCE.I`
and the memory model. It also lands *before* Ubuntu, because debugging virtio, firmware
and systemd at interpreter speed (tens of minutes per boot) is impractical.
- Builds on the Phase 2.5 spike findings and on the Phase 2 decoded-block cache and
  invalidation machinery.
- Translation unit: basic block → superblock (follow static fall-throughs), keyed by
  (physical PC, privilege/mode bits, relevant CSR state such as `vtype`/FS/VS).
- Cranelift backend: guest registers in a context struct with register caching inside blocks,
  **inline TLB lookup** for loads/stores (fast path ≈ 5–8 host instructions), helper calls
  for slow paths, precise exceptions (PC + instret reconstruction).
- **Block chaining** (direct jumps patched; indirect via a hash-lookup trampoline), return
  address prediction for `jalr ra`.
- Interrupt and exit checks at block entry (one atomic load), instret/cycle accounting.
- Code cache management, SMC detection, `FENCE.I`, per-hart vs shared translations.
- **Vector stays in the interpreter for now.** JIT code calls RVV helpers; RVV is JIT-ed
  in Phase 8, once profiling shows which ops matter.
- Background compilation thread(s) so the tier-0 interpreter keeps running while hot blocks compile.
- **Interpreter-vs-JIT differential mode** (compare architectural state at block boundaries)
  runs in CI.
- **Benchmarks tracked in CI:** CoreMark, Dhrystone, `xz`/`gzip` of a fixed file,
  Linux boot time to shell, `sysbench`.
- **Targets:** ≥ 1 GIPS/core on CoreMark-like code on an Apple M-series / modern x86.

### Phase 6: Virtio + Ubuntu Server (≈4–6 weeks)
- PCIe ECAM host bridge, virtio-pci transport (modern), MSI via AIA (APLIC+IMSIC) or
  INTx via PLIC first.
- virtio-blk (raw + qcow2 later, async I/O thread), virtio-net + user-mode networking
  (DHCP, DNS, TCP/UDP NAT), virtio-rng, virtio-console.
- Firmware chain: OpenSBI → U-Boot (or EDK2) → GRUB → Ubuntu kernel, using the **official
  Ubuntu RISC-V preinstalled server image** (latest release).
- **Milestone:** Ubuntu Server boots to a login prompt in < 60 s; `apt update` works over
  the network.

### Phase 7: Desktop I/O + Ubuntu Desktop (≈4–6 weeks)
- Frontend window (SDL3 or winit + wgpu) with scaling and HiDPI; mouse grab/absolute tablet.
- virtio-gpu 2D (scanout, resource transfer, cursor), multiple resolutions.
- virtio-input (keyboard, absolute pointer), virtio-sound → host audio (cpal), clipboard later.
- **Milestone:** Ubuntu Desktop (GNOME, Wayland) boots to the desktop and runs Firefox.
  At this point the guest renders with **llvmpipe** (software OpenGL), which is CPU-heavy.

### Phase 8: Performance for a usable browser (open-ended, ≈2–3 months)
- Profile real workloads (GNOME Shell, Firefox/Chromium page loads, JS benchmarks such as
  Speedometer/JetStream inside the guest).
- **Vector JIT:** translate the RVV ops that profiling shows are hot (glibc mem*/str*,
  auto-vectorized loops) to host SIMD (NEON/SSE/AVX2) via Cranelift SIMD types.
- Other likely wins: better register allocation across blocks, flag/condition fusion,
  cheaper TLB (larger, huge-page aware), fewer exits, tuning the code cache.
- **GPU acceleration** (the biggest single win for a desktop): virtio-gpu 3D via
  **Venus (Vulkan) or virgl**, backed by host Vulkan/Metal (via MoltenVK on macOS) or
  gfxstream. This moves GNOME compositing and browser rasterization off the emulated CPU.
- **Milestone:** browse typical websites at an acceptable frame rate; Speedometer score tracked.

### Phase 9: Product polish and packaging (≈3–4 weeks, partly in parallel)
- Config files / CLI, VM disk-image management, snapshots (save/restore), shared folders
  (virtio-fs or 9p), clipboard integration.
- Release binaries: macOS (signed + notarized, JIT entitlement, universal or per-arch),
  Windows (MSI/zip, x86-64 and ARM64), Linux (tarball/AppImage, x86-64 and arm64).

### Phase 10 (stretch): Hypervisor extension (H) and complete RVA23S64 conformance
- H extension (two-stage translation, VS-mode CSRs, guest interrupts via AIA IMSIC guest files).
  This allows running KVM inside the guest.

---

## 4. Testing & verification strategy (continuous, all phases)
1. **Unit tests** per instruction/CSR (table-driven).
2. **riscv-tests** and **riscv-arch-test** in CI.
3. **Differential testing vs Spike**: lockstep commit-log comparison plus random
   instruction-stream fuzzing. This is the main defense against subtle ISA bugs.
4. **Interpreter-vs-JIT differential mode**: run the same guest with both tiers and compare
   state at block boundaries. It is cheap to build and catches nearly all JIT bugs.
5. **Boot tests**: scripted serial-console checks (OpenSBI banner → kernel → login).
6. **Performance regression dashboard**: benchmarks per commit on fixed hardware.

## 5. Main risks
| Risk | Mitigation |
|------|------------|
| JIT complexity / subtle bugs | Interpreter as reference; interp-vs-JIT differential mode; fuzzing |
| RVV is large and slow to emulate | Implement in interpreter first; profile which ops Ubuntu actually uses; SIMD-map hot ones |
| Guest software rendering is too slow for a browser | virtio-gpu 3D (Venus/virgl) in phase 8 |
| Memory-model bugs on ARM hosts only | Litmus tests in CI on arm64 runners; conservative fence mapping first |
| macOS JIT restrictions | W^X toggling abstraction validated in the Phase 2.5 spike; test signed builds in CI |
| Cranelift compile latency too high | Measured in the Phase 2.5 spike; fallback is a `dynasm-rs` baseline tier |
| “Fast enough” is subjective | Concrete targets: GIPS/core, boot time, Speedometer score |

## 6. Rough timeline
Roughly 9–12 months to “Ubuntu Desktop + Firefox, usable” for one experienced developer
working focused, with phases 5–8 carrying most of the risk. Rough checkpoints:
- **~month 3:** decode cache in place and JIT spike results in hand (Phase 2.5).
- **~month 5–6:** the full JIT lands (Phase 5).
- **~month 7:** Ubuntu Server boots at JIT speed (Phase 6).

## 7. Immediate next steps
1. Install Rust (`rustup`), create the workspace and CI.
2. Build Spike + the RISC-V GNU toolchain locally, and fetch riscv-tests.
3. Start Phase 1: define the `#[repr(C)]` hart state, TLB layout and instruction-semantics
   API, then the `rv-isa` decoder + `rv-core` interpreter for RV64I, driven by riscv-tests.
