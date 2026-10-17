//! What the guest shows on its screen, shared between the GPU device (on hart threads) and
//! whichever front end presents it.

use std::sync::{Arc, Mutex};

/// Pixels are `0x00RRGGBB` (the in-memory layout of XRGB8888 on a little-endian host).
#[derive(Clone, Default)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
    /// Bumped on every change, so a front end can skip redundant redraws.
    pub serial: u64,
}

#[derive(Clone, Default)]
pub struct Cursor {
    /// 64x64 ARGB (`0xAARRGGBB`) image; empty if the guest set none.
    pub image: Vec<u32>,
    pub hot_x: u32,
    pub hot_y: u32,
    /// Position of the hot spot in frame coordinates.
    pub x: u32,
    pub y: u32,
    pub visible: bool,
}

pub const CURSOR_SIZE: u32 = 64;

type Waker = Box<dyn Fn() + Send + Sync>;

pub struct Display {
    frame: Mutex<Frame>,
    cursor: Mutex<Cursor>,
    /// Display size the guest is told about.
    preferred: Mutex<(u32, u32)>,
    waker: Mutex<Option<Waker>>,
}

impl Display {
    pub fn new(width: u32, height: u32) -> Arc<Self> {
        Arc::new(Display {
            frame: Mutex::new(Frame::default()),
            cursor: Mutex::new(Cursor::default()),
            preferred: Mutex::new((width, height)),
            waker: Mutex::new(None),
        })
    }

    /// Registers the front end's "something changed" callback.
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.waker.lock().unwrap() = Some(Box::new(waker));
    }

    fn wake(&self) {
        if let Some(w) = &*self.waker.lock().unwrap() {
            w();
        }
    }

    pub fn preferred_size(&self) -> (u32, u32) {
        *self.preferred.lock().unwrap()
    }

    pub fn set_preferred_size(&self, width: u32, height: u32) {
        *self.preferred.lock().unwrap() = (width, height);
    }

    /// Runs `f` on the frame and notifies the front end.
    pub fn update(&self, f: impl FnOnce(&mut Frame)) {
        {
            let mut frame = self.frame.lock().unwrap();
            f(&mut frame);
            frame.serial += 1;
        }
        self.wake();
    }

    pub fn update_cursor(&self, f: impl FnOnce(&mut Cursor)) {
        f(&mut self.cursor.lock().unwrap());
        self.wake();
    }

    /// Runs `f` with the current frame and cursor (for presenting them).
    pub fn with<R>(&self, f: impl FnOnce(&Frame, &Cursor) -> R) -> R {
        let frame = self.frame.lock().unwrap();
        let cursor = self.cursor.lock().unwrap();
        f(&frame, &cursor)
    }
}
