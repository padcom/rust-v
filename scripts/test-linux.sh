#!/usr/bin/env bash
# Builds rust-v and runs its test suites on Linux, in a container (podman or docker), so a
# macOS or Windows host can check the Linux build. Set PLATFORM=linux/amd64 to test x86-64
# (needs emulation or Rosetta in the container VM). Extra arguments go to `cargo test`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENGINE="${CONTAINER_ENGINE:-$(command -v podman || command -v docker)}"
PLATFORM="${PLATFORM:-}"
TAG="$(echo "${PLATFORM:-native}" | tr '/' '-')"

"$ENGINE" run --rm ${PLATFORM:+--platform "$PLATFORM"} \
  -v "$ROOT:/work" -w /work \
  -v "rust-v-cargo-$TAG:/usr/local/cargo/registry" \
  -v "rust-v-target-$TAG:/target" \
  -e CARGO_TARGET_DIR=/target \
  docker.io/library/rust:1 bash -euo pipefail -c '
    apt-get update -qq && apt-get install -y -qq --no-install-recommends libasound2-dev pkg-config >/dev/null
    rustc --version
    cargo build --release --workspace
    cargo test --release --workspace -- --include-ignored '"$*"'
  '
