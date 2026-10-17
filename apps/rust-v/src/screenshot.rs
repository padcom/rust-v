//! Periodic screenshots of the guest display as PNG files, for checking a guest without
//! looking at it (and for scripted tests).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rv_machine::display::Display;

/// Writes the display to `path` every `interval` while it changes.
pub fn spawn(display: Arc<Display>, path: PathBuf, interval: Duration) {
    std::thread::spawn(move || {
        let mut last = u64::MAX;
        loop {
            std::thread::sleep(interval);
            let snapshot = display.with(|frame, _| {
                (frame.serial != last && frame.width > 0)
                    .then(|| (frame.serial, frame.width, frame.height, frame.pixels.clone()))
            });
            let Some((serial, w, h, pixels)) = snapshot else { continue };
            last = serial;
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, encode_png(w, h, &pixels)).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    });
}

/// Encodes `0x00RRGGBB` pixels as an RGB PNG (uncompressed deflate blocks: no dependencies).
pub fn encode_png(width: u32, height: u32, pixels: &[u32]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((height * (1 + 3 * width)) as usize);
    for row in pixels.chunks_exact(width as usize) {
        raw.push(0); // filter: none
        for &p in row {
            raw.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        z.push(blocks.peek().is_none() as u8);
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    let _ = png.write_all(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn png_layout() {
        let png = encode_png(2, 1, &[0xff0000, 0x00ff00]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }
}
