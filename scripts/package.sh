#!/usr/bin/env bash
# Assembles a release archive for one target: the rust-v binary, the firmware it boots by
# default (OpenSBI fw_dynamic.bin and U-Boot u-boot.bin, in firmware/ next to the binary,
# where rust-v looks first) and the docs.
#
#   scripts/package.sh TARGET [FIRMWARE_DIR]   e.g. scripts/package.sh aarch64-apple-darwin
#
# Expects target/TARGET/release/rust-v[.exe] to be built. FIRMWARE_DIR defaults to
# third_party/guest (scripts/build-guest-images.sh and scripts/build-uboot.sh). On macOS the
# binary is signed with the JIT entitlement: ad hoc, or with $MACOS_SIGNING_IDENTITY (with the
# hardened runtime, as notarization requires). Writes dist/rust-v-VERSION-TARGET.{tar.gz,zip}.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$1"
FIRMWARE="${2:-$ROOT/third_party/guest}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
NAME="rust-v-$VERSION-$TARGET"
STAGE="$ROOT/dist/$NAME"
EXE=rust-v
case "$TARGET" in *windows*) EXE=rust-v.exe ;; esac

rm -rf "$STAGE" && mkdir -p "$STAGE/firmware"
cp "$ROOT/target/$TARGET/release/$EXE" "$STAGE/"
cp "$FIRMWARE/fw_dynamic.bin" "$FIRMWARE/u-boot.bin" "$STAGE/firmware/"
cp "$ROOT/README.md" "$STAGE/"

case "$TARGET" in
  *apple-darwin)
    if [ -n "${MACOS_SIGNING_IDENTITY:-}" ]; then
      codesign --force --options runtime --timestamp --sign "$MACOS_SIGNING_IDENTITY" \
        --entitlements "$ROOT/packaging/macos/rust-v.entitlements" "$STAGE/$EXE"
    else
      codesign --force --sign - --entitlements "$ROOT/packaging/macos/rust-v.entitlements" "$STAGE/$EXE"
    fi
    ;;
esac

cd "$ROOT/dist"
case "$TARGET" in
  *windows*) (command -v zip >/dev/null && zip -qr "$NAME.zip" "$NAME") || powershell -Command "Compress-Archive -Path '$NAME' -DestinationPath '$NAME.zip' -Force" ;;
  *) tar -czf "$NAME.tar.gz" "$NAME" ;;
esac
ls -la "$ROOT/dist" | grep "$NAME"
