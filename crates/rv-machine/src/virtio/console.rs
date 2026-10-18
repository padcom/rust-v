//! virtio-console (§5.3) with multiple ports, carrying up to two: `com.redhat.spice.0`, on
//! which the guest's spice-vdagentd finds the host's clipboard agent ([`super::vdagent`]),
//! and `org.qemu.guest_agent.0`, on which the guest's qemu-ga takes commands from the host
//! (a plain byte stream, [`GuestAgentHandle`]).

use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::vdagent::{Agent, Event};
use super::{Activation, VirtioDevice, device_type};

const F_MULTIPORT: u64 = 1 << 1;
/// Port 0 is left unused: Linux and QEMU keep it for a console.
const PORTS: u32 = 3;
const SPICE_PORT: u32 = 1;
const GUEST_AGENT_PORT: u32 = 2;

/// Queues: receive/transmit for port 0, the control pair, then a receive/transmit pair for
/// each further port.
const TX0: usize = 1;
const CONTROL_RX: usize = 2;
const CONTROL_TX: usize = 3;

fn rx_queue(port: u32) -> usize {
    2 * (port as usize + 1)
}

mod event {
    pub const DEVICE_READY: u16 = 0;
    pub const DEVICE_ADD: u16 = 1;
    pub const PORT_READY: u16 = 3;
    pub const PORT_OPEN: u16 = 6;
    pub const PORT_NAME: u16 = 7;
}

/// What the host end of a port is.
enum Peer {
    Spice {
        agent: Agent,
        guest_text: Sender<String>,
    },
    /// Bytes from the guest go to the sender as they arrive.
    Stream(Sender<Vec<u8>>),
}

struct Port {
    id: u32,
    name: &'static [u8],
    /// The guest has the port open.
    open: bool,
    /// Bytes for the guest.
    rx: VecDeque<u8>,
    peer: Peer,
}

impl Port {
    fn opened(&mut self, open: bool) {
        self.open = open;
        self.rx.clear();
        if let (true, Peer::Spice { agent, .. }) = (open, &mut self.peer) {
            let mut out = Vec::new();
            agent.opened(&mut out);
            self.rx.extend(out);
        }
    }

    fn received(&mut self, data: Vec<u8>) {
        match &mut self.peer {
            Peer::Spice { agent, guest_text } => {
                let mut out = Vec::new();
                for e in agent.received(&data, &mut out) {
                    match e {
                        Event::GuestText(text) => {
                            let _ = guest_text.send(text);
                        }
                    }
                }
                self.rx.extend(out);
            }
            Peer::Stream(output) => {
                let _ = output.send(data);
            }
        }
    }
}

struct Shared {
    active: Option<Activation>,
    /// Control messages for the guest.
    control: VecDeque<Vec<u8>>,
    ports: Vec<Port>,
}

fn control(id: u32, event: u16, value: u16) -> Vec<u8> {
    let mut m = Vec::with_capacity(8);
    m.extend_from_slice(&id.to_le_bytes());
    m.extend_from_slice(&event.to_le_bytes());
    m.extend_from_slice(&value.to_le_bytes());
    m
}

impl Shared {
    fn port(&mut self, id: u32) -> Option<&mut Port> {
        self.ports.iter_mut().find(|p| p.id == id)
    }

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
        for port in self.ports.iter_mut().filter(|p| p.open) {
            let rx = std::cell::RefCell::new(&mut port.rx);
            fill(rx_queue(port.id), &|| !rx.borrow().is_empty(), &mut |room| {
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
                for port in &self.ports {
                    self.control.push_back(control(port.id, event::DEVICE_ADD, 0));
                }
            }
            event::PORT_READY if value == 1 => {
                let Some(port) = self.ports.iter().find(|p| p.id == id) else { return };
                let mut name = control(id, event::PORT_NAME, 1);
                name.extend_from_slice(port.name);
                self.control.push_back(name);
                // The host end is always connected.
                self.control.push_back(control(id, event::PORT_OPEN, 1));
            }
            event::PORT_OPEN => {
                if let Some(port) = self.port(id) {
                    port.opened(value == 1);
                }
            }
            _ => {}
        }
    }
}

/// Shares the clipboard with the guest from any thread.
#[derive(Clone)]
pub struct ClipboardHandle(Arc<Mutex<Shared>>);

impl ClipboardHandle {
    /// The host clipboard now holds `text`.
    pub fn set_host_text(&self, text: String) {
        let mut s = self.0.lock().unwrap();
        let Some(port) = s.port(SPICE_PORT) else { return };
        let mut out = Vec::new();
        if let Peer::Spice { agent, .. } = &mut port.peer {
            agent.host_text(text, &mut out);
        }
        if port.open {
            port.rx.extend(out);
            s.deliver();
        }
    }
}

/// The host end of the guest agent's port: bytes from the guest arrive on the receiver
/// returned along with it.
#[derive(Clone)]
pub struct GuestAgentHandle(Arc<Mutex<Shared>>);

impl GuestAgentHandle {
    /// Whether the guest agent has the port open (is running).
    pub fn is_open(&self) -> bool {
        self.0.lock().unwrap().port(GUEST_AGENT_PORT).is_some_and(|p| p.open)
    }

    /// Sends `data` to the guest; dropped unless the port is open.
    pub fn send(&self, data: &[u8]) {
        let mut s = self.0.lock().unwrap();
        let Some(port) = s.port(GUEST_AGENT_PORT) else { return };
        if port.open {
            port.rx.extend(data);
            s.deliver();
        }
    }
}

pub struct Console {
    shared: Arc<Mutex<Shared>>,
}

/// The ports a console carries, and how to reach their host ends.
pub struct Ports {
    pub clipboard: Option<ClipboardHandle>,
    pub guest_agent: Option<(GuestAgentHandle, std::sync::mpsc::Receiver<Vec<u8>>)>,
}

impl Console {
    /// A console with the SPICE agent's port if `guest_text` is given (the agent sends the
    /// guest's clipboard text there) and the guest agent's port if `guest_agent`.
    pub fn new(guest_text: Option<Sender<String>>, guest_agent: bool) -> (Self, Ports) {
        let mut ports = Vec::new();
        if let Some(guest_text) = guest_text {
            ports.push(Port {
                id: SPICE_PORT,
                name: b"com.redhat.spice.0",
                open: false,
                rx: VecDeque::new(),
                peer: Peer::Spice { agent: Agent::new(), guest_text },
            });
        }
        let mut agent_output = None;
        if guest_agent {
            let (tx, rx) = std::sync::mpsc::channel();
            agent_output = Some(rx);
            ports.push(Port {
                id: GUEST_AGENT_PORT,
                name: b"org.qemu.guest_agent.0",
                open: false,
                rx: VecDeque::new(),
                peer: Peer::Stream(tx),
            });
        }
        let has_spice = ports.iter().any(|p| p.id == SPICE_PORT);
        let shared = Arc::new(Mutex::new(Shared { active: None, control: VecDeque::new(), ports }));
        let handles = Ports {
            clipboard: has_spice.then(|| ClipboardHandle(shared.clone())),
            guest_agent: agent_output.map(|rx| (GuestAgentHandle(shared.clone()), rx)),
        };
        (Console { shared }, handles)
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
            q if q > CONTROL_TX && q % 2 == 1 => {
                let id = (q / 2 - 1) as u32;
                if s.port(id).is_some() {
                    let data = take_all(&a, q);
                    let port = s.port(id).unwrap();
                    for m in data {
                        port.received(m);
                    }
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
        for port in &mut s.ports {
            port.open = false;
            port.rx.clear();
        }
    }

    fn pci_class(&self) -> u32 {
        0x07_80_00
    }

    fn save_state(&self, w: &mut rv_core::snapshot::Writer) {
        let s = self.shared.lock().unwrap();
        for port in &s.ports {
            w.bool(port.open);
        }
    }

    fn restore_state(&mut self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        let mut s = self.shared.lock().unwrap();
        for port in &mut s.ports {
            // The program in the guest carries on; start the host side over with it.
            let open = r.bool()?;
            port.opened(open);
        }
        Ok(())
    }
}
