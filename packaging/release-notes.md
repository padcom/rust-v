## Downloads

| Host | Installer | Archive (unpack and run in place) |
|------|-----------|-----------------------------------|
| Linux x86-64 | `rust-v_*_amd64.deb` (`sudo apt install ./rust-v_*_amd64.deb`) | `rust-v-*-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `rust-v_*_arm64.deb` | `rust-v-*-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `rust-v-*-aarch64-apple-darwin.pkg` | `rust-v-*-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `rust-v-*-x86_64-apple-darwin.pkg` | `rust-v-*-x86_64-apple-darwin.tar.gz` |
| Windows x86-64 | `rust-v-*-x86_64-pc-windows-msvc.msi` | `rust-v-*-x86_64-pc-windows-msvc.zip` |
| Windows arm64 | `rust-v-*-aarch64-pc-windows-msvc.msi` | `rust-v-*-aarch64-pc-windows-msvc.zip` |

The installers put `rust-v` on the `PATH` (`/usr/bin`, `/usr/local/bin`, or
`Program Files\rust-v` on Windows) along with the bundled OpenSBI and U-Boot firmware.
`SHA256SUMS` lists the checksums of all files.

Then start a distribution, which is downloaded on first use:

```sh
rust-v ubuntu     # Ubuntu Server
rust-v debian     # Debian 13
```

The Linux binaries need glibc 2.35 or newer (Ubuntu 22.04, Debian 12). macOS builds that
are not notarized are blocked by Gatekeeper when downloaded with a browser: allow the
`.pkg` under System Settings → Privacy & Security → Open Anyway, or clear the download's
quarantine flag first (`xattr -d com.apple.quarantine rust-v-*.pkg`, or the `.tar.gz`).
