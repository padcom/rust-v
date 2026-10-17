//! Machine snapshots: save the running machine to a file, and start a new one from it.
//!
//! Saving pauses every hart at a point between slices (each serialises its own state there),
//! freezes the devices that write guest memory from their own threads, writes the file, and
//! lets everything continue. A snapshot holds the harts, the non-zero pages of RAM, the device
//! state (including the in-memory changes of `,snapshot` disks) and the time; it can only be
//! restored into a machine with the same configuration, by the same build.
//!
//! Disks that are written through to their image files are not part of the snapshot: restoring
//! after the guest has written more to them gives the guest a disk that does not match its
//! memory. Network connections do not survive either (the guest sees them reset).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rv_core::snapshot::{Reader, Writer};
use rv_core::{Bus, GuestMemory, Hart, HartShared, HostClock, TimeSource};

use crate::{Machine, clint, pci, plic, rtc, uart};

const MAGIC: &[u8; 8] = b"RUSTVSNP";
const FORMAT: u32 = 1;
const PAGE: usize = 4096;

/// Where hart threads stop while a snapshot is taken.
#[derive(Default)]
pub struct Pause {
    state: Mutex<PauseState>,
    resumed: Condvar,
}

#[derive(Default)]
struct PauseState {
    requested: bool,
    /// Each hart's state, filled in as it stops.
    harts: Vec<Option<Vec<u8>>>,
    generation: u64,
}

impl Pause {
    /// Called by a hart thread between slices: if a snapshot is being taken, saves the hart
    /// and waits until the snapshot is written.
    pub(crate) fn check(&self, id: usize, hart: &Hart) {
        let mut s = self.state.lock().unwrap();
        if !s.requested || s.harts[id].is_some() {
            return;
        }
        let mut w = Writer::default();
        hart.save_state(&mut w);
        s.harts[id] = Some(w.buf);
        self.resumed.notify_all();
        let generation = s.generation;
        while s.generation == generation {
            s = self.resumed.wait(s).unwrap();
        }
    }

    /// The same for all harts at once, from the thread that runs them round-robin.
    pub(crate) fn check_all(&self, harts: &[Hart]) {
        let mut s = self.state.lock().unwrap();
        if !s.requested {
            return;
        }
        for (id, hart) in harts.iter().enumerate() {
            let mut w = Writer::default();
            hart.save_state(&mut w);
            s.harts[id] = Some(w.buf);
        }
        self.resumed.notify_all();
        let generation = s.generation;
        while s.generation == generation {
            s = self.resumed.wait(s).unwrap();
        }
    }
}

/// What it takes to snapshot a machine while its harts run elsewhere.
#[derive(Clone)]
pub struct SnapshotHandle {
    pause: Arc<Pause>,
    harts: Vec<Arc<HartShared>>,
    bus: Arc<Bus>,
    memory: GuestMemory,
    clock: Arc<HostClock>,
    clint: Arc<clint::Clint>,
    plic: Arc<plic::Plic>,
    uart: Arc<uart::Uart>,
    rtc: Arc<rtc::GoldfishRtc>,
    pci: Arc<pci::PciHost>,
    fingerprint: String,
    blocker: Option<&'static str>,
    /// Some disk is written through to its image (the snapshot does not capture it).
    pub disks_written_through: bool,
}

impl SnapshotHandle {
    /// Pauses the machine, writes the snapshot to `path` and lets the machine continue.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(why) = self.blocker {
            return Err(format!("snapshots are not supported: {why}"));
        }
        let started = Instant::now();
        self.memory.set_frozen(true);
        {
            let mut s = self.pause.state.lock().unwrap();
            s.requested = true;
            s.harts = vec![None; self.harts.len()];
        }
        for h in &self.harts {
            h.kick();
        }
        let result = (|| {
            let harts = {
                let mut s = self.pause.state.lock().unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while s.harts.iter().any(Option::is_none) {
                    // Harts waiting in WFI wake up when kicked; keep kicking in case one missed it.
                    if Instant::now() > deadline {
                        return Err("the harts did not stop for the snapshot".to_string());
                    }
                    s = self.pause.resumed.wait_timeout(s, Duration::from_millis(50)).unwrap().0;
                    for h in &self.harts {
                        h.kick();
                    }
                }
                s.harts.iter().map(|h| h.clone().unwrap()).collect::<Vec<_>>()
            };
            self.write(path, &harts)
        })();
        {
            let mut s = self.pause.state.lock().unwrap();
            s.requested = false;
            s.harts.clear();
            s.generation += 1;
            self.pause.resumed.notify_all();
        }
        self.memory.set_frozen(false);
        result.map(|bytes| {
            eprint!(
                "\r\nrust-v: snapshot saved to {} ({} MiB, {:.1} s)\r\n",
                path.display(),
                bytes >> 20,
                started.elapsed().as_secs_f64()
            );
        })
    }

    fn write(&self, path: &Path, harts: &[Vec<u8>]) -> Result<u64, String> {
        let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let io = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut header = Writer::default();
        header.buf.extend_from_slice(MAGIC);
        header.u32(FORMAT);
        header.str(env!("CARGO_PKG_VERSION"));
        header.str(&self.fingerprint);
        header.u64(self.clock.mtime());
        header.u64(harts.len() as u64);
        for h in harts {
            header.bytes(h);
        }
        out.write_all(&header.buf).map_err(io)?;
        let mut written = header.buf.len() as u64;

        // RAM: (page index, page) for every page that is not all zeros, then u64::MAX.
        let ram = self.bus.ram();
        let mut page = vec![0u8; PAGE];
        for index in 0..ram.size() / PAGE as u64 {
            ram.read_bytes(index * PAGE as u64, &mut page);
            if page.iter().all(|&b| b == 0) {
                continue;
            }
            out.write_all(&index.to_le_bytes()).map_err(io)?;
            out.write_all(&page).map_err(io)?;
            written += 8 + PAGE as u64;
        }
        out.write_all(&u64::MAX.to_le_bytes()).map_err(io)?;

        let mut devices = Writer::default();
        self.clint.save(&mut devices);
        self.rtc.save(&mut devices);
        self.uart.save(&mut devices);
        self.pci.save(&mut devices);
        self.plic.save(&mut devices);
        out.write_all(&devices.buf).map_err(io)?;
        written += devices.buf.len() as u64;
        out.flush().map_err(io)?;
        Ok(written)
    }
}

impl Machine {
    /// A handle for saving snapshots of this machine while it runs.
    pub fn snapshot_handle(&self) -> SnapshotHandle {
        SnapshotHandle {
            pause: self.pause.clone(),
            harts: self.harts.iter().map(|h| h.shared.clone()).collect(),
            bus: self.bus.clone(),
            memory: self.bus.memory(),
            clock: self.clock.clone(),
            clint: self.clint.clone(),
            plic: self.plic.clone(),
            uart: self.uart.clone(),
            rtc: self.rtc.clone(),
            pci: self.pci.clone(),
            fingerprint: self.fingerprint.clone(),
            blocker: self.snapshot_blocker,
            disks_written_through: self.disks_written_through,
        }
    }

    /// Replaces this (not yet started) machine's state with the snapshot at `path`.
    pub fn restore(&mut self, path: &Path) -> Result<(), String> {
        if let Some(why) = self.snapshot_blocker {
            return Err(format!("snapshots are not supported: {why}"));
        }
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut input = BufReader::with_capacity(1 << 20, file);
        let mut read = |n: usize| -> Result<Vec<u8>, String> {
            let mut buf = vec![0u8; n];
            input.read_exact(&mut buf).map_err(|_| format!("{}: truncated snapshot", path.display()))?;
            Ok(buf)
        };
        let not_ours = || format!("{} is not a rust-v snapshot", path.display());
        if read(8)? != MAGIC {
            return Err(not_ours());
        }
        let u32_le = |b: Vec<u8>| u32::from_le_bytes(b.try_into().unwrap());
        let u64_le = |b: Vec<u8>| u64::from_le_bytes(b.try_into().unwrap());
        if u32_le(read(4)?) != FORMAT {
            return Err(format!("{}: unsupported snapshot format", path.display()));
        }
        let string = |read: &mut dyn FnMut(usize) -> Result<Vec<u8>, String>| -> Result<String, String> {
            let n = u64_le(read(8)?) as usize;
            String::from_utf8(read(n)?).map_err(|_| not_ours())
        };
        let version = string(&mut read)?;
        if version != env!("CARGO_PKG_VERSION") {
            return Err(format!("{} was saved by rust-v {version}", path.display()));
        }
        let fingerprint = string(&mut read)?;
        if fingerprint != self.fingerprint {
            return Err(format!(
                "{} was saved from a machine configured differently ({fingerprint}; this one: {})",
                path.display(),
                self.fingerprint
            ));
        }
        let mtime = u64_le(read(8)?);
        let harts = u64_le(read(8)?) as usize;
        if harts != self.harts.len() {
            return Err("the snapshot has a different number of harts".into());
        }
        for hart in &mut self.harts {
            let n = u64_le(read(8)?) as usize;
            let state = read(n)?;
            hart.restore_state(&mut Reader::new(&state))?;
        }

        // RAM: clear what the machine loaded at start-up, then apply the saved pages.
        let memory = self.bus.memory();
        for &(addr, len) in &self.loaded {
            memory.write(addr, &vec![0u8; len as usize]).map_err(|e| e.to_string())?;
        }
        let base = self.bus.ram_base();
        loop {
            let index = u64_le(read(8)?);
            if index == u64::MAX {
                break;
            }
            let page = read(PAGE)?;
            memory.write(base + index * PAGE as u64, &page).map_err(|e| e.to_string())?;
        }

        let mut rest = Vec::new();
        input.read_to_end(&mut rest).map_err(|e| e.to_string())?;
        let mut r = Reader::new(&rest);
        self.clint.restore(&mut r)?;
        self.rtc.restore(&mut r)?;
        self.uart.restore(&mut r)?;
        self.pci.restore(&mut r)?;
        self.plic.restore(&mut r)?;
        if !r.done() {
            return Err(format!("{}: unexpected data at the end", path.display()));
        }
        self.clock.set(mtime);
        for hart in &mut self.harts {
            hart.update_timer_interrupts();
        }
        Ok(())
    }
}
