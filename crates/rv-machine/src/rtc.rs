//! Goldfish RTC: wall-clock time in nanoseconds. Alarms are not implemented.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SIZE: u64 = 0x1000;

const TIME_LOW: u64 = 0x00;
const TIME_HIGH: u64 = 0x04;

#[derive(Default)]
struct State {
    /// Guest-set offset from host time, in nanoseconds.
    offset: i64,
    /// TIME_HIGH value latched by the last TIME_LOW read.
    latched_high: u32,
    /// TIME_HIGH value written, waiting for the TIME_LOW write that commits it (Linux writes
    /// the high half first).
    pending_high: u32,
}

#[derive(Default)]
pub struct GoldfishRtc {
    state: Mutex<State>,
}

impl GoldfishRtc {
    pub fn save(&self, w: &mut rv_core::snapshot::Writer) {
        w.u64(self.state.lock().unwrap().offset as u64);
    }

    pub fn restore(&self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        self.state.lock().unwrap().offset = r.u64()? as i64;
        Ok(())
    }
}

fn host_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

impl rv_core::Device for GoldfishRtc {
    fn read(&self, offset: u64, _size: u8) -> u64 {
        let mut s = self.state.lock().unwrap();
        match offset {
            TIME_LOW => {
                let now = (host_ns() + s.offset) as u64;
                s.latched_high = (now >> 32) as u32;
                now as u32 as u64
            }
            TIME_HIGH => s.latched_high as u64,
            _ => 0,
        }
    }

    fn write(&self, offset: u64, _size: u8, value: u64) {
        let mut s = self.state.lock().unwrap();
        match offset {
            TIME_HIGH => s.pending_high = value as u32,
            TIME_LOW => {
                let ns = (((s.pending_high as u64) << 32) | value as u32 as u64) as i64;
                s.offset = ns - host_ns();
            }
            _ => {}
        }
    }
}
