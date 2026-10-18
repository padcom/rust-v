//! `rust-v ubuntu` and `rust-v debian`: download a distribution's official RISC-V disk image
//! once (checked against the published checksums), keep it in the user's data directory,
//! and boot it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{CommandFactory, FromArgMatches, Parser};
use sha2::{Digest, Sha256, Sha512};

/// A distribution's published image and how to unpack it.
pub struct Distro {
    pub name: &'static str,
    /// The subcommand, `rust-v NAME`.
    command: &'static str,
    title: &'static str,
    /// The release the emulator is tested with.
    default_release: &'static str,
    /// Download directory, archive file name and checksum file for a release.
    source: fn(&str) -> Result<Source, String>,
    /// How the disk sits in the archive.
    format: Format,
    /// What to tell the user about logging in on the first boot.
    login: &'static str,
}

struct Source {
    url: String,
    archive: String,
    sums: &'static str,
}

enum Format {
    /// The raw disk, xz-compressed.
    Xz,
    /// A tar archive, xz-compressed, holding the raw disk under this name.
    TarXz(&'static str),
}

pub const UBUNTU: Distro = Distro {
    name: "ubuntu",
    command: "rust-v ubuntu",
    title: "Ubuntu Server",
    default_release: "26.10",
    source: |release| {
        Ok(Source {
            url: format!("https://cdimage.ubuntu.com/ubuntu/releases/{release}/release"),
            archive: format!("ubuntu-{release}-preinstalled-server-riscv64.img.xz"),
            sums: "SHA256SUMS",
        })
    },
    format: Format::Xz,
    login: "first boot: after cloud-init finishes, log in as ubuntu, password ubuntu",
};

pub const DEBIAN: Distro = Distro {
    name: "debian",
    command: "rust-v debian",
    title: "Debian",
    default_release: "trixie",
    source: |release| {
        let version = match release {
            "trixie" | "13" => 13,
            "forky" | "14" => 14,
            _ => return Err(format!("unknown Debian release {release} (trixie or forky)")),
        };
        let codename = if version == 13 { "trixie" } else { "forky" };
        Ok(Source {
            url: format!("https://cloud.debian.org/images/cloud/{codename}/latest"),
            archive: format!("debian-{version}-nocloud-riscv64.tar.xz"),
            sums: "SHA512SUMS",
        })
    },
    format: Format::TarXz("disk.raw"),
    login: "the first boot asks for a time zone and a root password: set one (skipped, root \
            stays locked), then log in as root",
};

#[derive(Parser)]
struct DistroArgs {
    /// Release (one with a RISC-V image).
    #[arg(long)]
    release: Option<String>,
    /// Where images are kept. Default: RUST_V_HOME, or the user's data directory.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Size the image is grown to before its first boot (the guest grows its partition).
    #[arg(long, default_value = "16G", value_parser = crate::parse_size)]
    size: u64,
    /// Start over from the downloaded image, discarding the system's changes.
    #[arg(long)]
    fresh: bool,
    /// RAM size.
    #[arg(short, long, default_value = "4G")]
    memory: String,
    /// Host port forwarded to the guest's SSH server (0: none).
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
    /// Further emulator options (after `--`).
    #[arg(last = true)]
    emulator: Vec<String>,
}

/// The per-user directory for downloaded images.
fn data_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("RUST_V_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let env = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|h| h.join(".local/share")))
    };
    base.map(|b| b.join("rust-v")).ok_or_else(|| "no home directory; pass --dir or set RUST_V_HOME".into())
}

/// Prepares `distro`'s image and returns the emulator command line that boots it.
pub fn prepare(distro: &Distro, argv: impl Iterator<Item = String>) -> Result<Vec<String>, String> {
    let name = distro.name;
    let command = DistroArgs::command()
        .name(distro.command)
        .about(format!("Download {} for RISC-V (once) and boot it", distro.title))
        .long_about(format!(
            "Downloads {}'s official RISC-V disk image on first use, checks it against the published \
             checksums, unpacks it into the data directory and boots it; later runs boot the same \
             image, so the system keeps its changes. The default release is {}.\n\nOn the console: \
             {}. Options after `--` go to the emulator, e.g. `rust-v {name} -- --display window`.",
            distro.title, distro.default_release, distro.login
        ));
    let args = DistroArgs::from_arg_matches(&command.get_matches_from(argv)).map_err(|e| e.to_string())?;
    let dir = args.dir.clone().map_or_else(data_dir, Ok)?.join("images");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let release = args.release.as_deref().unwrap_or(distro.default_release);
    let source = (distro.source)(release)?;
    let archive = dir.join(&source.archive);
    let image = dir.join(format!("{name}-{release}-riscv64.img"));

    if args.fresh || !image.exists() {
        if !archive.exists() {
            download(&source, &archive)?;
        }
        unpack(&distro.format, &archive, &image, args.size)?;
    }

    let mut cmd: Vec<String> =
        ["rust-v", "--memory", &args.memory, "--disk"].iter().map(|s| s.to_string()).collect();
    cmd.push(image.display().to_string());
    if args.ssh_port != 0 {
        // A port another program holds would stop the machine from starting.
        if std::net::TcpListener::bind(("127.0.0.1", args.ssh_port)).is_ok() {
            cmd.push("--forward".into());
            cmd.push(format!("{}:22", args.ssh_port));
            eprintln!("rust-v: host port {} reaches the guest's SSH server", args.ssh_port);
        } else {
            eprintln!("rust-v: port {} is in use; the guest's SSH is not forwarded", args.ssh_port);
        }
    }
    cmd.extend(args.emulator);
    eprintln!("rust-v: booting {} ({})", image.display(), distro.login);
    Ok(cmd)
}

/// Downloads the archive, checked against the release's checksum file.
fn download(source: &Source, archive: &Path) -> Result<(), String> {
    let Source { url, archive: name, sums } = source;
    let list = curl_text(&format!("{url}/{sums}"))?;
    // Lines are `HASH  NAME` or `HASH *NAME`.
    let expected = list
        .lines()
        .find_map(|l| {
            let (hash, file) = l.split_once(' ')?;
            (file.trim_start_matches([' ', '*']) == name).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| format!("{name} is not listed in {url}/{sums}"))?;
    let partial = archive.with_extension("xz.part");
    eprintln!("rust-v: downloading {url}/{name}");
    // Resumes a partial download; a complete one makes curl fail, which the check settles.
    let status = Command::new("curl")
        .args(["-fL", "--progress-bar", "-C", "-", "-o"])
        .arg(&partial)
        .arg(format!("{url}/{name}"))
        .status()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !status.success() && !partial.exists() {
        return Err(format!("downloading {url}/{name} failed ({status})"));
    }
    eprintln!("rust-v: checking the download");
    let actual =
        if sums.starts_with("SHA512") { hash::<Sha512>(&partial)? } else { hash::<Sha256>(&partial)? };
    if actual != expected {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "{name}: checksum mismatch (expected {expected}, got {actual}); run again to retry"
        ));
    }
    std::fs::rename(&partial, archive).map_err(|e| format!("{}: {e}", archive.display()))
}

fn curl_text(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .map_err(|e| format!("cannot run curl (needed for downloading): {e}"))?;
    if !out.status.success() {
        return Err(format!("{url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    String::from_utf8(out.stdout).map_err(|_| format!("{url}: not text"))
}

fn hash<D: Digest>(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = D::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Unpacks the disk from `archive` into `image` and grows it to `size`.
fn unpack(format: &Format, archive: &Path, image: &Path, size: u64) -> Result<(), String> {
    eprintln!("rust-v: unpacking {}", archive.display());
    let input = File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let xz = liblzma::read::XzDecoder::new(std::io::BufReader::with_capacity(1 << 20, input));
    let partial = image.with_extension("img.part");
    let bad = |e: std::io::Error| format!("{}: {e}", archive.display());
    match format {
        Format::Xz => write_sparse(xz, &partial, size).map_err(bad)?,
        Format::TarXz(member) => {
            let mut tar = tar::Archive::new(xz);
            let mut found = false;
            for entry in tar.entries().map_err(bad)? {
                let entry = entry.map_err(bad)?;
                if entry.path().map_err(bad)?.as_os_str() == *member {
                    write_sparse(entry, &partial, size).map_err(bad)?;
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(format!("{}: no {member} in it", archive.display()));
            }
        }
    }
    std::fs::rename(&partial, image).map_err(|e| format!("{}: {e}", image.display()))
}

/// Copies `input` into a new file at `path`, leaving runs of zeros as holes, and grows the
/// file to at least `size`.
fn write_sparse(mut input: impl Read, path: &Path, size: u64) -> std::io::Result<()> {
    let mut out = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut len = 0u64;
    loop {
        // Fill the whole buffer, so that zero runs are judged a megabyte at a time.
        let mut n = 0;
        while n < buf.len() {
            match input.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|&b| b == 0) {
            out.seek(SeekFrom::Current(n as i64))?;
        } else {
            out.write_all(&buf[..n])?;
        }
        len += n as u64;
    }
    out.set_len(len.max(size))
}
