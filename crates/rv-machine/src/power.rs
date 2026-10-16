//! SiFive "test" device (as on QEMU's virt board): lets the guest power off or reset the machine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rv_core::HartShared;

pub const SIZE: u64 = 0x1000;

const FINISHER_FAIL: u64 = 0x3333;
const FINISHER_PASS: u64 = 0x5555;
const FINISHER_RESET: u64 = 0x7777;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shutdown {
    PowerOff,
    Reset,
    /// Powered off reporting failure with the given code.
    Fail(u32),
}

/// Shared "the machine should stop" state.
pub struct PowerState {
    requested: AtomicBool,
    reason: Mutex<Option<Shutdown>>,
    harts: Vec<Arc<HartShared>>,
}

impl PowerState {
    pub fn new(harts: Vec<Arc<HartShared>>) -> Self {
        PowerState { requested: AtomicBool::new(false), reason: Mutex::new(None), harts }
    }

    pub fn request(&self, reason: Shutdown) {
        self.reason.lock().unwrap().get_or_insert(reason);
        self.requested.store(true, Ordering::SeqCst);
        for h in &self.harts {
            h.kick();
        }
    }

    #[inline]
    pub fn reason(&self) -> Option<Shutdown> {
        if !self.requested.load(Ordering::Relaxed) {
            return None;
        }
        *self.reason.lock().unwrap()
    }
}

pub struct TestDevice {
    power: Arc<PowerState>,
}

impl TestDevice {
    pub fn new(power: Arc<PowerState>) -> Self {
        TestDevice { power }
    }
}

impl rv_core::Device for TestDevice {
    fn read(&self, _offset: u64, _size: u8) -> u64 {
        0
    }

    fn write(&self, offset: u64, _size: u8, value: u64) {
        if offset != 0 {
            return;
        }
        match value & 0xffff {
            FINISHER_PASS => self.power.request(Shutdown::PowerOff),
            FINISHER_RESET => self.power.request(Shutdown::Reset),
            FINISHER_FAIL => self.power.request(Shutdown::Fail((value >> 16) as u32 & 0xffff)),
            _ => {}
        }
    }
}
