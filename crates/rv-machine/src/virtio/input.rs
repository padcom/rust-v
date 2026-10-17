//! virtio-input (§5.8): a keyboard and an absolute pointer ("tablet"), fed evdev events by the
//! front end.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::{Activation, VirtioDevice, device_type};

const EVENTQ: usize = 0;
const STATUSQ: usize = 1;
const EVENT_SIZE: usize = 8;
/// Events held while the guest has no buffers; older ones are dropped beyond this.
const MAX_PENDING: usize = 1024;

/// Linux input event types and codes used here.
pub mod ev {
    pub const SYN: u16 = 0x00;
    pub const KEY: u16 = 0x01;
    pub const REL: u16 = 0x02;
    pub const ABS: u16 = 0x03;
    pub const SYN_REPORT: u16 = 0;
    pub const REL_HWHEEL: u16 = 0x06;
    pub const REL_WHEEL: u16 = 0x08;
    pub const ABS_X: u16 = 0x00;
    pub const ABS_Y: u16 = 0x01;
    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;
    /// Largest key code a keyboard reports (KEY_MICMUTE).
    pub const KEY_MAX_KEYBOARD: u16 = 248;
}

/// Range of the tablet's absolute axes.
pub const ABS_MAX: u32 = 32767;

mod select {
    pub const ID_NAME: u8 = 0x01;
    pub const ID_SERIAL: u8 = 0x02;
    pub const ID_DEVIDS: u8 = 0x03;
    pub const PROP_BITS: u8 = 0x10;
    pub const EV_BITS: u8 = 0x11;
    pub const ABS_INFO: u8 = 0x12;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Keyboard,
    Tablet,
}

#[derive(Default)]
struct Shared {
    pending: VecDeque<[u8; EVENT_SIZE]>,
    active: Option<Activation>,
}

impl Shared {
    fn deliver(&mut self) {
        let Some(a) = &self.active else { return };
        let mut q = a.queues[EVENTQ].lock().unwrap();
        let mut delivered = false;
        while let Some(event) = self.pending.front() {
            let chain = match q.pop(&a.mem) {
                Ok(Some(chain)) => chain,
                _ => break,
            };
            let n = chain.writer(&a.mem).write(event).unwrap_or(0);
            self.pending.pop_front();
            if q.push(&a.mem, chain.head, n as u32).is_err() {
                return;
            }
            delivered = true;
        }
        if delivered && q.needs_interrupt(&a.mem).unwrap_or(true) {
            a.irq.queue();
        }
    }
}

/// Sends events to an input device from any thread.
#[derive(Clone)]
pub struct InputHandle(Arc<Mutex<Shared>>);

impl InputHandle {
    /// Queues `events` (type, code, value) followed by a SYN_REPORT.
    pub fn send(&self, events: &[(u16, u16, i32)]) {
        let mut s = self.0.lock().unwrap();
        if s.active.is_none() {
            return;
        }
        for &(kind, code, value) in events.iter().chain(&[(ev::SYN, ev::SYN_REPORT, 0)]) {
            if s.pending.len() >= MAX_PENDING {
                s.pending.pop_front();
            }
            let mut e = [0u8; EVENT_SIZE];
            e[0..2].copy_from_slice(&kind.to_le_bytes());
            e[2..4].copy_from_slice(&code.to_le_bytes());
            e[4..8].copy_from_slice(&value.to_le_bytes());
            s.pending.push_back(e);
        }
        if !s.active.as_ref().is_some_and(|a| a.mem.frozen()) {
            s.deliver();
        }
    }
}

pub struct Input {
    kind: Kind,
    select: u8,
    subsel: u8,
    shared: Arc<Mutex<Shared>>,
}

impl Input {
    pub fn new(kind: Kind) -> (Self, InputHandle) {
        let shared = Arc::new(Mutex::new(Shared::default()));
        (Input { kind, select: 0, subsel: 0, shared: shared.clone() }, InputHandle(shared))
    }

    /// The payload for the current select/subsel query.
    fn payload(&self) -> Vec<u8> {
        let bitmap = |codes: &mut dyn Iterator<Item = u16>| {
            let mut b = vec![0u8; 128];
            let mut len = 0;
            for c in codes {
                b[c as usize / 8] |= 1 << (c % 8);
                len = len.max(c as usize / 8 + 1);
            }
            b.truncate(len);
            b
        };
        match (self.select, self.kind) {
            (select::ID_NAME, Kind::Keyboard) => b"rust-v keyboard".to_vec(),
            (select::ID_NAME, Kind::Tablet) => b"rust-v tablet".to_vec(),
            (select::ID_SERIAL, _) => b"0".to_vec(),
            (select::ID_DEVIDS, _) => {
                // BUS_VIRTUAL, vendor, product, version.
                let product: u16 = if self.kind == Kind::Keyboard { 1 } else { 2 };
                [0x06u16, 0x1af4, product, 1].iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            (select::PROP_BITS, _) => Vec::new(),
            (select::EV_BITS, Kind::Keyboard) => match self.subsel as u16 {
                ev::KEY => bitmap(&mut (1..=ev::KEY_MAX_KEYBOARD)),
                _ => Vec::new(),
            },
            (select::EV_BITS, Kind::Tablet) => match self.subsel as u16 {
                ev::KEY => bitmap(
                    &mut [ev::BTN_LEFT, ev::BTN_RIGHT, ev::BTN_MIDDLE, ev::BTN_SIDE, ev::BTN_EXTRA]
                        .into_iter(),
                ),
                ev::REL => bitmap(&mut [ev::REL_HWHEEL, ev::REL_WHEEL].into_iter()),
                ev::ABS => bitmap(&mut [ev::ABS_X, ev::ABS_Y].into_iter()),
                _ => Vec::new(),
            },
            (select::ABS_INFO, Kind::Tablet) if self.subsel as u16 <= ev::ABS_Y => {
                // min, max, fuzz, flat, resolution
                [0, ABS_MAX, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            _ => Vec::new(),
        }
    }
}

impl VirtioDevice for Input {
    fn device_type(&self) -> u16 {
        device_type::INPUT
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![64, 64]
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let payload = self.payload();
        let mut config = [0u8; 8 + 128];
        config[0] = self.select;
        config[1] = self.subsel;
        config[2] = payload.len() as u8;
        config[8..8 + payload.len()].copy_from_slice(&payload);
        super::read_config_bytes(&config, offset, data);
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            match offset + i {
                0 => self.select = b,
                1 => self.subsel = b,
                _ => {}
            }
        }
    }

    fn activate(&mut self, activation: Activation) {
        let mut s = self.shared.lock().unwrap();
        s.active = Some(activation);
        s.deliver();
    }

    fn notify(&mut self, queue: usize) {
        let mut s = self.shared.lock().unwrap();
        match queue {
            EVENTQ => s.deliver(),
            STATUSQ => {
                // LED updates and the like: accepted and ignored.
                if let Some(a) = s.active.clone() {
                    drop(s);
                    a.drain(STATUSQ, |_| 0);
                }
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        let mut s = self.shared.lock().unwrap();
        s.active = None;
        s.pending.clear();
        self.select = 0;
        self.subsel = 0;
    }

    fn pci_class(&self) -> u32 {
        0x09_80_00
    }

    fn save_state(&self, w: &mut rv_core::snapshot::Writer) {
        w.u8(self.select);
        w.u8(self.subsel);
    }

    fn restore_state(&mut self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        self.select = r.u8()?;
        self.subsel = r.u8()?;
        Ok(())
    }
}
