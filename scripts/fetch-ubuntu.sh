#!/usr/bin/env bash
# Downloads an official Ubuntu RISC-V image and checks it against Ubuntu's SHA256SUMS.
#
#   scripts/fetch-ubuntu.sh            preinstalled server image, unpacked into
#                                      third_party/ubuntu/ubuntu-<release>-server-riscv64.img and
#                                      grown to $DISK_SIZE (cloud-init grows the root partition),
#                                      plus an identical ...-pristine.img that the smoke and
#                                      benchmark scripts boot (with writes kept in memory)
#   scripts/fetch-ubuntu.sh desktop    desktop live ISO,
#                                      third_party/ubuntu/ubuntu-<release>-desktop-riscv64.iso
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/third_party/ubuntu"
FLAVOR="${1:-server}"
RELEASE="${UBUNTU_RELEASE:-26.10}"
DISK_SIZE="${DISK_SIZE:-16G}"
URL="https://cdimage.ubuntu.com/ubuntu/releases/$RELEASE/release"
case "$FLAVOR" in
  server) NAME="ubuntu-$RELEASE-preinstalled-server-riscv64.img.xz" ;;
  desktop) NAME="ubuntu-$RELEASE-desktop-riscv64.iso" ;;
  *) echo "usage: $0 [server|desktop]" >&2; exit 2 ;;
esac

mkdir -p "$OUT"
cd "$OUT"
curl -fL --progress-bar -C - -o "$NAME" "$URL/$NAME"
curl -fsSL -o SHA256SUMS "$URL/SHA256SUMS"
expected="$(grep " \*$NAME\$" SHA256SUMS | cut -d' ' -f1)"
actual="$( (command -v sha256sum >/dev/null && sha256sum "$NAME" || shasum -a 256 "$NAME") | cut -d' ' -f1)"
if [ "$expected" != "$actual" ]; then
  echo "checksum mismatch for $NAME: expected $expected, got $actual" >&2
  exit 1
fi
if [ "$FLAVOR" = desktop ]; then
  echo "$OUT/$NAME"
  exit 0
fi
IMG="ubuntu-$RELEASE-server-riscv64.img"
xz -dc "$NAME" > "$IMG.tmp"
# Grow the image file; the partition table is fixed up by the guest on first boot.
python3 - "$IMG.tmp" "$DISK_SIZE" <<'PY'
import os, sys
units = {"K": 10, "M": 20, "G": 30}
s = sys.argv[2]
size = int(s[:-1]) << units[s[-1]] if s[-1] in units else int(s)
if os.path.getsize(sys.argv[1]) < size:
    os.truncate(sys.argv[1], size)
PY
PRISTINE="ubuntu-$RELEASE-server-riscv64-pristine.img"
# A copy-on-write clone where the file system supports it, a sparse copy otherwise.
cp -c "$IMG.tmp" "$PRISTINE" 2>/dev/null || cp --sparse=always "$IMG.tmp" "$PRISTINE" 2>/dev/null || cp "$IMG.tmp" "$PRISTINE"
mv "$IMG.tmp" "$IMG"
echo "$OUT/$IMG"
