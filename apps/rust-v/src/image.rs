//! `rust-v image`: raw disk image management.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rust-v image", about = "Create and inspect raw disk images")]
struct ImageArgs {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an empty (sparse) image.
    Create {
        path: PathBuf,
        /// Size, e.g. 32G.
        #[arg(long, value_parser = crate::parse_size)]
        size: u64,
    },
    /// Grow (or, with --shrink, shrink) an image. The guest grows its partitions itself
    /// (Ubuntu's cloud images do on boot).
    Resize {
        path: PathBuf,
        #[arg(long, value_parser = crate::parse_size)]
        size: u64,
        /// Allow making the image smaller, cutting off its end.
        #[arg(long)]
        shrink: bool,
    },
    /// Copy an image; copy-on-write where the file system supports it (APFS, Btrfs, XFS),
    /// so a fresh copy of a large image is instant and takes no space.
    Clone { source: PathBuf, destination: PathBuf },
    /// Show an image's size, the space it takes, and its partition table.
    Info { path: PathBuf },
}

pub fn main(args: impl Iterator<Item = String>) -> Result<(), String> {
    let args = ImageArgs::parse_from(args);
    match args.command {
        Command::Create { path, size } => {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            file.set_len(size).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("created {} ({})", path.display(), human(size));
        }
        Command::Resize { path, size, shrink } => {
            let file =
                OpenOptions::new().write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let old = file.metadata().map_err(|e| e.to_string())?.len();
            if size < old && !shrink {
                return Err(format!(
                    "{} is {}; pass --shrink to make it smaller",
                    path.display(),
                    human(old)
                ));
            }
            file.set_len(size).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("{}: {} -> {}", path.display(), human(old), human(size));
        }
        Command::Clone { source, destination } => {
            if destination.exists() {
                return Err(format!("{} already exists", destination.display()));
            }
            // std::fs::copy clones on APFS and uses copy_file_range (reflinks) on Linux.
            std::fs::copy(&source, &destination).map_err(|e| format!("{}: {e}", source.display()))?;
            println!("{} -> {}", source.display(), destination.display());
        }
        Command::Info { path } => info(&path)?,
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

fn info(path: &PathBuf) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    println!("{}", path.display());
    println!("  size:      {} ({} bytes)", human(meta.len()), meta.len());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        println!("  allocated: {}", human(meta.blocks() * 512));
    }
    let mut head = vec![0u8; 34 * 512];
    let n = file.read(&mut head).map_err(|e| e.to_string())?;
    head.truncate(n);
    if head.len() >= 1024 && &head[512..520] == b"EFI PART" {
        let u32_at = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        let u64_at = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        let entries_lba = u64_at(&head, 512 + 72);
        let count = u32_at(&head, 512 + 80) as usize;
        let size = u32_at(&head, 512 + 84) as usize;
        let mut table = vec![0u8; count * size];
        file.seek(SeekFrom::Start(entries_lba * 512)).map_err(|e| e.to_string())?;
        file.read_exact(&mut table).map_err(|e| e.to_string())?;
        println!("  partitions (GPT):");
        for (i, e) in table.chunks_exact(size).enumerate() {
            let (first, last) = (u64_at(e, 32), u64_at(e, 40));
            if first == 0 {
                continue;
            }
            let name: String =
                char::decode_utf16(e[56..128].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])))
                    .map(|c| c.unwrap_or('?'))
                    .take_while(|&c| c != '\0')
                    .collect();
            println!("    {:>2}  {:>10}  {name}", i + 1, human((last + 1 - first) * 512));
        }
        let backup = u64_at(&head, 512 + 32);
        if (backup + 1) * 512 != meta.len() {
            println!("  note: the backup GPT header is not at the end (the image was resized)");
        }
    } else if head.len() >= 512 && head[510] == 0x55 && head[511] == 0xaa {
        println!("  partitions (MBR):");
        for i in 0..4 {
            let e = &head[446 + 16 * i..446 + 16 * (i + 1)];
            let kind = e[4];
            let sectors = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
            if kind != 0 {
                println!("    {}  {:>10}  type {kind:#04x}", i + 1, human(sectors * 512));
            }
        }
    } else {
        println!("  no partition table");
    }
    Ok(())
}
