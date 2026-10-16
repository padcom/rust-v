#!/usr/bin/env bash
# Builds U-Boot for S-mode (qemu-riscv64_smode, which matches our board) into
# third_party/guest/u-boot.bin. OpenSBI starts it as the kernel payload; it then finds the
# EFI system partition of a disk image and runs GRUB from it, which is how the official
# Ubuntu RISC-V images boot under QEMU.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UBOOT_VERSION="${UBOOT_VERSION:-2026.07}"
ENGINE="${CONTAINER_ENGINE:-$(command -v podman || command -v docker)}"

mkdir -p "$ROOT/third_party/guest/src"
"$ENGINE" run --rm -v "$ROOT:/work" -w /work -e UBOOT_VERSION="$UBOOT_VERSION" \
  docker.io/library/debian:trixie bash -euo pipefail -c '
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      build-essential gcc-riscv64-linux-gnu bc bison flex libssl-dev libgnutls28-dev uuid-dev \
      python3 python3-setuptools python3-pyelftools swig wget ca-certificates >/dev/null
    SRC=/work/third_party/guest/src
    TAR=$SRC/u-boot-$UBOOT_VERSION.tar.bz2
    [ -f $TAR ] || wget -q -O $TAR https://ftp.denx.de/pub/u-boot/u-boot-$UBOOT_VERSION.tar.bz2
    mkdir -p /build && tar -C /build -xjf $TAR
    cd /build/u-boot-$UBOOT_VERSION
    make -s CROSS_COMPILE=riscv64-linux-gnu- qemu-riscv64_smode_defconfig
    # Boot without waiting for a key press.
    ./scripts/config --set-val BOOTDELAY 0
    make -s CROSS_COMPILE=riscv64-linux-gnu- olddefconfig
    make -s CROSS_COMPILE=riscv64-linux-gnu- -j$(nproc)
    cp u-boot.bin /work/third_party/guest/
    ls -l /work/third_party/guest/u-boot.bin
  '
