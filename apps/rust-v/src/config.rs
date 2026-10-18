//! VM configuration files: the command-line options as TOML, e.g.
//!
//! ```toml
//! memory = "4G"
//! smp = 4
//! disk = ["ubuntu.img"]
//! display = "window"
//! forward = ["2222:22"]
//! no_sound = true
//! ```
//!
//! Keys are option names (`_` or `-`); arrays repeat an option; `true` turns a flag on.
//! Relative paths are relative to the file. Options given on the command line win.

use std::path::{Path, PathBuf};

/// Options whose values are paths (for `disk` and `share`, the part before the first comma).
const PATH_OPTIONS: &[&str] =
    &["bios", "kernel", "initrd", "disk", "screenshot", "dump-dtb", "share", "restore"];

/// Expands `--config FILE` in `args` into the options the file holds, placed before the
/// other command-line options so that those override them.
pub fn expand(args: Vec<String>) -> Result<Vec<String>, String> {
    let mut rest = Vec::with_capacity(args.len());
    let mut files = Vec::new();
    let mut it = args.into_iter();
    if let Some(program) = it.next() {
        rest.push(program);
    }
    while let Some(arg) = it.next() {
        if arg == "--config" {
            files.push(it.next().ok_or("--config needs a file")?);
        } else if let Some(file) = arg.strip_prefix("--config=") {
            files.push(file.to_string());
        } else {
            rest.push(arg);
        }
    }
    let mut from_files = Vec::new();
    for file in files {
        from_files.extend(options_from_file(Path::new(&file))?);
    }
    rest.splice(1..1, from_files);
    Ok(rest)
}

fn options_from_file(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let table: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut out = Vec::new();
    for (key, value) in table {
        let name = key.replace('_', "-");
        let values = match value {
            toml::Value::Array(items) => items,
            other => vec![other],
        };
        for value in values {
            let text = match value {
                toml::Value::Boolean(true) => {
                    out.push(format!("--{name}"));
                    continue;
                }
                toml::Value::Boolean(false) => continue,
                toml::Value::String(s) => s,
                toml::Value::Integer(n) => n.to_string(),
                other => return Err(format!("{}: unsupported value for `{key}`: {other}", path.display())),
            };
            let text = if PATH_OPTIONS.contains(&name.as_str()) { resolve(&dir, &text) } else { text };
            out.push(format!("--{name}"));
            out.push(text);
        }
    }
    Ok(out)
}

/// Makes the path part of `value` (up to the first comma) relative to `dir`.
fn resolve(dir: &Path, value: &str) -> String {
    let (path, options) = match value.split_once(',') {
        Some((p, o)) => (p, Some(o)),
        None => (value, None),
    };
    let path = if Path::new(path).is_absolute() { PathBuf::from(path) } else { dir.join(path) };
    match options {
        Some(o) => format!("{},{o}", path.display()),
        None => path.display().to_string(),
    }
}

/// Where firmware is looked for when `--bios`/`--kernel` are not given: `$RUST_V_FIRMWARE`,
/// next to the executable, a `share/rust-v/firmware` or (macOS) `Resources/firmware`
/// directory beside its `bin`, and this repository's `third_party/guest`.
pub fn firmware_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("RUST_V_FIRMWARE") {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(bin) = exe.parent()
    {
        dirs.push(bin.join("firmware"));
        if let Some(prefix) = bin.parent() {
            dirs.push(prefix.join("share/rust-v/firmware"));
            dirs.push(prefix.join("Resources/firmware"));
            // target/release/rust-v inside a checkout.
            if let Some(root) = prefix.parent() {
                dirs.push(root.join("third_party/guest"));
            }
        }
    }
    dirs.push(PathBuf::from("third_party/guest"));
    dirs
}

pub fn find_firmware(name: &str) -> Option<PathBuf> {
    firmware_dirs().into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_options_come_first_and_paths_resolve() {
        let dir = std::env::temp_dir().join(format!("rust-v-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("vm.toml");
        // Absolute on every host (on Windows, `/abs` is relative to the current drive).
        let abs = std::env::temp_dir().join("b.img").display().to_string();
        std::fs::write(
            &file,
            format!(
                "memory = \"4G\"\nsmp = 2\ndisk = [\"a.img,snapshot\", '{abs}']\nno_net = true\nstats = false\n"
            ),
        )
        .unwrap();
        let args = vec![
            "rust-v".to_string(),
            "--config".into(),
            file.display().to_string(),
            "--smp".into(),
            "8".into(),
        ];
        let out = expand(args).unwrap();
        let a = dir.join("a.img").display().to_string() + ",snapshot";
        assert_eq!(
            out,
            [
                "rust-v", "--disk", &a, "--disk", &abs, "--memory", "4G", "--no-net", "--smp", "2", "--smp",
                "8"
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
