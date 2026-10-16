//! The modern (virtio 1.x) PCI transport (§4.1). All structures live in one 64-bit BAR, laid
//! out like QEMU's: common configuration, ISR, device configuration, then notifications.

use std::sync::{Arc, Mutex};

use rv_core::GuestMemory;

use super::{Activation, Interrupt, Queue, VirtioDevice, feature};
use crate::pci::{Bar, Intx, PciDevice, PciHeader};

const BAR: usize = 4;
const BAR_SIZE: u64 = 0x4000;
const COMMON: u64 = 0x0000;
const ISR: u64 = 0x1000;
const DEVICE: u64 = 0x2000;
const NOTIFY: u64 = 0x3000;
const REGION: u64 = 0x1000;
const NOTIFY_MULTIPLIER: u32 = 4;

const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_ISR: u8 = 3;
const CFG_DEVICE: u8 = 4;

mod status {
    pub const FEATURES_OK: u8 = 8;
    pub const DRIVER_OK: u8 = 4;
}

const NO_VECTOR: u64 = 0xffff;

/// Features every device gets from the transport.
const TRANSPORT_FEATURES: u64 = feature::VERSION_1 | feature::RING_INDIRECT_DESC | feature::RING_EVENT_IDX;

struct Transport {
    device: Box<dyn VirtioDevice>,
    queues: Vec<Arc<Mutex<Queue>>>,
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: u64,
    status: u8,
    queue_select: u16,
    config_generation: u8,
}

pub struct VirtioPci {
    transport: Mutex<Transport>,
    irq: Arc<Interrupt>,
    mem: GuestMemory,
    device_type: u16,
    class: u32,
}

impl VirtioPci {
    pub fn new(device: Box<dyn VirtioDevice>, mem: GuestMemory, intx: Intx) -> Self {
        let queues = device.queue_sizes().into_iter().map(|n| Arc::new(Mutex::new(Queue::new(n)))).collect();
        let (device_type, class) = (device.device_type(), device.pci_class());
        VirtioPci {
            transport: Mutex::new(Transport {
                device,
                queues,
                device_feature_select: 0,
                driver_feature_select: 0,
                driver_features: 0,
                status: 0,
                queue_select: 0,
                config_generation: 0,
            }),
            irq: Arc::new(Interrupt::new(intx)),
            mem,
            device_type,
            class,
        }
    }
}

impl Transport {
    fn offered(&self) -> u64 {
        self.device.features() | TRANSPORT_FEATURES
    }

    fn queue(&self) -> Option<&Arc<Mutex<Queue>>> {
        self.queues.get(self.queue_select as usize)
    }

    fn reset(&mut self, irq: &Interrupt) {
        self.device.reset();
        for q in &self.queues {
            q.lock().unwrap().reset();
        }
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.driver_features = 0;
        self.status = 0;
        self.queue_select = 0;
        irq.take();
    }

    fn read_common(&self, offset: u64, size: u8) -> u64 {
        let q = self.queue().map(|q| q.lock().unwrap());
        let half = |v: u64, at: u64| if offset == at + 4 { v >> 32 } else { v };
        let value = match offset {
            0x00 => self.device_feature_select as u64,
            0x04 => match self.device_feature_select {
                0 => self.offered() & 0xffff_ffff,
                1 => self.offered() >> 32,
                _ => 0,
            },
            0x08 => self.driver_feature_select as u64,
            0x0c => match self.driver_feature_select {
                0 => self.driver_features & 0xffff_ffff,
                1 => self.driver_features >> 32,
                _ => 0,
            },
            0x10 => NO_VECTOR,
            0x12 => self.queues.len() as u64,
            0x14 => self.status as u64,
            0x15 => self.config_generation as u64,
            0x16 => self.queue_select as u64,
            0x18 => q.map_or(0, |q| q.size as u64),
            0x1a => NO_VECTOR,
            0x1c => q.map_or(0, |q| q.ready as u64),
            0x1e => self.queue_select as u64,
            0x20 | 0x24 => q.map_or(0, |q| half(q.desc, 0x20)),
            0x28 | 0x2c => q.map_or(0, |q| half(q.avail, 0x28)),
            0x30 | 0x34 => q.map_or(0, |q| half(q.used, 0x30)),
            _ => 0,
        };
        if size >= 8 { value } else { value & ((1 << (8 * size)) - 1) }
    }

    fn write_common(&mut self, offset: u64, size: u8, value: u64, irq: &Arc<Interrupt>, mem: &GuestMemory) {
        let set_half = |field: &mut u64, at: u64| match (offset - at, size) {
            (0, 8) => *field = value,
            (0, _) => *field = (*field & !0xffff_ffff) | (value & 0xffff_ffff),
            _ => *field = (*field & 0xffff_ffff) | (value << 32),
        };
        match offset {
            0x00 => self.device_feature_select = value as u32,
            0x08 => self.driver_feature_select = value as u32,
            0x0c => match self.driver_feature_select {
                0 => self.driver_features = (self.driver_features & !0xffff_ffff) | (value & 0xffff_ffff),
                1 => self.driver_features = (self.driver_features & 0xffff_ffff) | (value << 32),
                _ => {}
            },
            0x14 => self.write_status(value as u8, irq, mem),
            0x16 => self.queue_select = value as u16,
            0x18 => {
                if let Some(q) = self.queue() {
                    let mut q = q.lock().unwrap();
                    q.size = (value as u16).min(q.max_size);
                }
            }
            0x1c => {
                if let Some(q) = self.queue() {
                    q.lock().unwrap().ready = value & 1 != 0;
                }
            }
            0x20..0x38 => {
                if let Some(q) = self.queue() {
                    let mut q = q.lock().unwrap();
                    match offset {
                        0x20 | 0x24 => set_half(&mut q.desc, 0x20),
                        0x28 | 0x2c => set_half(&mut q.avail, 0x28),
                        _ => set_half(&mut q.used, 0x30),
                    }
                }
            }
            _ => {}
        }
    }

    fn write_status(&mut self, value: u8, irq: &Arc<Interrupt>, mem: &GuestMemory) {
        if value == 0 {
            self.reset(irq);
            return;
        }
        let mut value = value;
        if value & status::FEATURES_OK != 0
            && self.status & status::FEATURES_OK == 0
            && self.driver_features & !self.offered() != 0
        {
            value &= !status::FEATURES_OK;
        }
        let activate = value & status::DRIVER_OK != 0 && self.status & status::DRIVER_OK == 0;
        self.status = value;
        if activate {
            let event_idx = self.driver_features & feature::RING_EVENT_IDX != 0;
            for q in &self.queues {
                q.lock().unwrap().event_idx = event_idx;
            }
            self.device.activate(Activation {
                features: self.driver_features,
                queues: self.queues.clone(),
                mem: mem.clone(),
                irq: irq.clone(),
            });
        }
    }
}

fn virtio_cap(cfg_type: u8, next: u8, offset: u64, length: u64, extra: &[u8]) -> Vec<u8> {
    let mut cap = vec![CAP_VENDOR, next, 16 + extra.len() as u8, cfg_type, BAR as u8, 0, 0, 0];
    cap.extend_from_slice(&(offset as u32).to_le_bytes());
    cap.extend_from_slice(&(length as u32).to_le_bytes());
    cap.extend_from_slice(extra);
    cap
}

impl PciDevice for VirtioPci {
    fn header(&self) -> PciHeader {
        let mut bars = [Bar::None; 6];
        bars[BAR] = Bar::Mem64 { size: BAR_SIZE, prefetchable: true };
        // Capabilities start at configuration offset 0x40.
        let mut caps = virtio_cap(CFG_COMMON, 0x50, COMMON, REGION, &[]);
        caps.extend(virtio_cap(CFG_ISR, 0x60, ISR, REGION, &[]));
        caps.extend(virtio_cap(CFG_DEVICE, 0x70, DEVICE, REGION, &[]));
        caps.extend(virtio_cap(CFG_NOTIFY, 0, NOTIFY, REGION, &NOTIFY_MULTIPLIER.to_le_bytes()));
        PciHeader {
            vendor: 0x1af4,
            device: 0x1040 + self.device_type,
            revision: 1,
            class: self.class,
            subsystem_vendor: 0x1af4,
            subsystem: 0x1100,
            bars,
            capabilities: caps,
            intx: true,
        }
    }

    fn bar_read(&self, bar: usize, offset: u64, size: u8) -> u64 {
        if bar != BAR {
            return 0;
        }
        match offset {
            o if o < ISR => self.transport.lock().unwrap().read_common(o - COMMON, size),
            o if o < DEVICE => {
                if o == ISR {
                    self.irq.take() as u64
                } else {
                    0
                }
            }
            o if o < NOTIFY => {
                let t = self.transport.lock().unwrap();
                let mut buf = [0u8; 8];
                t.device.read_config((o - DEVICE) as usize, &mut buf[..size as usize]);
                u64::from_le_bytes(buf)
            }
            _ => 0,
        }
    }

    fn bar_write(&self, bar: usize, offset: u64, size: u8, value: u64) {
        if bar != BAR {
            return;
        }
        match offset {
            o if o < ISR => {
                self.transport.lock().unwrap().write_common(o - COMMON, size, value, &self.irq, &self.mem)
            }
            o if o < DEVICE => {}
            o if o < NOTIFY => {
                let mut t = self.transport.lock().unwrap();
                t.device.write_config((o - DEVICE) as usize, &value.to_le_bytes()[..size as usize]);
            }
            o if o < BAR_SIZE => {
                let queue = ((o - NOTIFY) / NOTIFY_MULTIPLIER as u64) as usize;
                let mut t = self.transport.lock().unwrap();
                if t.status & status::DRIVER_OK != 0 && queue < t.queues.len() {
                    t.device.notify(queue);
                }
            }
            _ => {}
        }
    }
}
