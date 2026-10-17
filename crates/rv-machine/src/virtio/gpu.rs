//! virtio-gpu (§5.7) in 2D mode: guest-allocated resources are copied into host memory on
//! TRANSFER_TO_HOST_2D and shown on RESOURCE_FLUSH. One scanout; a hardware cursor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rv_core::GuestMemory;

use super::{Activation, Chain, VirtioDevice, device_type, read_config_bytes};
use crate::display::{CURSOR_SIZE, Display};

pub(super) const CONTROLQ: usize = 0;
pub(super) const CURSORQ: usize = 1;

pub(super) mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
    pub const UPDATE_CURSOR: u32 = 0x0300;
    pub const MOVE_CURSOR: u32 = 0x0301;

    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1205;
}

pub(super) const FLAG_FENCE: u32 = 1;
pub(super) const HEADER: usize = 24;
const EVENT_DISPLAY: u32 = 1;
pub(super) const MAX_SCANOUTS: usize = 16;
/// Host memory all resources together may use.
const MAX_RESOURCE_BYTES: u64 = 1 << 30;

/// Pixel formats (`enum virtio_gpu_formats`), by their byte order in memory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    /// B, G, R, A/X: already the host layout.
    Bgrx,
    /// A/X, R, G, B.
    Xrgb,
    /// R, G, B, A/X.
    Rgbx,
    /// A/X, B, G, R.
    Xbgr,
}

impl Format {
    pub(super) fn from_virtio(f: u32) -> Option<Self> {
        Some(match f {
            1 | 2 => Format::Bgrx,
            3 | 4 => Format::Xrgb,
            67 | 134 => Format::Rgbx,
            68 | 121 => Format::Xbgr,
            _ => return None,
        })
    }

    /// Converts a pixel as stored (little-endian u32 of its bytes) to `0xAARRGGBB`.
    #[inline]
    pub(super) fn to_argb(self, p: u32) -> u32 {
        match self {
            Format::Bgrx => p,
            Format::Xrgb => p.swap_bytes(),
            Format::Rgbx => (p & 0xff00_ff00) | ((p & 0xff) << 16) | ((p >> 16) & 0xff),
            Format::Xbgr => p.rotate_right(8),
        }
    }
}

struct Resource {
    width: u32,
    height: u32,
    format: Format,
    /// The guest's format code (`enum virtio_gpu_formats`).
    virtio_format: u32,
    /// Host copy, in the guest's pixel format.
    pixels: Vec<u32>,
    /// Guest memory backing the resource: (address, length) pieces in order.
    backing: Vec<(u64, u32)>,
    /// Byte offset within the resource at which each backing piece starts.
    starts: Vec<u64>,
}

impl Resource {
    fn set_backing(&mut self, backing: Vec<(u64, u32)>) {
        let mut at = 0;
        self.starts = backing
            .iter()
            .map(|&(_, len)| {
                let start = at;
                at += len as u64;
                start
            })
            .collect();
        self.backing = backing;
    }

    /// Copies `out.len()` bytes starting at byte `offset` of the backing into `out`.
    fn read_backing(&self, mem: &GuestMemory, offset: u64, out: &mut [u8]) -> bool {
        // The piece containing `offset`.
        let mut i = self.starts.partition_point(|&s| s <= offset).saturating_sub(1);
        let mut done = 0;
        while done < out.len() {
            let Some(&(addr, len)) = self.backing.get(i) else { return false };
            let within = offset + done as u64 - self.starts[i];
            if within >= len as u64 {
                i += 1;
                continue;
            }
            let n = ((len as u64 - within) as usize).min(out.len() - done);
            if mem.read(addr + within, &mut out[done..done + n]).is_err() {
                return false;
            }
            done += n;
            i += 1;
        }
        true
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// State the front end can reach to announce display changes.
pub(super) struct Shared {
    pub events_read: u32,
    pub active: Option<Activation>,
}

pub struct Gpu {
    display: Arc<Display>,
    resources: HashMap<u32, Resource>,
    resource_bytes: u64,
    /// Resource shown on scanout 0, and the part of it that is shown.
    scanout: Option<(u32, Rect)>,
    shared: Arc<Mutex<Shared>>,
}

/// Lets the front end tell the guest that the preferred display size changed.
#[derive(Clone)]
pub struct GpuHandle {
    pub(super) display: Arc<Display>,
    pub(super) shared: Arc<Mutex<Shared>>,
}

impl GpuHandle {
    pub fn resize(&self, width: u32, height: u32) {
        if self.display.preferred_size() == (width, height) {
            return;
        }
        self.display.set_preferred_size(width, height);
        let mut s = self.shared.lock().unwrap();
        s.events_read |= EVENT_DISPLAY;
        if let Some(a) = &s.active {
            a.irq.config();
        }
    }
}

pub(super) fn trace() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("RV_GPU_TRACE").is_some())
}

pub(super) fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

pub(super) fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}

pub(super) fn rect_at(b: &[u8], at: usize) -> Rect {
    Rect { x: u32_at(b, at), y: u32_at(b, at + 4), width: u32_at(b, at + 8), height: u32_at(b, at + 12) }
}

/// The GET_DISPLAY_INFO payload: one enabled scanout of the display's preferred size.
pub(super) fn display_info(display: &Display) -> Vec<u8> {
    let (w, h) = display.preferred_size();
    let mut info = vec![0u8; 24 * MAX_SCANOUTS];
    info[8..12].copy_from_slice(&w.to_le_bytes());
    info[12..16].copy_from_slice(&h.to_le_bytes());
    info[16..20].copy_from_slice(&1u32.to_le_bytes()); // enabled
    info
}

impl Gpu {
    pub fn new(display: Arc<Display>) -> (Self, GpuHandle) {
        let shared = Arc::new(Mutex::new(Shared { events_read: 0, active: None }));
        let handle = GpuHandle { display: display.clone(), shared: shared.clone() };
        let gpu = Gpu { display, resources: HashMap::new(), resource_bytes: 0, scanout: None, shared };
        (gpu, handle)
    }

    /// Handles one control command; returns the response type and any payload after the header.
    fn command(&mut self, mem: &GuestMemory, req: &[u8]) -> (u32, Vec<u8>) {
        let kind = u32_at(req, 0);
        let body = &req[HEADER.min(req.len())..];
        match kind {
            cmd::GET_DISPLAY_INFO => (cmd::OK_DISPLAY_INFO, display_info(&self.display)),
            cmd::RESOURCE_CREATE_2D => {
                let (id, format, width, height) =
                    (u32_at(body, 0), u32_at(body, 4), u32_at(body, 8), u32_at(body, 12));
                let Some(format) = Format::from_virtio(format) else {
                    return (cmd::ERR_INVALID_PARAMETER, vec![]);
                };
                let bytes = 4 * width as u64 * height as u64;
                if id == 0 || self.resources.contains_key(&id) || width == 0 || height == 0 {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                }
                if self.resource_bytes + bytes > MAX_RESOURCE_BYTES {
                    return (cmd::ERR_OUT_OF_MEMORY, vec![]);
                }
                self.resource_bytes += bytes;
                let pixels = vec![0; (width * height) as usize];
                self.resources.insert(
                    id,
                    Resource {
                        width,
                        height,
                        format,
                        virtio_format: u32_at(body, 4),
                        pixels,
                        backing: Vec::new(),
                        starts: Vec::new(),
                    },
                );
                (cmd::OK_NODATA, vec![])
            }
            cmd::RESOURCE_UNREF => {
                let id = u32_at(body, 0);
                match self.resources.remove(&id) {
                    Some(r) => {
                        self.resource_bytes -= 4 * r.width as u64 * r.height as u64;
                        if self.scanout.is_some_and(|(s, _)| s == id) {
                            self.scanout = None;
                        }
                        (cmd::OK_NODATA, vec![])
                    }
                    None => (cmd::ERR_INVALID_RESOURCE_ID, vec![]),
                }
            }
            cmd::RESOURCE_ATTACH_BACKING => {
                let (id, count) = (u32_at(body, 0), u32_at(body, 4) as usize);
                let Some(r) = self.resources.get_mut(&id) else {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                };
                if body.len() < 8 + 16 * count {
                    return (cmd::ERR_INVALID_PARAMETER, vec![]);
                }
                r.set_backing(
                    (0..count).map(|i| (u64_at(body, 8 + 16 * i), u32_at(body, 16 + 16 * i))).collect(),
                );
                (cmd::OK_NODATA, vec![])
            }
            cmd::RESOURCE_DETACH_BACKING => match self.resources.get_mut(&u32_at(body, 0)) {
                Some(r) => {
                    r.set_backing(Vec::new());
                    (cmd::OK_NODATA, vec![])
                }
                None => (cmd::ERR_INVALID_RESOURCE_ID, vec![]),
            },
            cmd::SET_SCANOUT => {
                let (r, scanout, id) = (rect_at(body, 0), u32_at(body, 16), u32_at(body, 20));
                if scanout != 0 {
                    return (cmd::ERR_INVALID_SCANOUT_ID, vec![]);
                }
                if id == 0 {
                    self.scanout = None;
                    self.display.update(|f| {
                        f.width = 0;
                        f.height = 0;
                        f.pixels.clear();
                    });
                    return (cmd::OK_NODATA, vec![]);
                }
                let Some(res) = self.resources.get(&id) else {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                };
                if r.width == 0 || r.height == 0 || r.x + r.width > res.width || r.y + r.height > res.height {
                    return (cmd::ERR_INVALID_PARAMETER, vec![]);
                }
                self.scanout = Some((id, r));
                self.present(Rect { x: r.x, y: r.y, width: r.width, height: r.height });
                (cmd::OK_NODATA, vec![])
            }
            cmd::TRANSFER_TO_HOST_2D => {
                let (r, offset, id) = (rect_at(body, 0), u64_at(body, 16), u32_at(body, 24));
                let Some(res) = self.resources.get_mut(&id) else {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                };
                if r.x.checked_add(r.width).is_none_or(|e| e > res.width)
                    || r.y.checked_add(r.height).is_none_or(|e| e > res.height)
                {
                    return (cmd::ERR_INVALID_PARAMETER, vec![]);
                }
                let stride = 4 * res.width as u64;
                let mut row = vec![0u8; 4 * r.width as usize];
                for h in 0..r.height {
                    // As in QEMU: the source rows start at `offset` and are `stride` apart.
                    let src = offset + stride * h as u64;
                    if !res.read_backing(mem, src, &mut row) {
                        return (cmd::ERR_UNSPEC, vec![]);
                    }
                    let dst = ((r.y + h) * res.width + r.x) as usize;
                    for (i, px) in row.as_chunks::<4>().0.iter().enumerate() {
                        res.pixels[dst + i] = u32::from_le_bytes(*px);
                    }
                }
                (cmd::OK_NODATA, vec![])
            }
            cmd::RESOURCE_FLUSH => {
                let (r, id) = (rect_at(body, 0), u32_at(body, 16));
                if !self.resources.contains_key(&id) {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                }
                if self.scanout.is_some_and(|(s, _)| s == id) {
                    self.present(r);
                }
                (cmd::OK_NODATA, vec![])
            }
            _ => (cmd::ERR_UNSPEC, vec![]),
        }
    }

    /// Copies rectangle `r` (in resource coordinates) of the scanout resource to the display.
    fn present(&self, r: Rect) {
        let Some((id, view)) = self.scanout else { return };
        let Some(res) = self.resources.get(&id) else { return };
        self.display.update(|frame| {
            if frame.width != view.width || frame.height != view.height {
                frame.width = view.width;
                frame.height = view.height;
                frame.pixels = vec![0; (view.width * view.height) as usize];
                // A new mode: refresh all of it.
                return copy_rect(frame, res, view, view);
            }
            copy_rect(frame, res, view, r);
        });
    }

    fn cursor_command(&mut self, req: &[u8]) {
        let kind = u32_at(req, 0);
        let body = &req[HEADER.min(req.len())..];
        let (x, y) = (u32_at(body, 4), u32_at(body, 8));
        match kind {
            cmd::UPDATE_CURSOR => {
                let (id, hot_x, hot_y) = (u32_at(body, 16), u32_at(body, 20), u32_at(body, 24));
                let image = self.resources.get(&id).map(|res| {
                    let mut img = vec![0u32; (CURSOR_SIZE * CURSOR_SIZE) as usize];
                    for cy in 0..CURSOR_SIZE.min(res.height) {
                        for cx in 0..CURSOR_SIZE.min(res.width) {
                            let p = res.pixels[(cy * res.width + cx) as usize];
                            img[(cy * CURSOR_SIZE + cx) as usize] = res.format.to_argb(p);
                        }
                    }
                    img
                });
                self.display.update_cursor(|c| {
                    c.visible = image.is_some();
                    c.image = image.unwrap_or_default();
                    (c.hot_x, c.hot_y, c.x, c.y) = (hot_x, hot_y, x, y);
                });
            }
            cmd::MOVE_CURSOR => self.display.update_cursor(|c| (c.x, c.y) = (x, y)),
            _ => {}
        }
    }
}

fn copy_rect(frame: &mut crate::display::Frame, res: &Resource, view: Rect, r: Rect) {
    // Clip to the visible part of the resource.
    let x0 = r.x.max(view.x);
    let y0 = r.y.max(view.y);
    let x1 = (r.x.saturating_add(r.width)).min(view.x + view.width);
    let y1 = (r.y.saturating_add(r.height)).min(view.y + view.height);
    for y in y0..y1 {
        let src = (y * res.width) as usize;
        let dst = ((y - view.y) * view.width) as usize;
        for x in x0..x1 {
            frame.pixels[dst + (x - view.x) as usize] =
                res.format.to_argb(res.pixels[src + x as usize]) & 0x00ff_ffff;
        }
    }
}

impl VirtioDevice for Gpu {
    fn device_type(&self) -> u16 {
        device_type::GPU
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![256, 16]
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = [0u8; 16];
        config[0..4].copy_from_slice(&self.shared.lock().unwrap().events_read.to_le_bytes());
        config[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts
        read_config_bytes(&config, offset, data);
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) {
        // events_clear
        if offset == 4 && data.len() == 4 {
            self.shared.lock().unwrap().events_read &= !u32::from_le_bytes(data.try_into().unwrap());
        }
    }

    fn activate(&mut self, activation: Activation) {
        self.shared.lock().unwrap().active = Some(activation);
    }

    fn notify(&mut self, queue: usize) {
        let Some(a) = self.shared.lock().unwrap().active.clone() else { return };
        let read = |chain: &Chain| {
            let mut req = vec![0u8; chain.readable_len().min(1 << 20) as usize];
            let n = chain.reader(&a.mem).read(&mut req).unwrap_or(0);
            req.truncate(n);
            req
        };
        match queue {
            CONTROLQ => a.drain(CONTROLQ, |chain| {
                let req = read(chain);
                let (kind, payload) =
                    if req.len() < HEADER { (cmd::ERR_UNSPEC, vec![]) } else { self.command(&a.mem, &req) };
                if trace() {
                    let t = u32_at(&req, 0);
                    if !matches!(t, cmd::TRANSFER_TO_HOST_2D | cmd::RESOURCE_FLUSH) || kind >= cmd::ERR_UNSPEC
                    {
                        eprintln!(
                            "virtio-gpu: {t:#06x} {:02x?} -> {kind:#06x}\r",
                            &req[HEADER.min(req.len())..req.len().min(HEADER + 32)]
                        );
                    }
                }
                // The response header echoes the fence.
                let mut resp = vec![0u8; HEADER];
                resp[0..4].copy_from_slice(&kind.to_le_bytes());
                let flags = u32_at(&req, 4);
                if flags & FLAG_FENCE != 0 {
                    resp[4..8].copy_from_slice(&FLAG_FENCE.to_le_bytes());
                    resp[8..20].copy_from_slice(&req[8..20]);
                }
                resp.extend_from_slice(&payload);
                chain.writer(&a.mem).write(&resp).unwrap_or(0) as u32
            }),
            CURSORQ => a.drain(CURSORQ, |chain| {
                let req = read(chain);
                self.cursor_command(&req);
                0
            }),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.resources.clear();
        self.resource_bytes = 0;
        self.scanout = None;
        let mut s = self.shared.lock().unwrap();
        s.active = None;
        s.events_read = 0;
    }

    fn pci_class(&self) -> u32 {
        0x03_80_00
    }

    fn save_state(&self, w: &mut rv_core::snapshot::Writer) {
        w.u32(self.shared.lock().unwrap().events_read);
        w.u64(self.resources.len() as u64);
        for (&id, res) in &self.resources {
            w.u32(id);
            w.u32(res.width);
            w.u32(res.height);
            w.u32(res.virtio_format);
            w.bytes(&res.pixels.iter().flat_map(|p| p.to_le_bytes()).collect::<Vec<u8>>());
            w.u64(res.backing.len() as u64);
            for &(addr, len) in &res.backing {
                w.u64(addr);
                w.u32(len);
            }
        }
        match self.scanout {
            Some((id, r)) => {
                w.bool(true);
                for v in [id, r.x, r.y, r.width, r.height] {
                    w.u32(v);
                }
            }
            None => w.bool(false),
        }
    }

    fn restore_state(&mut self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        self.shared.lock().unwrap().events_read = r.u32()?;
        self.resources.clear();
        self.resource_bytes = 0;
        for _ in 0..r.u64()? {
            let (id, width, height, virtio_format) = (r.u32()?, r.u32()?, r.u32()?, r.u32()?);
            let format = Format::from_virtio(virtio_format).ok_or("bad GPU resource in snapshot")?;
            let pixels: Vec<u32> =
                r.bytes()?.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).collect();
            if pixels.len() != (width * height) as usize {
                return Err("bad GPU resource in snapshot".into());
            }
            let mut backing = Vec::new();
            for _ in 0..r.u64()? {
                backing.push((r.u64()?, r.u32()?));
            }
            let mut res = Resource {
                width,
                height,
                format,
                virtio_format,
                pixels,
                backing: Vec::new(),
                starts: Vec::new(),
            };
            res.set_backing(backing);
            self.resource_bytes += 4 * width as u64 * height as u64;
            self.resources.insert(id, res);
        }
        self.scanout = if r.bool()? {
            let id = r.u32()?;
            Some((id, Rect { x: r.u32()?, y: r.u32()?, width: r.u32()?, height: r.u32()? }))
        } else {
            None
        };
        if let Some((_, view)) = self.scanout {
            self.present(view);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_formats() {
        // The pixel with R=0x11, G=0x22, B=0x33, A=0x44 in each memory order.
        let argb = 0x4411_2233;
        assert_eq!(Format::Bgrx.to_argb(u32::from_le_bytes([0x33, 0x22, 0x11, 0x44])), argb);
        assert_eq!(Format::Xrgb.to_argb(u32::from_le_bytes([0x44, 0x11, 0x22, 0x33])), argb);
        assert_eq!(Format::Rgbx.to_argb(u32::from_le_bytes([0x11, 0x22, 0x33, 0x44])), argb);
        assert_eq!(Format::Xbgr.to_argb(u32::from_le_bytes([0x44, 0x33, 0x22, 0x11])), argb);
    }
}
