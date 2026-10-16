# Phase 2.5: JIT spike report

**Date:** 2026-10-16. **Host:** Apple M-series (arm64), macOS 26. **Code:** `crates/rv-jit`,
`crates/rv-core/src/jit.rs`, enabled with `risc-vm --jit`.

## What was built

- Cranelift 0.136 translates decoded basic blocks to native code once a block has been
  interpreted `jit_threshold` times.
- **Coverage:** RV64I and M arithmetic. A block containing anything else (CSRs, AMOs, system
  instructions) is compiled up to that instruction, and the interpreter takes over from there.
- **Registers:** guest registers become SSA variables. They are loaded on entry if read, and
  written back on exit if dirty.
- **Loads and stores:** the TLB lookup is inlined, roughly 10 host instructions on the fast path.
  Misses, page-crossing accesses and MMIO go to helper calls. Faults return to the dispatcher
  with a precise `pc` and retired count.
- **No chaining:** every block returns to the dispatcher.

## Correctness

- All 198 riscv-tests (rv64 ui/um/ua/uc/mi/si, physical and virtual memory) pass with every
  block compiled on first use. A deliberately broken `add` in the JIT fails 89 of them, so the
  tests do exercise the compiled code.
- OpenSBI and Linux 6.12 boot to the shell and power off with the JIT on, including with a
  compile threshold of 2 (`boot_linux_jit` test).
- CoreMark validates (`Correct operation validated`, crcfinal `0xcc42`).

## Performance

| Workload | Interpreter | JIT | Speed-up |
|---|---|---|---|
| CoreMark 2K, 25 000 iterations | 666 it/s, 198 MIPS | **2272 it/s, 717 MIPS** | 3.4× |
| Kernel `raid6: int64x4 gen()` | 174 MB/s | 2207 MB/s | 12.7× |
| Linux boot to `/init`, threshold 16 | 1.77 s | 2.09 s | 0.85× |
| Linux boot to `/init`, threshold 4096 | 1.77 s | **1.11 s** | 1.6× |

### Compile latency is the main problem for cold code

| Threshold | Blocks compiled | Avg compile | Total compile | Boot to `/init` |
|---|---|---|---|---|
| 16 | 14 856 | 105 µs | 1.56 s | 2.09 s |
| 64 | 8 640 | 102 µs | 0.88 s | 1.66 s |
| 256 | 5 916 | 108 µs | 0.64 s | 1.48 s |
| 1024 | 2 456 | 120 µs | 0.29 s | 1.20 s |
| 4096 | 814 | 139 µs | 0.11 s | 1.11 s |

Blocks average 6–7 guest instructions, so compilation costs about **15 µs per guest
instruction**. In that time the interpreter executes about 3000 instructions. Compiling
synchronously on the hart thread only pays off for blocks that run thousands of times.

### Dispatch overhead dominates hot code

Profile of CoreMark under the JIT:

| Where | Share |
|---|---|
| Generated code | 32 % |
| Dispatcher (block lookup via fetch TLB + hash map, loop) | 50 % |
| `run_compiled` (call in and out, retire accounting) | 13 % |
| Interrupt check between blocks | 5 % |

Two thirds of the time goes to getting between blocks, not to running them.

### Code size

The JIT emits about 80 bytes of host code per guest instruction, or 6.7 MiB for the
threshold-16 boot. `cranelift-jit` finalizes, and so `mprotect`s, per function, and nothing is
freed when guest code is invalidated.

## Decisions for Phase 5 (full JIT)

1. **Block chaining is the top priority.** Direct branches should jump straight to the target's
   compiled code, patched once it exists. Indirect jumps (`jalr`) should go through a small
   inline jump cache keyed by guest pc. The interrupt and exit check moves into generated code
   as one load of `exit_request` at block entry. Expected gain: 2–3× on hot code, taking
   CoreMark into the 1.5–2 GIPS range.
2. **Keep Cranelift as the optimizing tier, but compile off-thread.** Hot blocks are queued to a
   background compiler thread, and the hart keeps interpreting until the code is ready. Combined
   with a threshold around 1000, cold-code cost all but disappears.
3. **Make translation units larger.** Superblocks or traces amortize the per-function overhead
   (prologue, register load and store, finalize) and give Cranelift more to optimize.
4. **A cheap baseline tier (dynasm templates) is not needed yet.** Revisit only if workloads with
   lots of warm-but-not-hot code (browser start-up, JavaScript JITs in the guest) show
   interpreter time dominating after items 1–3.
5. **Use our own code cache.** Allocate from large arenas with batched finalization and
   invalidate by page generation, as for decoded blocks. Free code with a generational flush
   when the cache fills.
6. **macOS hardened runtime.** `cranelift-jit` flips pages from read-write to read-execute with
   `mprotect`. That works for unsigned binaries, but signed and notarized builds need a custom
   `JITMemoryProvider` using `MAP_JIT` and `pthread_jit_write_protect_np`, plus the
   `com.apple.security.cs.allow-jit` entitlement. Do it together with item 5.
7. **Keep compiling per data privilege.** Compiled code is specialized by data-access privilege,
   because it inlines that privilege's TLB table. That's cheap and correct; keep it.
