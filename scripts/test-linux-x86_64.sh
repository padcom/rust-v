#!/usr/bin/env bash
# Cross-compiles the test suites for x86-64 Linux in an (arm64 or x86-64) container and runs
# them under qemu-user when the container is not x86-64, which exercises the JIT's x86-64
# backend from an ARM host. Extra arguments go to `cargo test`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENGINE="${CONTAINER_ENGINE:-$(command -v podman || command -v docker)}"

"$ENGINE" run --rm -v "$ROOT:/work" -w /work \
  -v rust-v-cargo-native:/usr/local/cargo/registry \
  -v rust-v-target-cross:/target -e CARGO_TARGET_DIR=/target \
  docker.io/library/rust:1 bash -euo pipefail -c '
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends gcc-x86-64-linux-gnu libc6-dev-amd64-cross qemu-user >/dev/null
    rustup target add x86_64-unknown-linux-gnu >/dev/null
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
    if [ "$(uname -m)" != x86_64 ]; then
      export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="qemu-x86_64 -L /usr/x86_64-linux-gnu"
    fi
    cargo test --release --target x86_64-unknown-linux-gnu -p rv-isa -p rv-softfloat -p rv-core -p rv-jit \
      -p rv-net -p rv-machine -p rv-conformance -- --include-ignored '"$*"'
  '
