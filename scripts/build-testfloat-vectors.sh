#!/usr/bin/env bash
# Generates Berkeley TestFloat vectors (SoftFloat with RISC-V specialization: canonical NaNs,
# RISC-V integer-conversion saturation, tininess after rounding) into
# third_party/testfloat/vectors, for crates/rv-softfloat/tests/testfloat.rs.
# Runs inside a Debian container (podman or docker).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENGINE="${CONTAINER_ENGINE:-$(command -v podman || command -v docker)}"
CASES="${TESTFLOAT_CASES:-100000}"
mkdir -p "$ROOT/third_party/testfloat"

"$ENGINE" run --rm -v "$ROOT:/work" -w /work -e CASES="$CASES" docker.io/library/debian:trixie bash -euo pipefail -c '
  apt-get update -qq && apt-get install -y -qq --no-install-recommends build-essential git ca-certificates >/dev/null
  D=/work/third_party/testfloat
  [ -d $D/berkeley-softfloat-3 ] || git clone -q https://github.com/ucb-bar/berkeley-softfloat-3.git $D/berkeley-softfloat-3
  [ -d $D/berkeley-testfloat-3 ] || git clone -q https://github.com/ucb-bar/berkeley-testfloat-3.git $D/berkeley-testfloat-3
  rm -rf /build && mkdir /build && cp -r $D/berkeley-softfloat-3 $D/berkeley-testfloat-3 /build/
  make -s -C /build/berkeley-softfloat-3/build/Linux-x86_64-GCC SPECIALIZE_TYPE=RISCV -j$(nproc)
  make -s -C /build/berkeley-testfloat-3/build/Linux-x86_64-GCC SPECIALIZE_TYPE=RISCV -j$(nproc) testfloat_gen
  GEN=/build/berkeley-testfloat-3/build/Linux-x86_64-GCC/testfloat_gen
  OUT=$D/vectors
  rm -rf $OUT && mkdir -p $OUT
  RMS="near_even minMag min max near_maxMag"
  gen() { # name, extra args...
    local name=$1; shift
    # Level 1 enumerates up to millions of cases; keep the first $CASES.
    ($GEN -tininessafter "$@" || true) | head -n $CASES > $OUT/$name.txt
  }
  for f in f32 f64; do
    for op in add sub mul div sqrt mulAdd; do
      for rm in $RMS; do gen ${f}_${op}-$rm -r$rm ${f}_$op; done
    done
    for op in eq le lt eq_signaling le_quiet lt_quiet; do gen ${f}_$op ${f}_$op; done
    for i in i32 ui32 i64 ui64; do
      for rm in $RMS; do
        gen ${f}_to_$i-$rm -r$rm -exact ${f}_to_$i
        gen ${i}_to_$f-$rm -r$rm ${i}_to_$f
      done
    done
    for rm in $RMS; do
      gen ${f}_roundToInt-$rm -r$rm ${f}_roundToInt
      gen ${f}_roundToInt_exact-$rm -r$rm -exact ${f}_roundToInt
    done
  done
  for rm in $RMS; do
    gen f64_to_f32-$rm -r$rm f64_to_f32
    gen f32_to_f16-$rm -r$rm f32_to_f16
    gen f64_to_f16-$rm -r$rm f64_to_f16
  done
  gen f32_to_f64 f32_to_f64
  gen f16_to_f32 f16_to_f32
  gen f16_to_f64 f16_to_f64
  ls $OUT | wc -l; du -sh $OUT
'
