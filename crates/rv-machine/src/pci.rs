//! A PCIe host bridge in the style of QEMU's `gpex`: an ECAM configuration window for bus 0,
//! memory windows through which device BARs are reached, and legacy INTx interrupts routed to
//! four PLIC lines.
//!
//! Configuration space is modelled as bytes plus a write mask, so BAR sizing and read-only
//! fields fall out of the masks instead of needing special cases.

use std::sync::{Arc, Mutex, RwLock};

use crate::plic::Plic;

/// Devices (slots) on bus 0.
pub const NUM_SLOTS: usize = 32;
/// INTx lines (INTA..INTD) and the number of PLIC sources they use.
pub const NUM_INTX: u32 = 4;
pub const ECAM_SIZE: u64 = 0x1000_0000;

const CONFIG_SIZE: usize = 4096;

mod reg {
    pub const VENDOR_ID: usize = 0x00;
    pub const DEVICE_ID: usize = 0x02;
    pub const COMMAND: usize = 0x04;
    pub const STATUS: usize = 0x06;
    pub const REVISION: usize = 0x08;
    pub const CLASS: usize = 0x09;
    pub const CACHE_LINE: usize = 0x0c;
    pub const LATENCY: usize = 0x0d;
    pub const BAR0: usize = 0x10;
    pub const SUBSYSTEM_VENDOR: usize = 0x2c;
    pub const SUBSYSTEM: usize = 0x2e;
    pub const CAPABILITIES: usize = 0x34;
    pub const INTERRUPT_LINE: usize = 0x3c;
    pub const INTERRUPT_PIN: usize = 0x3d;
    /// Where device capabilities start.
    pub const CAP_START: usize = 0x40;
}

const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_WRITABLE: u16 = 0x0547;
const STATUS_CAP_LIST: u16 = 1 << 4;
const STATUS_W1C: u16 = 0xf900;

const BAR_MEM64: u32 = 0b100;
const BAR_PREFETCH: u32 = 0b1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    None,
    /// A 32-bit memory BAR of `size` bytes (a power of two, at least 16).
    Mem32 {
        size: u64,
    },
    /// A 64-bit memory BAR; it also takes up the next BAR slot.
    Mem64 {
        size: u64,
        prefetchable: bool,
    },
}

/// What a device puts in its configuration header.
#[derive(Clone, Debug)]
pub struct PciHeader {
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    /// Class code, subclass and programming interface (24 bits).
    pub class: u32,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
    pub bars: [Bar; 6],
    /// Capability list, placed at configuration offset 0x40. `next` pointers are absolute
    /// configuration-space offsets.
    pub capabilities: Vec<u8>,
    /// Uses INTA#.
    pub intx: bool,
}

/// A PCI function as seen by the host bridge.
pub trait PciDevice: Send + Sync {
    fn header(&self) -> PciHeader;
    fn bar_read(&self, bar: usize, offset: u64, size: u8) -> u64;
    fn bar_write(&self, bar: usize, offset: u64, size: u8, value: u64);

    /// Device state for a snapshot (the configuration space is saved by the host bridge).
    fn save(&self, _w: &mut rv_core::snapshot::Writer) {}

    fn restore(&self, _r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        Ok(())
    }
}

/// One INTx pin of one slot. Lines are shared between slots and wired-OR'ed.
#[derive(Clone)]
pub struct Intx {
    lines: Arc<IntxLines>,
    slot: usize,
}

struct IntxLines {
    plic: Arc<Plic>,
    first_irq: u32,
    /// Slots currently asserting each line.
    asserted: Mutex<[u32; NUM_INTX as usize]>,
}

impl Intx {
    /// The line INTA# of `slot` is swizzled onto.
    fn line(&self) -> usize {
        self.slot % NUM_INTX as usize
    }

    pub fn set(&self, level: bool) {
        let line = self.line();
        let mut asserted = self.lines.asserted.lock().unwrap();
        let bit = 1 << self.slot;
        let before = asserted[line] != 0;
        if level {
            asserted[line] |= bit;
        } else {
            asserted[line] &= !bit;
        }
        let after = asserted[line] != 0;
        if before != after {
            self.lines.plic.set_level(self.lines.first_irq + line as u32, after);
        }
    }
}

struct Function {
    device: Arc<dyn PciDevice>,
    bars: [Bar; 6],
    config: Box<[u8; CONFIG_SIZE]>,
    wmask: Box<[u8; CONFIG_SIZE]>,
    w1c: Box<[u8; CONFIG_SIZE]>,
}

impl Function {
    fn new(device: Arc<dyn PciDevice>) -> Self {
        let h = device.header();
        let mut f = Function {
            device,
            bars: h.bars,
            config: Box::new([0; CONFIG_SIZE]),
            wmask: Box::new([0; CONFIG_SIZE]),
            w1c: Box::new([0; CONFIG_SIZE]),
        };
        f.put16(reg::VENDOR_ID, h.vendor);
        f.put16(reg::DEVICE_ID, h.device);
        f.config[reg::REVISION] = h.revision;
        f.config[reg::CLASS..reg::CLASS + 3].copy_from_slice(&h.class.to_le_bytes()[..3]);
        f.put16(reg::SUBSYSTEM_VENDOR, h.subsystem_vendor);
        f.put16(reg::SUBSYSTEM, h.subsystem);
        f.wmask[reg::COMMAND..reg::COMMAND + 2].copy_from_slice(&COMMAND_WRITABLE.to_le_bytes());
        f.w1c[reg::STATUS..reg::STATUS + 2].copy_from_slice(&STATUS_W1C.to_le_bytes());
        f.wmask[reg::CACHE_LINE] = 0xff;
        f.wmask[reg::LATENCY] = 0xff;
        f.wmask[reg::INTERRUPT_LINE] = 0xff;
        f.config[reg::INTERRUPT_PIN] = h.intx as u8;
        if !h.capabilities.is_empty() {
            assert!(reg::CAP_START + h.capabilities.len() <= 0x100, "capabilities overflow the header");
            f.put16(reg::STATUS, STATUS_CAP_LIST);
            f.config[reg::CAPABILITIES] = reg::CAP_START as u8;
            f.config[reg::CAP_START..reg::CAP_START + h.capabilities.len()].copy_from_slice(&h.capabilities);
        }
        let mut i = 0;
        while i < 6 {
            let at = reg::BAR0 + 4 * i;
            match h.bars[i] {
                Bar::None => {}
                Bar::Mem32 { size } => {
                    assert!(size.is_power_of_two() && (16..=1 << 31).contains(&size));
                    f.wmask[at..at + 4].copy_from_slice(&(!(size as u32 - 1)).to_le_bytes());
                }
                Bar::Mem64 { size, prefetchable } => {
                    assert!(i < 5 && size.is_power_of_two() && size >= 16);
                    let flags = BAR_MEM64 | if prefetchable { BAR_PREFETCH } else { 0 };
                    f.config[at..at + 4].copy_from_slice(&flags.to_le_bytes());
                    let mask = !(size - 1);
                    f.wmask[at..at + 4].copy_from_slice(&((mask as u32) & !0xf).to_le_bytes());
                    f.wmask[at + 4..at + 8].copy_from_slice(&((mask >> 32) as u32).to_le_bytes());
                    i += 1;
                }
            }
            i += 1;
        }
        f
    }

    fn put16(&mut self, at: usize, v: u16) {
        self.config[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn read(&self, offset: usize, size: u8) -> u64 {
        let mut v = 0u64;
        for i in 0..size as usize {
            v |= (self.config[offset + i] as u64) << (8 * i);
        }
        v
    }

    fn write(&mut self, offset: usize, size: u8, value: u64) {
        for i in 0..size as usize {
            let (o, b) = (offset + i, (value >> (8 * i)) as u8);
            let c = &mut self.config[o];
            *c = (*c & !self.wmask[o]) | (b & self.wmask[o]);
            *c &= !(b & self.w1c[o]);
        }
    }

    /// Guest-physical ranges the memory BARs currently decode.
    fn mapped_bars(&self) -> Vec<(u64, u64, usize)> {
        let command = u16::from_le_bytes([self.config[reg::COMMAND], self.config[reg::COMMAND + 1]]);
        if command & COMMAND_MEMORY == 0 {
            return Vec::new();
        }
        let dword = |i: usize| self.read(reg::BAR0 + 4 * i, 4);
        let mut out = Vec::new();
        for (i, bar) in self.bars.iter().enumerate() {
            let (base, size) = match *bar {
                Bar::None => continue,
                Bar::Mem32 { size } => (dword(i) & !0xf, size),
                Bar::Mem64 { size, .. } => ((dword(i) & !0xf) | (dword(i + 1) << 32), size),
            };
            if base != 0 {
                out.push((base, size, i));
            }
        }
        out
    }
}

struct Mapping {
    start: u64,
    end: u64,
    slot: usize,
    bar: usize,
    device: Arc<dyn PciDevice>,
}

pub struct PciHost {
    functions: Mutex<Vec<Option<Function>>>,
    mappings: RwLock<Vec<Mapping>>,
    intx: Arc<IntxLines>,
}

impl PciHost {
    /// Creates a host bridge whose INTA..INTD lines are PLIC sources `first_irq..first_irq + 4`.
    /// Slot 0 holds the host bridge function itself.
    pub fn new(plic: Arc<Plic>, first_irq: u32) -> Arc<Self> {
        let host = Arc::new(PciHost {
            functions: Mutex::new((0..NUM_SLOTS).map(|_| None).collect()),
            mappings: RwLock::new(Vec::new()),
            intx: Arc::new(IntxLines { plic, first_irq, asserted: Mutex::new([0; NUM_INTX as usize]) }),
        });
        host.attach(0, Arc::new(HostBridge));
        host
    }

    /// The INTx pin a device in `slot` drives.
    pub fn intx(&self, slot: usize) -> Intx {
        Intx { lines: self.intx.clone(), slot }
    }

    /// PLIC source that INTA# of `slot` arrives on.
    pub fn slot_irq(&self, slot: usize) -> u32 {
        self.intx.first_irq + (slot % NUM_INTX as usize) as u32
    }

    /// Plugs `device` into `slot` of bus 0.
    pub fn attach(&self, slot: usize, device: Arc<dyn PciDevice>) {
        let mut functions = self.functions.lock().unwrap();
        assert!(functions[slot].is_none(), "PCI slot {slot} is taken");
        functions[slot] = Some(Function::new(device));
    }

    /// First free slot.
    pub fn free_slot(&self) -> Option<usize> {
        self.functions.lock().unwrap().iter().position(Option::is_none)
    }

    pub fn save(&self, w: &mut rv_core::snapshot::Writer) {
        let functions = self.functions.lock().unwrap();
        for f in functions.iter() {
            match f {
                Some(f) => {
                    w.bool(true);
                    w.bytes(&f.config[..]);
                    f.device.save(w);
                }
                None => w.bool(false),
            }
        }
        w.bytes(&self.intx.asserted.lock().unwrap().iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
    }

    pub fn restore(&self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        let mut functions = self.functions.lock().unwrap();
        for slot in 0..functions.len() {
            let present = r.bool()?;
            match (&mut functions[slot], present) {
                (Some(f), true) => {
                    let config = r.bytes()?;
                    if config.len() != CONFIG_SIZE {
                        return Err("bad PCI configuration in snapshot".into());
                    }
                    f.config.copy_from_slice(config);
                    f.device.restore(r)?;
                }
                (None, false) => {}
                _ => return Err("the snapshot's PCI devices differ from this machine's".into()),
            }
        }
        let asserted = r.bytes()?;
        let mut lines = self.intx.asserted.lock().unwrap();
        for (i, v) in asserted.as_chunks::<4>().0.iter().enumerate().take(lines.len()) {
            lines[i] = u32::from_le_bytes(*v);
        }
        drop(lines);
        for slot in 0..functions.len() {
            if let Some(f) = &functions[slot] {
                self.remap(slot, f);
            }
        }
        Ok(())
    }

    fn config_read(&self, offset: u64, size: u8) -> u64 {
        let ones = if size >= 8 { u64::MAX } else { (1 << (8 * size)) - 1 };
        let Some((slot, reg)) = decode_ecam(offset) else { return ones };
        let functions = self.functions.lock().unwrap();
        match &functions[slot] {
            Some(f) if reg + size as usize <= CONFIG_SIZE => f.read(reg, size),
            _ => ones,
        }
    }

    fn config_write(&self, offset: u64, size: u8, value: u64) {
        let Some((slot, reg)) = decode_ecam(offset) else { return };
        let mut functions = self.functions.lock().unwrap();
        let Some(f) = &mut functions[slot] else { return };
        if reg + size as usize > CONFIG_SIZE {
            return;
        }
        f.write(reg, size, value);
        if reg < reg::BAR0 + 24 && reg + size as usize > reg::COMMAND {
            self.remap(slot, f);
        }
    }

    fn remap(&self, slot: usize, f: &Function) {
        let mut mappings = self.mappings.write().unwrap();
        mappings.retain(|m| m.slot != slot);
        for (start, size, bar) in f.mapped_bars() {
            mappings.push(Mapping { start, end: start + size, slot, bar, device: f.device.clone() });
        }
    }

    fn mem_read(&self, addr: u64, size: u8) -> u64 {
        let mappings = self.mappings.read().unwrap();
        match mappings.iter().find(|m| (m.start..m.end).contains(&addr)) {
            Some(m) => m.device.bar_read(m.bar, addr - m.start, size),
            None => u64::MAX,
        }
    }

    fn mem_write(&self, addr: u64, size: u8, value: u64) {
        let mappings = self.mappings.read().unwrap();
        if let Some(m) = mappings.iter().find(|m| (m.start..m.end).contains(&addr)) {
            m.device.bar_write(m.bar, addr - m.start, size, value);
        }
    }
}

/// Slot and register of an ECAM offset; only function 0 of devices on bus 0 exists.
fn decode_ecam(offset: u64) -> Option<(usize, usize)> {
    let (bus, slot, func) = (offset >> 20, (offset >> 15) & 31, (offset >> 12) & 7);
    (bus == 0 && func == 0).then_some((slot as usize, (offset & 0xfff) as usize))
}

/// The ECAM configuration window.
pub struct Ecam(pub Arc<PciHost>);

impl rv_core::Device for Ecam {
    fn read(&self, offset: u64, size: u8) -> u64 {
        self.0.config_read(offset, size)
    }

    fn write(&self, offset: u64, size: u8, value: u64) {
        self.0.config_write(offset, size, value)
    }
}

/// A memory window: guest-physical addresses `base..base + size` reach whichever BAR is
/// assigned there.
pub struct MemWindow {
    pub host: Arc<PciHost>,
    pub base: u64,
}

impl rv_core::Device for MemWindow {
    fn read(&self, offset: u64, size: u8) -> u64 {
        self.host.mem_read(self.base + offset, size)
    }

    fn write(&self, offset: u64, size: u8, value: u64) {
        self.host.mem_write(self.base + offset, size, value)
    }
}

/// The I/O port window. No device has I/O BARs, so it reads as zero and ignores writes.
pub struct IoWindow;

impl rv_core::Device for IoWindow {
    fn read(&self, _offset: u64, _size: u8) -> u64 {
        0
    }

    fn write(&self, _offset: u64, _size: u8, _value: u64) {}
}

/// The host bridge function at 00:00.0 (Red Hat's QEMU PCIe host bridge IDs).
struct HostBridge;

impl PciDevice for HostBridge {
    fn header(&self) -> PciHeader {
        PciHeader {
            vendor: 0x1b36,
            device: 0x0008,
            revision: 0,
            class: 0x06_00_00,
            subsystem_vendor: 0x1af4,
            subsystem: 0x1100,
            bars: [Bar::None; 6],
            capabilities: Vec::new(),
            intx: false,
        }
    }

    fn bar_read(&self, _bar: usize, _offset: u64, _size: u8) -> u64 {
        0
    }

    fn bar_write(&self, _bar: usize, _offset: u64, _size: u8, _value: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rv_core::{Device, HartShared};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Probe {
        last: AtomicU64,
    }

    impl PciDevice for Probe {
        fn header(&self) -> PciHeader {
            let mut bars = [Bar::None; 6];
            bars[0] = Bar::Mem32 { size: 0x1000 };
            bars[2] = Bar::Mem64 { size: 0x4000, prefetchable: true };
            PciHeader {
                vendor: 0x1234,
                device: 0x5678,
                revision: 1,
                class: 0x02_00_00,
                subsystem_vendor: 0,
                subsystem: 0,
                bars,
                capabilities: vec![0x09, 0, 4, 0],
                intx: true,
            }
        }

        fn bar_read(&self, bar: usize, offset: u64, _size: u8) -> u64 {
            ((bar as u64) << 32) | offset
        }

        fn bar_write(&self, _bar: usize, offset: u64, _size: u8, value: u64) {
            self.last.store(offset ^ value, Ordering::Relaxed);
        }
    }

    fn cfg(slot: u64, reg: u64) -> u64 {
        (slot << 15) | reg
    }

    #[test]
    fn enumeration_and_bar_sizing() {
        let hart = Arc::new(HartShared::default());
        let plic = Arc::new(Plic::new(std::slice::from_ref(&hart)));
        let host = PciHost::new(plic, 32);
        let probe = Arc::new(Probe { last: AtomicU64::new(0) });
        host.attach(1, probe.clone());
        let ecam = Ecam(host.clone());

        assert_eq!(ecam.read(cfg(0, 0), 4), 0x0008_1b36);
        assert_eq!(ecam.read(cfg(1, 0), 4), 0x5678_1234);
        assert_eq!(ecam.read(cfg(2, 0), 4), 0xffff_ffff, "empty slot");
        assert_eq!(ecam.read(cfg(1, 0x08), 4), 0x0200_0001);
        assert_eq!(ecam.read(cfg(1, 0x06), 2) & STATUS_CAP_LIST as u64, STATUS_CAP_LIST as u64);
        assert_eq!(ecam.read(cfg(1, 0x34), 1), 0x40);
        assert_eq!(ecam.read(cfg(1, 0x40), 1), 0x09);
        assert_eq!(ecam.read(cfg(1, 0x3d), 1), 1);

        // Sizing: write all ones, read back the size mask.
        ecam.write(cfg(1, 0x10), 4, 0xffff_ffff);
        assert_eq!(ecam.read(cfg(1, 0x10), 4), 0xffff_f000);
        ecam.write(cfg(1, 0x18), 4, 0xffff_ffff);
        ecam.write(cfg(1, 0x1c), 4, 0xffff_ffff);
        assert_eq!(ecam.read(cfg(1, 0x18), 4), 0xffff_c00c);
        assert_eq!(ecam.read(cfg(1, 0x1c), 4), 0xffff_ffff);
        assert_eq!(ecam.read(cfg(1, 0x14), 4), 0, "unimplemented BAR");

        // Assign and enable decoding.
        ecam.write(cfg(1, 0x10), 4, 0x4000_0000);
        ecam.write(cfg(1, 0x18), 4, 0x0000_0000);
        ecam.write(cfg(1, 0x1c), 4, 0x4);
        let low = MemWindow { host: host.clone(), base: 0x4000_0000 };
        let high = MemWindow { host: host.clone(), base: 0x4_0000_0000 };
        assert_eq!(low.read(0x10, 4), u64::MAX, "decoding still off");
        ecam.write(cfg(1, 0x04), 2, COMMAND_MEMORY as u64);
        assert_eq!(low.read(0x10, 4), 0x10);
        assert_eq!(high.read(0x20, 4), (2 << 32) | 0x20);
        high.write(0x8, 4, 0xff);
        assert_eq!(probe.last.load(Ordering::Relaxed), 0xf7);
        assert_eq!(low.read(0x1000, 4), u64::MAX, "past the end of the BAR");

        // Read-only fields stay put.
        ecam.write(cfg(1, 0x00), 4, 0);
        assert_eq!(ecam.read(cfg(1, 0), 4), 0x5678_1234);
    }

    #[test]
    fn intx_lines_are_shared() {
        let hart = Arc::new(HartShared::default());
        let plic = Arc::new(Plic::new(std::slice::from_ref(&hart)));
        let host = PciHost::new(plic.clone(), 32);
        // Enable source 33 for the S-mode context.
        plic.write(4 * 33, 4, 1);
        plic.write(0x2000 + 0x80 + 4, 4, 1 << 1);
        let sei = || hart.mip.load(Ordering::Relaxed) & (1 << rv_core::trap::irq::SEI) != 0;
        let (a, b) = (host.intx(1), host.intx(5));
        assert_eq!(host.slot_irq(5), 33);
        a.set(true);
        assert!(sei());
        b.set(true);
        a.set(false);
        assert!(sei(), "slot 5 still asserts the shared line");
        b.set(false);
        assert!(!sei());
    }
}
