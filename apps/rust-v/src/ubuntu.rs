//! `rust-v ubuntu`: downloads Ubuntu's preinstalled RISC-V server image once (checked
//! against Ubuntu's SHA256SUMS), keeps it in the user's data directory, and boots it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;
use sha2::{Digest, Sha256};

/// The release the emulator is tested with.
const DEFAULT_RELEASE: &str = "26.10";

#[derive(Parser)]
#[command(
    name = "rust-v ubuntu",
    about = "Download Ubuntu Server for RISC-V (once) and boot it",
    long_about = "Downloads Ubuntu's preinstalled RISC-V server image on first use, checks it against \
                  Ubuntu's SHA256SUMS, unpacks it into the data directory and boots it; later runs boot \
                  the same image, so the system keeps its changes.\n\nThe first boot sets the system up \
                  (cloud-init) for a few minutes; then log in as `ubuntu` with password `ubuntu` (you are \
                  asked for a new one). Options after `--` go to the emulator, e.g. \
                  `rust-v ubuntu -- --display window`."
)]
struct UbuntuArgs {
    /// Ubuntu release (one with a preinstalled RISC-V server image).
    #[arg(long, default_value = DEFAULT_RELEASE)]
    release: String,
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

/// Prepares the image and returns the emulator command line that boots it.
pub fn prepare(argv: impl Iterator<Item = String>) -> Result<Vec<String>, String> {
    let args = UbuntuArgs::parse_from(argv);
    let dir = args.dir.clone().map_or_else(data_dir, Ok)?.join("images");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let release = &args.release;
    let archive_name = format!("ubuntu-{release}-preinstalled-server-riscv64.img.xz");
    let archive = dir.join(&archive_name);
    let image = dir.join(format!("ubuntu-{release}-server-riscv64.img"));

    if args.fresh || !image.exists() {
        if !archive.exists() {
            download(release, &archive_name, &archive)?;
        }
        unpack(&archive, &image, args.size)?;
    }

    let mut cmd: Vec<String> =
        ["rust-v", "--memory", &args.memory, "--disk"].iter().map(|s| s.to_string()).collect();
    cmd.push(image.display().to_string());
    if args.ssh_port != 0 {
        // A port another program holds would stop the machine from starting.
        if std::net::TcpListener::bind(("127.0.0.1", args.ssh_port)).is_ok() {
            cmd.push("--forward".into());
            cmd.push(format!("{}:22", args.ssh_port));
            eprintln!("rust-v: ssh -p {} ubuntu@localhost reaches the guest", args.ssh_port);
        } else {
            eprintln!("rust-v: port {} is in use; the guest's SSH is not forwarded", args.ssh_port);
        }
    }
    cmd.extend(args.emulator);
    eprintln!("rust-v: booting {} (first boot: log in as ubuntu, password ubuntu)", image.display());
    Ok(cmd)
}

/// Downloads `name` from the release directory into `archive`, checked against SHA256SUMS.
fn download(release: &str, name: &str, archive: &Path) -> Result<(), String> {
    let url = format!("https://cdimage.ubuntu.com/ubuntu/releases/{release}/release");
    let sums = curl_text(&format!("{url}/SHA256SUMS"))?;
    let expected = sums
        .lines()
        .find_map(|l| l.strip_suffix(name)?.trim_end().strip_suffix(" *").map(str::to_string))
        .ok_or_else(|| {
            format!("{name} is not listed in {url}/SHA256SUMS (is {release} a release with one?)")
        })?;
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
    let actual = sha256(&partial)?;
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

fn sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
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

/// Decompresses `archive` into `image`, leaving runs of zeros as holes, and grows it to `size`.
fn unpack(archive: &Path, image: &Path, size: u64) -> Result<(), String> {
    eprintln!("rust-v: unpacking {}", archive.display());
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let input = File::open(archive).map_err(|e| err(archive, e))?;
    let mut xz = liblzma::read::XzDecoder::new(std::io::BufReader::with_capacity(1 << 20, input));
    let partial = image.with_extension("img.part");
    let mut out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&partial)
        .map_err(|e| err(&partial, e))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut len = 0u64;
    loop {
        // Fill the whole buffer, so that zero runs are judged a megabyte at a time.
        let mut n = 0;
        while n < buf.len() {
            match xz.read(&mut buf[n..]).map_err(|e| err(archive, e))? {
                0 => break,
                k => n += k,
            }
        }
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|&b| b == 0) {
            out.seek(SeekFrom::Current(n as i64)).map_err(|e| err(&partial, e))?;
        } else {
            out.write_all(&buf[..n]).map_err(|e| err(&partial, e))?;
        }
        len += n as u64;
    }
    out.set_len(len.max(size)).map_err(|e| err(&partial, e))?;
    drop(out);
    std::fs::rename(&partial, image).map_err(|e| err(image, e))
}
