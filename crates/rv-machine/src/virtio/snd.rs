//! virtio-snd (§5.14) with one playback stream.
//!
//! Guest buffers are converted to stereo `f32` at the output rate and moved into a short
//! [`AudioRing`] by a pump thread; a buffer is returned to the guest only once its samples are
//! in the ring, so playback runs at the pace the host output (or, without one, a clock thread)
//! drains the ring.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{Activation, Chain, VirtioDevice, device_type, read_config_bytes};

const CONTROLQ: usize = 0;
const EVENTQ: usize = 1;
const TXQ: usize = 2;
const RXQ: usize = 3;

mod code {
    pub const PCM_INFO: u32 = 0x0100;
    pub const PCM_SET_PARAMS: u32 = 0x0101;
    pub const PCM_PREPARE: u32 = 0x0102;
    pub const PCM_RELEASE: u32 = 0x0103;
    pub const PCM_START: u32 = 0x0104;
    pub const PCM_STOP: u32 = 0x0105;
    pub const OK: u32 = 0x8000;
    pub const BAD_MSG: u32 = 0x8001;
    pub const NOT_SUPP: u32 = 0x8002;
}

/// Sample formats (`VIRTIO_SND_PCM_FMT_*`) the stream accepts.
const FMT_U8: u8 = 4;
const FMT_S16: u8 = 5;
const FMT_S32: u8 = 17;
const FMT_FLOAT: u8 = 19;
/// Frame rates (`VIRTIO_SND_PCM_RATE_*`) and their values.
const RATES: [(u8, u32); 7] =
    [(1, 8000), (2, 11025), (3, 16000), (4, 22050), (5, 32000), (6, 44100), (7, 48000)];

/// Samples (stereo, at the output rate) the ring holds before the pump waits: ~40 ms.
const RING_MS: u32 = 40;

/// Stereo `f32` samples on their way to the host's audio output.
pub struct AudioRing {
    state: Mutex<RingState>,
    room: Condvar,
}

struct RingState {
    samples: VecDeque<f32>,
    rate: u32,
    /// A front end is draining the ring (otherwise a clock thread does).
    attached: bool,
}

impl AudioRing {
    fn new() -> Arc<Self> {
        Arc::new(AudioRing {
            state: Mutex::new(RingState { samples: VecDeque::new(), rate: 48000, attached: false }),
            room: Condvar::new(),
        })
    }

    /// Called by the front end before it starts pulling samples at `rate`.
    pub fn attach(&self, rate: u32) {
        let mut s = self.state.lock().unwrap();
        s.rate = rate;
        s.attached = true;
    }

    /// Fills `out` (interleaved stereo) with queued samples, and silence after them.
    pub fn pull(&self, out: &mut [f32]) {
        let mut s = self.state.lock().unwrap();
        let n = out.len().min(s.samples.len());
        for (o, v) in out.iter_mut().zip(s.samples.drain(..n)) {
            *o = v;
        }
        out[n..].fill(0.0);
        self.room.notify_all();
    }

    fn capacity(rate: u32) -> usize {
        2 * (rate * RING_MS / 1000) as usize
    }
}

#[derive(Clone, Copy)]
struct Params {
    channels: u8,
    format: u8,
    rate: u32,
}

#[derive(Default)]
struct StreamState {
    params: Option<Params>,
    running: bool,
    /// Transmit chains not yet played: head and payload.
    pending: VecDeque<(u16, Vec<u8>)>,
    active: Option<Activation>,
}

struct Shared {
    stream: Mutex<StreamState>,
    work: Condvar,
    ring: Arc<AudioRing>,
}

pub struct Sound {
    shared: Arc<Shared>,
}

impl Sound {
    /// The device and the ring its output goes to.
    pub fn new() -> (Self, Arc<AudioRing>) {
        let ring = AudioRing::new();
        let shared = Arc::new(Shared {
            stream: Mutex::new(StreamState::default()),
            work: Condvar::new(),
            ring: ring.clone(),
        });
        let pump = shared.clone();
        std::thread::Builder::new()
            .name("virtio-snd".into())
            .spawn(move || pump_loop(&pump))
            .expect("spawn virtio-snd");
        let clock = ring.clone();
        std::thread::Builder::new()
            .name("virtio-snd-clock".into())
            .spawn(move || clock_loop(&clock))
            .expect("spawn clock");
        (Sound { shared }, ring)
    }

    fn control(&self, req: &[u8]) -> (u32, Vec<u8>) {
        let u32_at = |at: usize| req.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        let mut s = self.shared.stream.lock().unwrap();
        match u32_at(0) {
            code::PCM_INFO => {
                let (start, count, size) = (u32_at(4), u32_at(8), u32_at(12) as usize);
                if start != 0 || count != 1 || size < 32 {
                    return (code::BAD_MSG, vec![]);
                }
                let mut info = vec![0u8; size];
                let formats: u64 = (1 << FMT_U8) | (1 << FMT_S16) | (1 << FMT_S32) | (1 << FMT_FLOAT);
                let rates: u64 = RATES.iter().map(|&(bit, _)| 1u64 << bit).sum();
                info[8..16].copy_from_slice(&formats.to_le_bytes());
                info[16..24].copy_from_slice(&rates.to_le_bytes());
                info[24] = 0; // output
                info[25] = 1; // channels_min
                info[26] = 2; // channels_max
                (code::OK, info)
            }
            code::PCM_SET_PARAMS => {
                if u32_at(4) != 0 {
                    return (code::BAD_MSG, vec![]);
                }
                let (channels, format, rate) = (
                    req.get(20).copied().unwrap_or(0),
                    req.get(21).copied().unwrap_or(0),
                    req.get(22).copied().unwrap_or(0),
                );
                let Some(&(_, rate)) = RATES.iter().find(|&&(bit, _)| bit == rate) else {
                    return (code::NOT_SUPP, vec![]);
                };
                if !(1..=2).contains(&channels) || ![FMT_U8, FMT_S16, FMT_S32, FMT_FLOAT].contains(&format) {
                    return (code::NOT_SUPP, vec![]);
                }
                s.params = Some(Params { channels, format, rate });
                (code::OK, vec![])
            }
            code::PCM_PREPARE => (if s.params.is_some() { code::OK } else { code::BAD_MSG }, vec![]),
            code::PCM_START => {
                s.running = true;
                self.shared.work.notify_all();
                (code::OK, vec![])
            }
            code::PCM_STOP => {
                s.running = false;
                (code::OK, vec![])
            }
            code::PCM_RELEASE => {
                s.running = false;
                // Hand back whatever was not played.
                let pending: Vec<_> = s.pending.drain(..).collect();
                if let Some(a) = s.active.clone() {
                    drop(s);
                    complete(&a, pending.into_iter().map(|(head, _)| head));
                }
                (code::OK, vec![])
            }
            _ => (code::NOT_SUPP, vec![]),
        }
    }
}

/// Returns transmit chains to the guest with an OK status.
fn complete(a: &Activation, heads: impl Iterator<Item = u16>) {
    let mut q = a.queues[TXQ].lock().unwrap();
    let mut any = false;
    for head in heads {
        // The status was written when the buffer was taken (see `tx_payload`).
        if q.push(&a.mem, head, 8).is_err() {
            return;
        }
        any = true;
    }
    if any && q.needs_interrupt(&a.mem).unwrap_or(true) {
        a.irq.queue();
    }
}

/// Decodes one frame's worth of `format` samples into stereo.
fn frames(params: Params, data: &[u8]) -> Vec<(f32, f32)> {
    let width = match params.format {
        FMT_U8 => 1,
        FMT_S16 => 2,
        _ => 4,
    };
    let sample = |b: &[u8]| -> f32 {
        match params.format {
            FMT_U8 => (b[0] as f32 - 128.0) / 128.0,
            FMT_S16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            FMT_S32 => i32::from_le_bytes(b.try_into().unwrap()) as f32 / 2_147_483_648.0,
            _ => f32::from_le_bytes(b.try_into().unwrap()),
        }
    };
    let frame = width * params.channels as usize;
    data.chunks_exact(frame)
        .map(|f| {
            let l = sample(&f[..width]);
            let r = if params.channels == 2 { sample(&f[width..]) } else { l };
            (l, r)
        })
        .collect()
}

fn pump_loop(shared: &Shared) {
    // Position between input frames for resampling, carried across buffers.
    let mut phase = 0.0f64;
    let mut last = (0.0f32, 0.0f32);
    loop {
        let (head, data, params, active) = {
            let mut s = shared.stream.lock().unwrap();
            loop {
                let frozen = s.active.as_ref().is_some_and(|a| a.mem.frozen());
                if s.running && !s.pending.is_empty() && s.params.is_some() && !frozen {
                    break;
                }
                // A snapshot freezes guest memory without notifying: look again shortly.
                s = shared.work.wait_timeout(s, Duration::from_millis(20)).unwrap().0;
            }
            let (head, data) = s.pending.pop_front().unwrap();
            (head, data, s.params.unwrap(), s.active.clone())
        };
        let input = frames(params, &data);
        let out_rate = shared.ring.state.lock().unwrap().rate;
        let step = params.rate as f64 / out_rate as f64;
        let mut out = Vec::with_capacity((input.len() as f64 / step) as usize * 2 + 4);
        // Linear interpolation from the guest rate to the output rate.
        while (phase as usize) < input.len() {
            let i = phase as usize;
            let t = (phase - i as f64) as f32;
            let a = if i == 0 { last } else { input[i - 1] };
            let b = input[i];
            out.push(a.0 + (b.0 - a.0) * t);
            out.push(a.1 + (b.1 - a.1) * t);
            phase += step;
        }
        phase -= input.len() as f64;
        if let Some(&f) = input.last() {
            last = f;
        }
        {
            let mut ring = shared.ring.state.lock().unwrap();
            let cap = AudioRing::capacity(ring.rate);
            while ring.samples.len() + out.len() > cap.max(out.len()) {
                ring = shared.ring.room.wait(ring).unwrap();
            }
            ring.samples.extend(out);
        }
        if let Some(a) = active {
            complete(&a, std::iter::once(head));
        }
    }
}

/// Drains the ring at its nominal rate while no front end does.
fn clock_loop(ring: &AudioRing) {
    let tick = Duration::from_millis(10);
    let mut next = Instant::now();
    let mut scratch = Vec::new();
    loop {
        next += tick;
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        } else {
            next = Instant::now();
        }
        let (attached, rate) = {
            let s = ring.state.lock().unwrap();
            (s.attached, s.rate)
        };
        if attached {
            continue;
        }
        scratch.resize(2 * (rate / 100) as usize, 0.0);
        ring.pull(&mut scratch);
    }
}

impl VirtioDevice for Sound {
    fn device_type(&self) -> u16 {
        device_type::SOUND
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![64, 64, 64, 64]
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        // jacks, streams, chmaps
        let config: Vec<u8> = [0u32, 1, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        read_config_bytes(&config, offset, data);
    }

    fn activate(&mut self, activation: Activation) {
        self.shared.stream.lock().unwrap().active = Some(activation);
    }

    fn notify(&mut self, queue: usize) {
        let Some(a) = self.shared.stream.lock().unwrap().active.clone() else { return };
        match queue {
            CONTROLQ => a.drain(CONTROLQ, |chain| {
                let mut req = vec![0u8; chain.readable_len().min(4096) as usize];
                let n = chain.reader(&a.mem).read(&mut req).unwrap_or(0);
                req.truncate(n);
                let (status, payload) = self.control(&req);
                let mut resp = status.to_le_bytes().to_vec();
                resp.extend_from_slice(&payload);
                chain.writer(&a.mem).write(&resp).unwrap_or(0) as u32
            }),
            TXQ => {
                // Take the buffers now; they are completed as they are played.
                let mut taken = Vec::new();
                {
                    let mut q = a.queues[TXQ].lock().unwrap();
                    while let Ok(Some(chain)) = q.pop(&a.mem) {
                        taken.push(chain);
                    }
                }
                let mut s = self.shared.stream.lock().unwrap();
                for chain in taken {
                    s.pending.push_back((chain.head, tx_payload(&a, &chain)));
                }
                self.shared.work.notify_all();
            }
            // No capture stream and no jack events: buffers are left with the device.
            EVENTQ | RXQ => {}
            _ => {}
        }
    }

    fn reset(&mut self) {
        let mut s = self.shared.stream.lock().unwrap();
        *s = StreamState::default();
    }

    fn pci_class(&self) -> u32 {
        0x04_01_00 // multimedia audio
    }

    fn save_state(&self, w: &mut rv_core::snapshot::Writer) {
        let s = self.shared.stream.lock().unwrap();
        match s.params {
            Some(p) => {
                w.bool(true);
                w.u8(p.channels);
                w.u8(p.format);
                w.u32(p.rate);
            }
            None => w.bool(false),
        }
        w.bool(s.running);
        w.u64(s.pending.len() as u64);
        for (head, data) in &s.pending {
            w.u16(*head);
            w.bytes(data);
        }
    }

    fn restore_state(&mut self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        let mut s = self.shared.stream.lock().unwrap();
        s.params = if r.bool()? {
            Some(Params { channels: r.u8()?, format: r.u8()?, rate: r.u32()? })
        } else {
            None
        };
        s.running = r.bool()?;
        s.pending.clear();
        for _ in 0..r.u64()? {
            let head = r.u16()?;
            s.pending.push_back((head, r.bytes()?.to_vec()));
        }
        self.shared.work.notify_all();
        Ok(())
    }
}

/// Reads a transmit chain's samples (after the 4-byte stream header) and writes its status
/// in advance: OK, with no latency reported.
fn tx_payload(a: &Activation, chain: &Chain) -> Vec<u8> {
    let mut data = vec![0u8; chain.readable_len() as usize];
    let n = chain.reader(&a.mem).read(&mut data).unwrap_or(0);
    data.truncate(n);
    let mut status = [0u8; 8];
    status[0..4].copy_from_slice(&code::OK.to_le_bytes());
    let _ = chain.writer(&a.mem).write(&status);
    if data.len() >= 4 { data.split_off(4) } else { Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_s16_mono_to_stereo() {
        let p = Params { channels: 1, format: FMT_S16, rate: 48000 };
        let f = frames(p, &[0x00, 0x40, 0x00, 0xc0]);
        assert_eq!(f, vec![(0.5, 0.5), (-0.5, -0.5)]);
    }

    #[test]
    fn ring_pulls_with_silence_after() {
        let ring = AudioRing::new();
        ring.state.lock().unwrap().samples.extend([0.25, -0.25]);
        let mut out = [1.0f32; 4];
        ring.pull(&mut out);
        assert_eq!(out, [0.25, -0.25, 0.0, 0.0]);
    }
}
