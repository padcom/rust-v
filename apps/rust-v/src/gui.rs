//! The desktop window: shows the guest display (scaled to fit, keeping its aspect ratio) with
//! the guest's cursor, and turns keyboard and mouse input into virtio-input events.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rv_machine::display::{CURSOR_SIZE, Display};
use rv_machine::virtio::gpu::GpuHandle;
use rv_machine::virtio::input::{ABS_MAX, InputHandle, ev};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::keymap::evdev_code;

pub enum UserEvent {
    /// The guest display changed.
    Redraw,
    /// The machine stopped.
    Quit,
}

/// Lets the machine thread wake the window.
pub struct Waker {
    proxy: EventLoopProxy<UserEvent>,
    pending: AtomicBool,
}

impl Waker {
    pub fn redraw(&self) {
        // One outstanding redraw is enough, however many updates arrive meanwhile.
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.proxy.send_event(UserEvent::Redraw);
        }
    }

    pub fn quit(&self) {
        let _ = self.proxy.send_event(UserEvent::Quit);
    }
}

pub struct Devices {
    pub display: Arc<Display>,
    pub gpu: GpuHandle,
    pub keyboard: InputHandle,
    pub tablet: InputHandle,
}

/// Where the guest frame lands in the window (physical pixels).
#[derive(Clone, Copy, Default)]
struct Placement {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    frame_width: u32,
    frame_height: u32,
}

struct App {
    devices: Devices,
    waker: Arc<Waker>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    placement: Placement,
    /// Wheel movement not yet sent, in notches.
    scroll: (f64, f64),
    /// The window size to announce to the guest once resizing settles.
    pending_resize: Option<(Instant, u32, u32)>,
}

const RESIZE_SETTLE: Duration = Duration::from_millis(400);

pub fn run(devices: Devices, event_loop: EventLoop<UserEvent>, waker: Arc<Waker>) {
    let mut app = App {
        devices,
        waker,
        window: None,
        surface: None,
        placement: Placement::default(),
        scroll: (0.0, 0.0),
        pending_resize: None,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("rust-v: window: {e}\r");
    }
}

pub fn event_loop() -> Result<(EventLoop<UserEvent>, Arc<Waker>), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
    let waker = Arc::new(Waker { proxy: event_loop.create_proxy(), pending: AtomicBool::new(false) });
    Ok((event_loop, waker))
}

impl App {
    fn present(&mut self) {
        self.waker.pending.store(false, Ordering::Release);
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else { return };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else { return };
        let (w, h) = (size.width, size.height);
        self.placement = self.devices.display.with(|frame, cursor| {
            buffer.fill(0);
            if frame.width == 0 || frame.height == 0 {
                return Placement::default();
            }
            // Largest size that fits and keeps the aspect ratio.
            let scale = (w as f64 / frame.width as f64).min(h as f64 / frame.height as f64);
            let dw = ((frame.width as f64 * scale) as u32).clamp(1, w);
            let dh = ((frame.height as f64 * scale) as u32).clamp(1, h);
            let p = Placement {
                x: (w - dw) / 2,
                y: (h - dh) / 2,
                width: dw,
                height: dh,
                frame_width: frame.width,
                frame_height: frame.height,
            };
            let xs: Vec<usize> =
                (0..dw).map(|x| (x as u64 * frame.width as u64 / dw as u64) as usize).collect();
            for y in 0..dh {
                let sy = (y as u64 * frame.height as u64 / dh as u64) as usize;
                let src = &frame.pixels[sy * frame.width as usize..(sy + 1) * frame.width as usize];
                let row = ((p.y + y) * w + p.x) as usize;
                for (dst, &sx) in buffer[row..row + dw as usize].iter_mut().zip(&xs) {
                    *dst = src[sx];
                }
            }
            if cursor.visible && !cursor.image.is_empty() {
                draw_cursor(&mut buffer, w, &p, cursor);
            }
            p
        });
        window.pre_present_notify();
        let _ = buffer.present();
    }

    /// Guest tablet coordinates of a window position.
    fn to_tablet(&self, x: f64, y: f64) -> (i32, i32) {
        let p = &self.placement;
        if p.width == 0 {
            return (0, 0);
        }
        let fx = ((x - p.x as f64) / p.width as f64).clamp(0.0, 1.0);
        let fy = ((y - p.y as f64) / p.height as f64).clamp(0.0, 1.0);
        ((fx * ABS_MAX as f64) as i32, (fy * ABS_MAX as f64) as i32)
    }
}

fn draw_cursor(buffer: &mut [u32], stride: u32, p: &Placement, c: &rv_machine::display::Cursor) {
    let sx = p.width as f64 / p.frame_width as f64;
    let sy = p.height as f64 / p.frame_height as f64;
    let left = c.x as f64 - c.hot_x as f64;
    let top = c.y as f64 - c.hot_y as f64;
    let size = CURSOR_SIZE as f64;
    let (x0, y0) = ((left * sx).floor() as i64, (top * sy).floor() as i64);
    let (x1, y1) = (((left + size) * sx).ceil() as i64, ((top + size) * sy).ceil() as i64);
    for dy in y0.max(0)..y1.min(p.height as i64) {
        let cy = ((dy as f64 / sy - top) as i64).clamp(0, CURSOR_SIZE as i64 - 1) as u32;
        for dx in x0.max(0)..x1.min(p.width as i64) {
            let cx = ((dx as f64 / sx - left) as i64).clamp(0, CURSOR_SIZE as i64 - 1) as u32;
            let src = c.image[(cy * CURSOR_SIZE + cx) as usize];
            let a = src >> 24;
            if a == 0 {
                continue;
            }
            let i = ((p.y as i64 + dy) as u32 * stride + p.x + dx as u32) as usize;
            buffer[i] = blend(buffer[i], src, a);
        }
    }
}

fn blend(dst: u32, src: u32, a: u32) -> u32 {
    let ch = |shift: u32| {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        ((s * a + d * (255 - a)) / 255) << shift
    };
    ch(16) | ch(8) | ch(0)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let (w, h) = self.devices.display.preferred_size();
        let attrs = Window::default_attributes().with_title("rust-v").with_inner_size(LogicalSize::new(w, h));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("rust-v: cannot open a window: {e}\r");
                event_loop.exit();
                return;
            }
        };
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface"));
        self.window = Some(window);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Quit => event_loop.exit(),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.pending_resize {
            Some((at, w, h)) if at.elapsed() >= RESIZE_SETTLE => {
                self.pending_resize = None;
                self.devices.gpu.resize(w, h);
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some((at, ..)) => event_loop.set_control_flow(ControlFlow::WaitUntil(at + RESIZE_SETTLE)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.present(),
            WindowEvent::Resized(size) => {
                if let Some(w) = &self.window {
                    // Offer the guest a mode matching the window (in logical pixels, even widths).
                    let logical = size.to_logical::<f64>(w.scale_factor());
                    let (lw, lh) = ((logical.width as u32) & !7, logical.height as u32);
                    if lw >= 640 && lh >= 480 {
                        self.pending_resize = Some((Instant::now(), lw, lh));
                    }
                    w.request_redraw();
                }
            }
            WindowEvent::CursorEntered { .. } => {
                // The guest draws its own cursor.
                if let Some(w) = &self.window {
                    w.set_cursor_visible(false);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(w) = &self.window {
                    w.set_cursor_visible(true);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.repeat {
                    return; // the guest repeats keys itself
                }
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(key) = evdev_code(code)
                {
                    let value = (event.state == ElementState::Pressed) as i32;
                    self.devices.keyboard.send(&[(ev::KEY, key, value)]);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = self.to_tablet(position.x, position.y);
                self.devices.tablet.send(&[(ev::ABS, ev::ABS_X, x), (ev::ABS, ev::ABS_Y, y)]);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let code = match button {
                    MouseButton::Left => ev::BTN_LEFT,
                    MouseButton::Right => ev::BTN_RIGHT,
                    MouseButton::Middle => ev::BTN_MIDDLE,
                    MouseButton::Back => ev::BTN_SIDE,
                    MouseButton::Forward => ev::BTN_EXTRA,
                    MouseButton::Other(_) => return,
                };
                self.devices.tablet.send(&[(ev::KEY, code, (state == ElementState::Pressed) as i32)]);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
                    // Trackpads report pixels; about 50 make one wheel notch.
                    MouseScrollDelta::PixelDelta(p) => (p.x / 50.0, p.y / 50.0),
                };
                self.scroll.0 += dx;
                self.scroll.1 += dy;
                let (nx, ny) = (self.scroll.0.trunc(), self.scroll.1.trunc());
                self.scroll.0 -= nx;
                self.scroll.1 -= ny;
                let mut events = Vec::new();
                if ny != 0.0 {
                    events.push((ev::REL, ev::REL_WHEEL, ny as i32));
                }
                if nx != 0.0 {
                    events.push((ev::REL, ev::REL_HWHEEL, nx as i32));
                }
                if !events.is_empty() {
                    self.devices.tablet.send(&events);
                }
            }
            WindowEvent::Focused(false) => {
                // Release modifiers so none stays stuck down in the guest after switching away.
                for code in [
                    KeyCode::ShiftLeft,
                    KeyCode::ShiftRight,
                    KeyCode::ControlLeft,
                    KeyCode::ControlRight,
                    KeyCode::AltLeft,
                    KeyCode::AltRight,
                    KeyCode::SuperLeft,
                    KeyCode::SuperRight,
                ] {
                    if let Some(key) = evdev_code(code) {
                        self.devices.keyboard.send(&[(ev::KEY, key, 0)]);
                    }
                }
            }
            _ => {}
        }
    }
}
