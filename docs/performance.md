# Performance work (Phase 8, first round)

Measured on an Apple M-series host (18 cores), 4 harts, default settings, Ubuntu 26.10.

## Workloads

- **Server:** `scripts/ubuntu-bench.py` boots the Ubuntu server image from a pristine copy
  (first boot, cloud-init included) and runs single-threaded benchmarks inside it, best of
  three: a Python dictionary loop, `xz -1` and `gzip -1`, `sha256sum` of 64 MiB, `grep`,
  `sort` of 3 M lines and a 64 MiB `dd`.
- **Desktop:** `scripts/desktop-smoke.py` boots the desktop live ISO (GNOME on Wayland,
  llvmpipe), switches to the live session and runs Firefox, once headless
  (`firefox --headless --screenshot` of the Wikipedia RISC-V article, cold) and once in the
  session. `RV_VECTOR_PROFILE=1` counts the vector instructions that still run in the
  interpreter.

## Results

| | End of Phase 6 | Now |
|---|---|---|
| CoreMark (1 hart) | 6 650 it/s | 10 250 it/s |
| Server first boot to login prompt | 55 s | 50 s |
| Python loop | 14.4 s | 13.1 s |
| xz -1, 16 MiB | 11.1 s | 9.9 s |
| gzip -1 | 1.42 s | 1.03 s |
| sort | 6.1 s | 4.6 s |
| sha256, 64 MiB | 0.64 s | 0.56 s |
| JIT compile time during a server boot | 100 s | 62 s |

Desktop session (boot, GNOME, Firefox twice, about 15 minutes):

| | First Phase 8 profile | Now |
|---|---|---|
| Vector instructions run by the interpreter | 18.9 G | 3.8 G |
| JIT compile time | 1 864 s | 821 s |
| Code generated | 14.9 GB | 11.2 GB |
| Firefox headless, Wikipedia article, cold | — | 74 s |

## What changed

1. **One code cache and compiler pool for all harts** instead of one per hart: each block
   is compiled once, and a hart can use code another hart asked for. Resets are
   stop-the-world (see `rv-jit/src/pool.rs`).
2. **Two tiers.** A block first gets quick code for itself alone (single-pass register
   allocation) that counts its executions; hot blocks get their *region* (loops through
   them and two levels of successors within the page), with internal edges as plain jumps
   and guest registers in SSA values, compiled with the better allocator. The first-tier
   code is redirected to it.
3. **Native vector code.** VSETVLI/VSETIVLI; unit-stride loads and stores; integer
   arithmetic, logic, min/max, shifts, multiply, compares, merges, slides, gathers,
   reductions, mask operations, vid, scalar moves; masked forms. Blocks without their own
   VSETVLI are specialised on the vtype seen at compile time, behind a check.
4. **Interpreter:** page-at-a-time unit-stride vector memory accesses.

## Where the time goes now, and what is next

- **Rendering is on the CPU.** GNOME and Firefox draw through llvmpipe, whose shaders
  are guest code the JIT has to compile and run. GPU acceleration (virtio-gpu 3D with
  Venus or virgl on the host's Vulkan/Metal) is the largest remaining lever and is not
  started: it needs a host graphics stack, host-visible memory mapped into the guest
  address space, and the virtio-gpu 3D protocol.
- **Floating-point vector instructions** are the largest group still interpreted (vfmul,
  vfadd, vfmacc, vfcvt, ...). Native code needs RISC-V NaN, rounding-mode and exception
  flag behaviour.
- **Compile cost and code size.** Guest JITs (llvmpipe, SpiderMonkey in Firefox and GNOME
  Shell) produce a lot of code: about 50 M guest instructions are compiled in a desktop
  session, at ~15 µs each with Cranelift, and the code cache still fills several times.
  A cheaper first tier (a direct code emitter rather than Cranelift) and smaller exit
  sequences would help most.
- **Per-block overhead** in first-tier code: register write-back and the guards on every
  exit.
