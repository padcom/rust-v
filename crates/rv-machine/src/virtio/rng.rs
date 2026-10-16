//! virtio-rng (§5.4): fills the driver's buffers with host entropy.

use super::{Activation, VirtioDevice, device_type};

#[derive(Default)]
pub struct Rng {
    active: Option<Activation>,
}

impl VirtioDevice for Rng {
    fn device_type(&self) -> u16 {
        device_type::RNG
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![64]
    }

    fn read_config(&self, _offset: usize, data: &mut [u8]) {
        data.fill(0);
    }

    fn activate(&mut self, activation: Activation) {
        self.active = Some(activation);
    }

    fn notify(&mut self, queue: usize) {
        let Some(a) = &self.active else { return };
        a.drain(queue, |chain| {
            let len = chain.writable_len().min(4096) as usize;
            let mut buf = vec![0u8; len];
            if getrandom::fill(&mut buf).is_err() {
                return 0;
            }
            chain.writer(&a.mem).write(&buf).unwrap_or(0) as u32
        });
    }

    fn reset(&mut self) {
        self.active = None;
    }

    fn pci_class(&self) -> u32 {
        0x00_ff_00
    }
}
