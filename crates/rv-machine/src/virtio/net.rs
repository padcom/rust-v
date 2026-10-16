//! virtio-net (§5.1) with one receive and one transmit queue. Frames go to and come from a
//! [`Backend`], normally the user-mode network stack.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::{Activation, VirtioDevice, device_type, read_config_bytes};

const RX: usize = 0;
const TX: usize = 1;
const QUEUE_SIZE: u16 = 256;
/// `struct virtio_net_hdr` with `num_buffers` (always present with VIRTIO_F_VERSION_1).
const HEADER: usize = 12;
/// Frames held for the guest while it has no receive buffers; older ones are dropped.
const MAX_PENDING: usize = 4096;

mod feature {
    pub const MAC: u64 = 1 << 5;
    pub const STATUS: u64 = 1 << 16;
}

const STATUS_LINK_UP: u16 = 1;

/// Where transmitted frames go.
pub trait Backend: Send {
    fn send(&mut self, frame: Vec<u8>);
}

impl Backend for rv_net::UserNet {
    fn send(&mut self, frame: Vec<u8>) {
        rv_net::UserNet::send(self, frame)
    }
}

/// Receive side shared with the backend's thread.
#[derive(Default)]
struct Rx {
    pending: VecDeque<Vec<u8>>,
    active: Option<Activation>,
}

impl Rx {
    /// Moves pending frames into the guest's receive buffers while there are any.
    fn deliver(&mut self) {
        let Some(a) = &self.active else { return };
        if self.pending.is_empty() {
            return;
        }
        let mut q = a.queues[RX].lock().unwrap();
        let mut delivered = false;
        while let Some(frame) = self.pending.front() {
            let chain = match q.pop(&a.mem) {
                Ok(Some(chain)) => chain,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("virtio-net: receive queue: {e}\r");
                    return;
                }
            };
            let mut writer = chain.writer(&a.mem);
            let mut header = [0u8; HEADER];
            header[10] = 1; // num_buffers
            let written = if chain.writable_len() >= (HEADER + frame.len()) as u64 {
                writer.write(&header).and_then(|_| writer.write(frame)).map_or(0, |n| HEADER + n)
            } else {
                0 // too small: the frame is dropped
            };
            self.pending.pop_front();
            if q.push(&a.mem, chain.head, written as u32).is_err() {
                return;
            }
            delivered = true;
        }
        if delivered && q.needs_interrupt(&a.mem).unwrap_or(true) {
            a.irq.queue();
        }
    }
}

/// Feeds frames to a virtio-net device from any thread.
#[derive(Clone)]
pub struct RxHandle(Arc<Mutex<Rx>>);

impl RxHandle {
    pub fn receive(&self, frame: Vec<u8>) {
        let mut rx = self.0.lock().unwrap();
        if rx.active.is_none() {
            return; // link down from the guest's point of view
        }
        if rx.pending.len() >= MAX_PENDING {
            rx.pending.pop_front();
        }
        rx.pending.push_back(frame);
        rx.deliver();
    }
}

pub struct Net {
    mac: [u8; 6],
    backend: Box<dyn Backend>,
    rx: Arc<Mutex<Rx>>,
    active: Option<Activation>,
}

impl Net {
    /// A device and the handle through which its backend delivers received frames. The
    /// backend is attached afterwards with [`Net::set_backend`] when it needs the handle.
    pub fn new(mac: [u8; 6]) -> (Self, RxHandle) {
        let rx = Arc::new(Mutex::new(Rx::default()));
        let net = Net { mac, backend: Box::new(Discard), rx: rx.clone(), active: None };
        (net, RxHandle(rx))
    }

    pub fn set_backend(&mut self, backend: Box<dyn Backend>) {
        self.backend = backend;
    }

    fn transmit(&mut self) {
        let Some(a) = &self.active else { return };
        let backend = &mut self.backend;
        a.drain(TX, |chain| {
            let len = chain.readable_len() as usize;
            if len > HEADER && len <= HEADER + 65536 {
                let mut frame = vec![0u8; len];
                if chain.reader(&a.mem).read(&mut frame).is_ok() {
                    frame.drain(..HEADER);
                    backend.send(frame);
                }
            }
            0
        });
    }
}

struct Discard;

impl Backend for Discard {
    fn send(&mut self, _frame: Vec<u8>) {}
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u16 {
        device_type::NET
    }

    fn features(&self) -> u64 {
        feature::MAC | feature::STATUS
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE, QUEUE_SIZE]
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = [0u8; 10];
        config[0..6].copy_from_slice(&self.mac);
        config[6..8].copy_from_slice(&STATUS_LINK_UP.to_le_bytes());
        config[8..10].copy_from_slice(&1u16.to_le_bytes()); // max_virtqueue_pairs
        read_config_bytes(&config, offset, data);
    }

    fn activate(&mut self, activation: Activation) {
        self.active = Some(activation.clone());
        let mut rx = self.rx.lock().unwrap();
        rx.active = Some(activation);
        rx.deliver();
    }

    fn notify(&mut self, queue: usize) {
        match queue {
            RX => self.rx.lock().unwrap().deliver(),
            TX => self.transmit(),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.active = None;
        let mut rx = self.rx.lock().unwrap();
        rx.active = None;
        rx.pending.clear();
    }

    fn pci_class(&self) -> u32 {
        0x02_00_00
    }
}
