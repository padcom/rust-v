#!/usr/bin/env bash
# Downloads the official Ubuntu RISC-V preinstalled server image, checks it against Ubuntu's
# SHA256SUMS and unpacks it into third_party/ubuntu/ubuntu-<release>-server-riscv64.img,
# grown to $DISK_SIZE (the guest's cloud-init grows the root partition into the free space).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/third_party/ubuntu"
RELEASE="${UBUNTU_RELEASE:-26.10}"
DISK_SIZE="${DISK_SIZE:-16G}"
NAME="ubuntu-$RELEASE-preinstalled-server-riscv64.img.xz"
URL="https://cdimage.ubuntu.com/ubuntu/releases/$RELEASE/release"

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
mv "$IMG.tmp" "$IMG"
echo "$OUT/$IMG"
