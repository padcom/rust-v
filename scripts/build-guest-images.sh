#!/usr/bin/env bash
# Builds the guest images used for bring-up into third_party/guest:
#   fw_dynamic.bin   OpenSBI (generic platform)
#   Image            Linux kernel
#   initramfs.cpio   a minimal freestanding /init (see scripts/guest/init.c)
#
# Everything is built inside a Debian container (podman or docker) so it works the same on
# macOS, Linux and Windows hosts.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/third_party/guest"
LINUX_VERSION="${LINUX_VERSION:-6.12}"
OPENSBI_VERSION="${OPENSBI_VERSION:-1.6}"
ENGINE="${CONTAINER_ENGINE:-$(command -v podman || command -v docker)}"

mkdir -p "$OUT/src"
"$ENGINE" run --rm -v "$ROOT:/work" -w /work \
  -e LINUX_VERSION="$LINUX_VERSION" -e OPENSBI_VERSION="$OPENSBI_VERSION" \
  docker.io/library/debian:trixie bash -euo pipefail -c '
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      build-essential gcc-riscv64-linux-gnu bc bison flex libssl-dev libelf-dev \
      cpio xz-utils wget ca-certificates python3 >/dev/null
    SRC=/work/third_party/guest/src
    OUT=/work/third_party/guest
    CROSS=riscv64-linux-gnu-
    JOBS=$(nproc)
    mkdir -p /build

    # ---- OpenSBI ----
    TAR=$SRC/opensbi-$OPENSBI_VERSION.tar.gz
    [ -f $TAR ] || wget -q -O $TAR https://github.com/riscv-software-src/opensbi/archive/refs/tags/v$OPENSBI_VERSION.tar.gz
    tar -C /build -xzf $TAR
    make -s -C /build/opensbi-$OPENSBI_VERSION CROSS_COMPILE=$CROSS PLATFORM=generic FW_PIC=y -j$JOBS
    cp /build/opensbi-$OPENSBI_VERSION/build/platform/generic/firmware/fw_dynamic.bin $OUT/

    # ---- Linux ----
    TAR=$SRC/linux-$LINUX_VERSION.tar.xz
    [ -f $TAR ] || wget -q -O $TAR https://cdn.kernel.org/pub/linux/kernel/v${LINUX_VERSION%%.*}.x/linux-$LINUX_VERSION.tar.xz
    tar -C /build -xJf $TAR
    K=/build/linux-$LINUX_VERSION
    make -s -C $K ARCH=riscv CROSS_COMPILE=$CROSS defconfig
    $K/scripts/kconfig/merge_config.sh -m -O $K $K/.config /work/scripts/guest/kernel.config >/dev/null
    make -s -C $K ARCH=riscv CROSS_COMPILE=$CROSS olddefconfig
    make -s -C $K ARCH=riscv CROSS_COMPILE=$CROSS -j$JOBS Image
    cp $K/arch/riscv/boot/Image $OUT/

    # ---- initramfs ----
    ${CROSS}gcc -march=rv64imac_zicsr_zifencei -mabi=lp64 -Os -static -nostdlib -ffreestanding \
      -fno-builtin -fno-stack-protector -mno-relax -Wl,--no-relax -o /build/init /work/scripts/guest/init.c
    gcc -o /build/gen_init_cpio $K/usr/gen_init_cpio.c
    /build/gen_init_cpio /work/scripts/guest/initramfs.list > $OUT/initramfs.cpio
    ls -l $OUT
  '
