#!/usr/bin/env bash
# Assembles the release packages for one target: the rust-v binary, the firmware it boots by
# default (OpenSBI fw_dynamic.bin and U-Boot u-boot.bin) and the docs.
#
#   scripts/package.sh TARGET [FIRMWARE_DIR]   e.g. scripts/package.sh aarch64-apple-darwin
#
# Expects target/TARGET/release/rust-v[.exe] to be built. FIRMWARE_DIR defaults to
# third_party/guest (scripts/build-guest-images.sh and scripts/build-uboot.sh). Writes to dist/:
#
#   every target   rust-v-VERSION-TARGET.tar.gz (.zip on Windows): the binary with firmware/
#                  next to it, where rust-v looks first; runs from wherever it is unpacked
#   Linux          rust-v_VERSION_ARCH.deb (needs dpkg-deb): /usr/bin/rust-v and
#                  /usr/share/rust-v/firmware
#   macOS          rust-v-VERSION-TARGET.pkg: /usr/local/bin/rust-v and
#                  /usr/local/share/rust-v/firmware; signed with $MACOS_INSTALLER_IDENTITY if set
#   Windows        rust-v-VERSION-TARGET.msi (needs WiX 5, `wix`): Program Files\rust-v, on PATH
#
# An installer whose tool is missing is skipped with a warning, unless $REQUIRE_INSTALLERS is
# set (as in CI). On macOS the binary is signed with the JIT entitlement: ad hoc, or with
# $MACOS_SIGNING_IDENTITY (with the hardened runtime, as notarization requires).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$1"
FIRMWARE="${2:-$ROOT/third_party/guest}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
NAME="rust-v-$VERSION-$TARGET"
DIST="$ROOT/dist"
STAGE="$DIST/$NAME"
EXE=rust-v
case "$TARGET" in *windows*) EXE=rust-v.exe ;; esac

skip() {
  if [ -n "${REQUIRE_INSTALLERS:-}" ]; then
    echo "error: $1" >&2
    exit 1
  fi
  echo "warning: $1, skipped" >&2
}

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

cd "$DIST"
case "$TARGET" in
  *windows*)
    (command -v zip >/dev/null && zip -qr "$NAME.zip" "$NAME") || powershell -Command "Compress-Archive -Path '$NAME' -DestinationPath '$NAME.zip' -Force"
    OUT=("$NAME.zip")
    ;;
  *)
    COPYFILE_DISABLE=1 tar -czf "$NAME.tar.gz" "$NAME"
    OUT=("$NAME.tar.gz")
    ;;
esac

# The installers lay files out the way firmware_dirs() expects: firmware/ beside the
# binary on Windows, share/rust-v/firmware beside its bin/ elsewhere.
case "$TARGET" in
  *linux*)
    if command -v dpkg-deb >/dev/null; then
      case "$TARGET" in x86_64*) ARCH=amd64 ;; aarch64*) ARCH=arm64 ;; *) ARCH="${TARGET%%-*}" ;; esac
      # Debian orders "~" before anything, so 1.0.0~rc.1 comes before 1.0.0.
      DEB_VERSION="${VERSION/-/\~}"
      DEB="$DIST/deb-$TARGET"
      rm -rf "$DEB" && mkdir -p "$DEB/DEBIAN" "$DEB/usr/bin" "$DEB/usr/share/rust-v" "$DEB/usr/share/doc/rust-v"
      install -m 755 "$STAGE/$EXE" "$DEB/usr/bin/"
      cp -r "$STAGE/firmware" "$DEB/usr/share/rust-v/"
      chmod 644 "$DEB/usr/share/rust-v/firmware/"*
      install -m 644 "$ROOT/README.md" "$DEB/usr/share/doc/rust-v/"
      SIZE="$(du -sk "$DEB/usr" | cut -f1)"
      # The newest glibc symbol version the binary uses is the oldest libc6 it runs on.
      GLIBC="$(objdump -T "$STAGE/$EXE" | grep -o 'GLIBC_[0-9.]*' | cut -d_ -f2 | sort -uV | tail -1)"
      sed -e "s/@VERSION@/$DEB_VERSION/" -e "s/@ARCH@/$ARCH/" -e "s/@SIZE@/$SIZE/" -e "s/@GLIBC@/$GLIBC/" \
        "$ROOT/packaging/linux/control" > "$DEB/DEBIAN/control"
      # The file is named with the plain version: GitHub turns a "~" in asset names into ".".
      dpkg-deb --root-owner-group --build "$DEB" "rust-v_${VERSION}_$ARCH.deb"
      OUT+=("rust-v_${VERSION}_$ARCH.deb")
      rm -rf "$DEB"
    else
      skip "dpkg-deb not found, no .deb"
    fi
    ;;
  *apple-darwin)
    PKG="$DIST/pkg-$TARGET"
    rm -rf "$PKG" && mkdir -p "$PKG/bin" "$PKG/share/rust-v" "$PKG/share/doc/rust-v"
    cp -p "$STAGE/$EXE" "$PKG/bin/"
    cp -r "$STAGE/firmware" "$PKG/share/rust-v/"
    cp "$ROOT/README.md" "$PKG/share/doc/rust-v/"
    # Extended attributes would land in the payload as ._ files.
    xattr -cr "$PKG"
    pkgbuild --root "$PKG" --install-location /usr/local --identifier io.github.padcom.rust-v \
      --version "${VERSION%%-*}" ${MACOS_INSTALLER_IDENTITY:+--sign "$MACOS_INSTALLER_IDENTITY" --timestamp} \
      "$NAME.pkg"
    OUT+=("$NAME.pkg")
    rm -rf "$PKG"
    ;;
  *windows*)
    if command -v wix >/dev/null; then
      case "$TARGET" in x86_64*) ARCH=x64 ;; aarch64*) ARCH=arm64 ;; esac
      # MSI versions are numeric: 1.0.0-rc.1 installs as 1.0.0.
      wix build -arch "$ARCH" -d "Version=${VERSION%%-*}" -d "Stage=$(cygpath -w "$STAGE")" \
        -o "$NAME.msi" "$(cygpath -w "$ROOT/packaging/windows/rust-v.wxs")"
      rm -f "$NAME.wixpdb"
      OUT+=("$NAME.msi")
    else
      skip "wix not found, no .msi"
    fi
    ;;
esac

ls -l "${OUT[@]}"
