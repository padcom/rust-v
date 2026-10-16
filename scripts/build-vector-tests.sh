#!/usr/bin/env bash
# Builds Spike (the RISC-V golden model) and generates the chipsalliance riscv-vector-tests
# suite with it, into third_party/vector-tests. The generated tests are bare-metal ELFs that
# report through tohost like riscv-tests; their expected values come from Spike.
# Runs inside a Debian container (podman or docker).
# Usage: scripts/build-vector-tests.sh [VLEN]   (default 128)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENGINE="${CONTAINER_ENGINE:-$(command -v podman || command -v docker)}"
VLEN="${1:-128}"
mkdir -p "$ROOT/third_party/vector-tests"

"$ENGINE" run --rm -v "$ROOT:/work" -w /work -e VLEN="$VLEN" docker.io/library/debian:trixie bash -euo pipefail -c '
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends build-essential git ca-certificates device-tree-compiler \
    libboost-regex-dev libboost-system-dev gcc-riscv64-unknown-elf picolibc-riscv64-unknown-elf golang-go \
    python3 >/dev/null
  D=/work/third_party/vector-tests
  [ -d $D/riscv-isa-sim ] || git clone -q https://github.com/riscv-software-src/riscv-isa-sim.git $D/riscv-isa-sim
  [ -d $D/riscv-vector-tests ] || git clone -q --recursive https://github.com/chipsalliance/riscv-vector-tests.git $D/riscv-vector-tests

  # Build Spike out of tree (in the container) and install it into the shared directory.
  if [ ! -x $D/spike/bin/spike ]; then
    mkdir -p /build/spike && cd /build/spike
    $D/riscv-isa-sim/configure --prefix=$D/spike >/dev/null
    make -s -j$(nproc) >/dev/null && make -s install >/dev/null
  fi
  export PATH=$D/spike/bin:$PATH

  cd $D/riscv-vector-tests
  # Debian'"'"'s cross compiler lacks the bfloat16 extensions, so drop those test configurations.
  # Vector crypto tests are still built; the test runner skips what risc-vm does not implement.
  rm -f configs/*bf16*
  : > Makefrag  # placeholder so that the Makefile can include it; generate-stage1 rewrites it
  ARGS="VLEN=$VLEN XLEN=64 MODE=machine RISCV=$D/spike RISCV_PREFIX=riscv64-unknown-elf-
    MARCH=rv64gcv_zvbb_zvbc_zfh_zvfh_zvkg_zvkned_zvknha_zvksed_zvksh"
  # make reads Makefrag before regenerating it, so generate it in a separate run first.
  { make generate-stage1 --environment-overrides $ARGS && make all -j$(nproc) --environment-overrides $ARGS; } \
    > /work/third_party/vector-tests/build.log 2>&1 || { tail -30 /work/third_party/vector-tests/build.log; exit 1; }
  ls out/v${VLEN}x64machine/bin/stage2 | wc -l
'
