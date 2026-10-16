#!/usr/bin/env bash
# Fetches and builds the official riscv-tests ISA suite into third_party/riscv-tests.
# Requires a bare-metal RISC-V GCC; set RISCV_PREFIX if yours isn't riscv64-elf-.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/third_party/riscv-tests"
PREFIX="${RISCV_PREFIX:-riscv64-elf-}"
REV="${RISCV_TESTS_REV:-master}"

if [ ! -d "$DIR/.git" ]; then
  git clone --recursive https://github.com/riscv-software-src/riscv-tests.git "$DIR"
fi
git -C "$DIR" fetch --quiet origin "$REV" || true
git -C "$DIR" checkout --quiet "$REV"
git -C "$DIR" submodule update --init --recursive --quiet

# -ffreestanding: bare-metal toolchains without newlib (e.g. Homebrew's) lack libc headers;
# env/v gets the few declarations it needs from scripts/riscv-tests-shim.
# Build only the suites the emulator currently supports.
SUITES="rv64ui rv64um rv64ua rv64uc rv64mi rv64si"
make -C "$DIR/isa" -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" \
  RISCV_PREFIX="$PREFIX" XLEN=64 \
  RISCV_GCC_OPTS="-static -mcmodel=medany -fvisibility=hidden -nostdlib -nostartfiles -ffreestanding -isystem $ROOT/scripts/riscv-tests-shim" \
  $SUITES
echo "riscv-tests built in $DIR/isa"
