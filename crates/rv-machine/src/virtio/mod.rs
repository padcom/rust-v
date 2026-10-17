//! Virtio 1.x devices behind the modern virtio-pci transport.

pub mod blk;
pub mod gpu;
pub mod input;
pub mod net;
pub mod p9;
pub mod pci;
pub mod queue;
pub mod rng;
pub mod snd;
pub mod virgl;

use std::sync::{Arc, Mutex};

use rv_core::GuestMemory;

use crate::pci::Intx;
pub use pci::VirtioPci;
pub use queue::{Chain, Queue, QueueError};

/// Device types (virtio 1.x §5).
pub mod device_type {
    pub const NET: u16 = 1;
    pub const BLOCK: u16 = 2;
    pub const CONSOLE: u16 = 3;
    pub const RNG: u16 = 4;
    pub const GPU: u16 = 16;
    pub const INPUT: u16 = 18;
    pub const SOUND: u16 = 25;
}

/// Transport-level feature bits.
pub mod feature {
    pub const RING_INDIRECT_DESC: u64 = 1 << 28;
    pub const RING_EVENT_IDX: u64 = 1 << 29;
    pub const VERSION_1: u64 = 1 << 32;
}

/// The device's interrupt: the ISR status byte and the INTx line it drives.
pub struct Interrupt {
    isr: Mutex<u8>,
    intx: Intx,
}

const ISR_QUEUE: u8 = 1;
const ISR_CONFIG: u8 = 2;

impl Interrupt {
    pub fn new(intx: Intx) -> Self {
        Interrupt { isr: Mutex::new(0), intx }
    }

    /// Signals that used buffers are available.
    pub fn queue(&self) {
        self.raise(ISR_QUEUE);
    }

    /// Signals a device configuration change.
    pub fn config(&self) {
        self.raise(ISR_CONFIG);
    }

    fn raise(&self, bits: u8) {
        let mut isr = self.isr.lock().unwrap();
        *isr |= bits;
        self.intx.set(true);
    }

    fn isr(&self) -> u8 {
        *self.isr.lock().unwrap()
    }

    /// Sets the ISR status (when restoring a snapshot) and the line to match.
    fn set_isr(&self, v: u8) {
        *self.isr.lock().unwrap() = v;
        self.intx.set(v != 0);
    }

    /// Reads and clears the ISR status, deasserting the line.
    fn take(&self) -> u8 {
        let mut isr = self.isr.lock().unwrap();
        let v = std::mem::take(&mut *isr);
        self.intx.set(false);
        v
    }
}

/// Everything a device needs once the driver has set it up.
#[derive(Clone)]
pub struct Activation {
    /// Negotiated features.
    pub features: u64,
    pub queues: Vec<Arc<Mutex<Queue>>>,
    pub mem: GuestMemory,
    pub irq: Arc<Interrupt>,
}

impl Activation {
    /// Processes every available chain of queue `index` with `f`, which returns how many bytes
    /// it wrote into the chain, then interrupts the driver if it asked for that.
    pub fn drain(&self, index: usize, mut f: impl FnMut(&Chain) -> u32) {
        let mut q = self.queues[index].lock().unwrap();
        let result = (|| -> Result<(), QueueError> {
            while let Some(chain) = q.pop(&self.mem)? {
                let written = f(&chain);
                q.push(&self.mem, chain.head, written)?;
            }
            if q.needs_interrupt(&self.mem)? {
                self.irq.queue();
            }
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("virtio: queue {index}: {e}\r");
        }
    }
}

/// A virtio device model, independent of the transport.
pub trait VirtioDevice: Send {
    fn device_type(&self) -> u16;
    /// Device-specific features offered (the transport adds its own).
    fn features(&self) -> u64;
    /// Maximum size of each queue.
    fn queue_sizes(&self) -> Vec<u16>;
    /// Reads device configuration space.
    fn read_config(&self, offset: usize, data: &mut [u8]);
    fn write_config(&mut self, _offset: usize, _data: &[u8]) {}
    /// The driver set DRIVER_OK.
    fn activate(&mut self, activation: Activation);
    /// The driver notified queue `queue`.
    fn notify(&mut self, queue: usize);
    /// The driver reset the device.
    fn reset(&mut self);
    /// PCI class code for the device.
    fn pci_class(&self) -> u32;

    /// Device state for a snapshot (beyond the queues, which the transport saves). Called
    /// with the machine paused.
    fn save_state(&self, _w: &mut rv_core::snapshot::Writer) {}

    /// Restores what `save_state` wrote, before the device is activated again.
    fn restore_state(&mut self, _r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        Ok(())
    }
}

/// Copies the part of `config` that `offset..offset + data.len()` covers into `data`.
pub fn read_config_bytes(config: &[u8], offset: usize, data: &mut [u8]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b = config.get(offset + i).copied().unwrap_or(0);
    }
}
