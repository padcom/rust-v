//! SiFive-compatible PLIC with two contexts (M and S) per hart.

use std::sync::{Arc, Mutex};

use rv_core::HartShared;
use rv_core::trap::irq;

/// Number of interrupt sources, including the reserved source 0.
pub const NUM_SOURCES: usize = 96;
pub const SIZE: u64 = 0x60_0000;

const PRIORITY_BASE: u64 = 0x0;
const PENDING_BASE: u64 = 0x1000;
const ENABLE_BASE: u64 = 0x2000;
const ENABLE_STRIDE: u64 = 0x80;
const CONTEXT_BASE: u64 = 0x20_0000;
const CONTEXT_STRIDE: u64 = 0x1000;

type Bits = u128;
const _: () = assert!(NUM_SOURCES <= Bits::BITS as usize);

struct Context {
    hart: Arc<HartShared>,
    /// The `mip` bit this context drives (MEIP or SEIP).
    mip_bit: u64,
    enable: Bits,
    threshold: u32,
    /// Last level driven onto `mip_bit`, to avoid needless hart wake-ups.
    output: bool,
}

struct State {
    priority: [u32; NUM_SOURCES],
    level: Bits,
    pending: Bits,
    /// Sources claimed and not yet completed; their gateway is closed.
    claimed: Bits,
    contexts: Vec<Context>,
}

pub struct Plic {
    state: Mutex<State>,
}

impl Plic {
    /// Creates a PLIC with contexts `2*h` (M-mode) and `2*h + 1` (S-mode) for each hart `h`.
    pub fn new(harts: &[Arc<HartShared>]) -> Self {
        let contexts = harts
            .iter()
            .flat_map(|h| {
                [1 << irq::MEI, 1 << irq::SEI].map(|mip_bit| Context {
                    hart: h.clone(),
                    mip_bit,
                    enable: 0,
                    threshold: 0,
                    output: false,
                })
            })
            .collect();
        Plic {
            state: Mutex::new(State {
                priority: [0; NUM_SOURCES],
                level: 0,
                pending: 0,
                claimed: 0,
                contexts,
            }),
        }
    }

    /// Sets the level of interrupt line `source` (1-based).
    pub fn set_level(&self, source: u32, level: bool) {
        assert!((1..NUM_SOURCES as u32).contains(&source));
        let mut s = self.state.lock().unwrap();
        let bit: Bits = 1 << source;
        if level {
            s.level |= bit;
            if s.claimed & bit == 0 {
                s.pending |= bit;
            }
        } else {
            s.level &= !bit;
            s.pending &= !bit;
        }
        s.update();
    }
}

impl State {
    /// Highest-priority pending source that context `ctx` would be interrupted by.
    fn best(&self, ctx: usize) -> Option<usize> {
        let c = &self.contexts[ctx];
        let candidates = self.pending & c.enable & !1;
        let mut best: Option<usize> = None;
        for src in 1..NUM_SOURCES {
            if candidates & (1 << src) == 0 || self.priority[src] <= c.threshold {
                continue;
            }
            // Ties go to the lowest source number.
            if best.is_none_or(|b| self.priority[src] > self.priority[b]) {
                best = Some(src);
            }
        }
        best
    }

    fn update(&mut self) {
        for ctx in 0..self.contexts.len() {
            let level = self.best(ctx).is_some();
            let c = &mut self.contexts[ctx];
            if level != c.output {
                c.output = level;
                c.hart.set_irq(c.mip_bit, level);
            }
        }
    }

    fn claim(&mut self, ctx: usize) -> u32 {
        let Some(src) = self.best(ctx) else { return 0 };
        self.pending &= !(1 << src);
        self.claimed |= 1 << src;
        self.update();
        src as u32
    }

    fn complete(&mut self, ctx: usize, src: u32) {
        let src = src as usize;
        if src == 0 || src >= NUM_SOURCES || self.contexts[ctx].enable & (1 << src) == 0 {
            return;
        }
        self.claimed &= !(1 << src);
        if self.level & (1 << src) != 0 {
            self.pending |= 1 << src;
        }
        self.update();
    }
}

impl rv_core::Device for Plic {
    fn read(&self, offset: u64, _size: u8) -> u64 {
        let mut s = self.state.lock().unwrap();
        let ctxs = s.contexts.len() as u64;
        let word = |bits: Bits, w: u64| (bits >> (32 * w)) as u32 as u64;
        match offset {
            o if o < PENDING_BASE => {
                s.priority.get(((o - PRIORITY_BASE) / 4) as usize).copied().unwrap_or(0) as u64
            }
            o if o < ENABLE_BASE => word(s.pending, (o - PENDING_BASE) / 4),
            o if (ENABLE_BASE..ENABLE_BASE + ENABLE_STRIDE * ctxs).contains(&o) => {
                let ctx = ((o - ENABLE_BASE) / ENABLE_STRIDE) as usize;
                word(s.contexts[ctx].enable, (o - ENABLE_BASE) % ENABLE_STRIDE / 4)
            }
            o if (CONTEXT_BASE..CONTEXT_BASE + CONTEXT_STRIDE * ctxs).contains(&o) => {
                let ctx = ((o - CONTEXT_BASE) / CONTEXT_STRIDE) as usize;
                match (o - CONTEXT_BASE) % CONTEXT_STRIDE {
                    0 => s.contexts[ctx].threshold as u64,
                    4 => s.claim(ctx) as u64,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn write(&self, offset: u64, _size: u8, value: u64) {
        let mut s = self.state.lock().unwrap();
        let ctxs = s.contexts.len() as u64;
        let value = value as u32;
        match offset {
            o if o < PENDING_BASE => {
                let src = ((o - PRIORITY_BASE) / 4) as usize;
                if (1..NUM_SOURCES).contains(&src) {
                    s.priority[src] = value & 7;
                }
            }
            o if (ENABLE_BASE..ENABLE_BASE + ENABLE_STRIDE * ctxs).contains(&o) => {
                let ctx = ((o - ENABLE_BASE) / ENABLE_STRIDE) as usize;
                let w = (o - ENABLE_BASE) % ENABLE_STRIDE / 4;
                if w < (NUM_SOURCES as u64).div_ceil(32) {
                    let mask: Bits = (u32::MAX as Bits) << (32 * w);
                    let e = &mut s.contexts[ctx].enable;
                    *e = ((*e & !mask) | ((value as Bits) << (32 * w))) & !1;
                }
            }
            o if (CONTEXT_BASE..CONTEXT_BASE + CONTEXT_STRIDE * ctxs).contains(&o) => {
                let ctx = ((o - CONTEXT_BASE) / CONTEXT_STRIDE) as usize;
                match (o - CONTEXT_BASE) % CONTEXT_STRIDE {
                    0 => s.contexts[ctx].threshold = value & 7,
                    4 => {
                        s.complete(ctx, value);
                        return;
                    }
                    _ => return,
                }
            }
            _ => return,
        }
        s.update();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rv_core::Device;
    use std::sync::atomic::Ordering;

    fn mip(h: &HartShared) -> u64 {
        h.mip.load(Ordering::Relaxed)
    }

    #[test]
    fn claim_complete_cycle() {
        let hart = Arc::new(HartShared::default());
        let plic = Plic::new(std::slice::from_ref(&hart));
        let s_ctx = 1;
        plic.write(PRIORITY_BASE + 4 * 10, 4, 1);
        plic.write(ENABLE_BASE + ENABLE_STRIDE * s_ctx, 4, 1 << 10);

        plic.set_level(10, true);
        assert_eq!(mip(&hart), 1 << irq::SEI);
        assert_eq!(plic.read(PENDING_BASE, 4), 1 << 10);

        let claim = CONTEXT_BASE + CONTEXT_STRIDE * s_ctx + 4;
        assert_eq!(plic.read(claim, 4), 10);
        assert_eq!(mip(&hart), 0, "claiming drops the output");
        assert_eq!(plic.read(claim, 4), 0, "nothing else pending");

        // Still asserted at completion: becomes pending again.
        plic.write(claim, 4, 10);
        assert_eq!(mip(&hart), 1 << irq::SEI);
        assert_eq!(plic.read(claim, 4), 10);
        plic.set_level(10, false);
        plic.write(claim, 4, 10);
        assert_eq!(mip(&hart), 0);
    }

    #[test]
    fn threshold_and_priority() {
        let hart = Arc::new(HartShared::default());
        let plic = Plic::new(std::slice::from_ref(&hart));
        plic.write(PRIORITY_BASE + 4 * 3, 4, 2);
        plic.write(PRIORITY_BASE + 4 * 5, 4, 5);
        plic.write(ENABLE_BASE, 4, (1 << 3) | (1 << 5));
        plic.write(CONTEXT_BASE, 4, 2); // M context threshold
        plic.set_level(3, true);
        assert_eq!(mip(&hart), 0, "priority 2 does not exceed threshold 2");
        plic.set_level(5, true);
        assert_eq!(mip(&hart), 1 << irq::MEI);
        assert_eq!(plic.read(CONTEXT_BASE + 4, 4), 5);
    }
}
