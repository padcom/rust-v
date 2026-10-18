//! virtio-console (§5.3) with multiple ports, carrying one port: `com.redhat.spice.0`, on
//! which the guest's spice-vdagentd finds the host's clipboard agent ([`super::vdagent`]).

use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::vdagent::{Agent, Event};
use super::{Activation, VirtioDevice, device_type};

const F_MULTIPORT: u64 = 1 << 1;
/// Port 0 is left unused: Linux and QEMU keep it for a console.
const PORTS: u32 = 2;
const AGENT_PORT: u32 = 1;
const PORT_NAME: &[u8] = b"com.redhat.spice.0";

/// Queues: receive/transmit for port 0, the control pair, then receive/transmit for port 1.
const TX0: usize = 1;
const CONTROL_RX: usize = 2;
const CONTROL_TX: usize = 3;
const AGENT_RX: usize = 4;
const AGENT_TX: usize = 5;

mod event {
    pub const DEVICE_READY: u16 = 0;
    pub const DEVICE_ADD: u16 = 1;
    pub const PORT_READY: u16 = 3;
    pub const PORT_OPEN: u16 = 6;
    pub const PORT_NAME: u16 = 7;
}

struct Shared {
    active: Option<Activation>,
    /// Control messages for the guest.
    control: VecDeque<Vec<u8>>,
    /// Bytes for the guest on the agent port.
    rx: VecDeque<u8>,
    /// The guest has the port open.
    open: bool,
    agent: Agent,
    guest_text: Sender<String>,
}

fn control(id: u32, event: u16, value: u16) -> Vec<u8> {
    let mut m = Vec::with_capacity(8);
    m.extend_from_slice(&id.to_le_bytes());
    m.extend_from_slice(&event.to_le_bytes());
    m.extend_from_slice(&value.to_le_bytes());
    m
}

impl Shared {
    /// Writes what is pending into the guest's receive buffers.
    fn deliver(&mut self) {
        let Some(a) = &self.active else { return };
        if a.mem.frozen() {
            return;
        }
        // `pending` says whether there is more; `next` takes up to the given number of bytes.
        let fill = |index: usize, pending: &dyn Fn() -> bool, next: &mut dyn FnMut(usize) -> Vec<u8>| {
            let mut q = a.queues[index].lock().unwrap();
            let mut delivered = false;
            while pending() {
                let Ok(Some(chain)) = q.pop(&a.mem) else { break };
                let data = next(chain.writable_len() as usize);
                let n = chain.writer(&a.mem).write(&data).unwrap_or(0);
                if q.push(&a.mem, chain.head, n as u32).is_err() {
                    return;
                }
                delivered = true;
            }
            if delivered && q.needs_interrupt(&a.mem).unwrap_or(true) {
                a.irq.queue();
            }
        };
        let control = std::cell::RefCell::new(&mut self.control);
        fill(CONTROL_RX, &|| !control.borrow().is_empty(), &mut |_| {
            control.borrow_mut().pop_front().unwrap_or_default()
        });
        if self.open {
            let rx = std::cell::RefCell::new(&mut self.rx);
            fill(AGENT_RX, &|| !rx.borrow().is_empty(), &mut |room| {
                let mut rx = rx.borrow_mut();
                let n = room.min(rx.len());
                rx.drain(..n).collect()
            });
        }
    }

    fn control_message(&mut self, m: &[u8]) {
        if m.len() < 8 {
            return;
        }
        let id = u32::from_le_bytes(m[0..4].try_into().unwrap());
        let ev = u16::from_le_bytes([m[4], m[5]]);
        let value = u16::from_le_bytes([m[6], m[7]]);
        match ev {
            event::DEVICE_READY if value == 1 => {
                self.control.push_back(control(AGENT_PORT, event::DEVICE_ADD, 0));
            }
            event::PORT_READY if id == AGENT_PORT && value == 1 => {
                let mut name = control(AGENT_PORT, event::PORT_NAME, 1);
                name.extend_from_slice(PORT_NAME);
                self.control.push_back(name);
                // The host end is always connected.
                self.control.push_back(control(AGENT_PORT, event::PORT_OPEN, 1));
            }
            event::PORT_OPEN if id == AGENT_PORT => {
                self.open = value == 1;
                self.rx.clear();
                if self.open {
                    let mut out = Vec::new();
                    self.agent.opened(&mut out);
                    self.rx.extend(out);
                }
            }
            _ => {}
        }
    }

    fn agent_data(&mut self, data: &[u8]) {
        let mut out = Vec::new();
        for e in self.agent.received(data, &mut out) {
            match e {
                Event::GuestText(text) => {
                    let _ = self.guest_text.send(text);
                }
            }
        }
        self.rx.extend(out);
    }
}

/// Shares the clipboard with the guest from any thread.
#[derive(Clone)]
pub struct ClipboardHandle(Arc<Mutex<Shared>>);

impl ClipboardHandle {
    /// The host clipboard now holds `text`.
    pub fn set_host_text(&self, text: String) {
        let mut s = self.0.lock().unwrap();
        let mut out = Vec::new();
        s.agent.host_text(text, &mut out);
        if s.open {
            s.rx.extend(out);
            s.deliver();
        }
    }
}

pub struct Console {
    shared: Arc<Mutex<Shared>>,
}

impl Console {
    /// A console whose agent sends the guest's clipboard text to `guest_text`.
    pub fn new(guest_text: Sender<String>) -> (Self, ClipboardHandle) {
        let shared = Arc::new(Mutex::new(Shared {
            active: None,
            control: VecDeque::new(),
            rx: VecDeque::new(),
            open: false,
            agent: Agent::new(),
            guest_text,
        }));
        (Console { shared: shared.clone() }, ClipboardHandle(shared))
    }
}

/// Reads every available chain of queue `index` and completes it.
fn take_all(a: &Activation, index: usize) -> Vec<Vec<u8>> {
    let mut messages = Vec::new();
    a.drain(index, |chain| {
        let len = chain.readable_len().min(1 << 20) as usize;
        let mut buf = vec![0u8; len];
        let n = chain.reader(&a.mem).read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        messages.push(buf);
        0
    });
    messages
}

impl VirtioDevice for Console {
    fn device_type(&self) -> u16 {
        device_type::CONSOLE
    }

    fn features(&self) -> u64 {
        F_MULTIPORT
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![64; 2 * (PORTS as usize + 1)]
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        // cols, rows, max_nr_ports, emerg_wr
        let mut config = [0u8; 12];
        config[4..8].copy_from_slice(&PORTS.to_le_bytes());
        super::read_config_bytes(&config, offset, data);
    }

    fn activate(&mut self, activation: Activation) {
        self.shared.lock().unwrap().active = Some(activation);
    }

    fn notify(&mut self, queue: usize) {
        let mut s = self.shared.lock().unwrap();
        let Some(a) = s.active.clone() else { return };
        match queue {
            TX0 => drop(take_all(&a, TX0)),
            CONTROL_TX => {
                for m in take_all(&a, CONTROL_TX) {
                    s.control_message(&m);
                }
            }
            AGENT_TX => {
                for m in take_all(&a, AGENT_TX) {
                    s.agent_data(&m);
                }
            }
            _ => {}
        }
        s.deliver();
    }

    fn reset(&mut self) {
        let mut s = self.shared.lock().unwrap();
        s.active = None;
        s.control.clear();
        s.rx.clear();
        s.open = false;
    }

    fn pci_class(&self) -> u32 {
        0x07_80_00
    }

    fn save_state(&self, w: &mut rv_core::snapshot::Writer) {
        let s = self.shared.lock().unwrap();
        w.bool(s.open);
    }

    fn restore_state(&mut self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        let mut s = self.shared.lock().unwrap();
        s.open = r.bool()?;
        if s.open {
            // The agent in the guest carries on; start the host side over with it.
            let mut out = Vec::new();
            s.agent.opened(&mut out);
            s.rx.extend(out);
        }
        Ok(())
    }
}
