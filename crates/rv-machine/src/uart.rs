//! NS16550A UART. Transmission is instantaneous; received bytes are queued by the host side.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::plic::Plic;

pub const SIZE: u64 = 0x100;

const IER_RDI: u8 = 0x01; // received data available
const IER_THRI: u8 = 0x02; // transmitter holding register empty
const LCR_DLAB: u8 = 0x80;
const LSR_DR: u8 = 0x01;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;
const MCR_LOOP: u8 = 0x10;
const IIR_NONE: u8 = 0x01;
const IIR_THRI: u8 = 0x02;
const IIR_RDI: u8 = 0x04;
const IIR_FIFO_ENABLED: u8 = 0xc0;
const FCR_FIFO_ENABLE: u8 = 0x01;
const FCR_CLEAR_RX: u8 = 0x02;

struct State {
    ier: u8,
    lcr: u8,
    mcr: u8,
    fcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    rx: VecDeque<u8>,
    /// Pending "transmitter empty" interrupt; cleared by reading IIR or writing THR.
    thr_interrupt: bool,
    output: Box<dyn Write + Send>,
}

pub struct Uart {
    state: Mutex<State>,
    plic: Arc<Plic>,
    irq: u32,
}

impl Uart {
    pub fn new(output: Box<dyn Write + Send>, plic: Arc<Plic>, irq: u32) -> Self {
        Uart {
            state: Mutex::new(State {
                ier: 0,
                lcr: 0,
                mcr: 0,
                fcr: 0,
                scr: 0,
                dll: 0,
                dlm: 0,
                rx: VecDeque::new(),
                thr_interrupt: false,
                output,
            }),
            plic,
            irq,
        }
    }

    pub fn save(&self, w: &mut rv_core::snapshot::Writer) {
        let s = self.state.lock().unwrap();
        for v in [s.ier, s.lcr, s.mcr, s.fcr, s.scr, s.dll, s.dlm] {
            w.u8(v);
        }
        w.bool(s.thr_interrupt);
        w.bytes(&s.rx.iter().copied().collect::<Vec<u8>>());
    }

    pub fn restore(&self, r: &mut rv_core::snapshot::Reader) -> rv_core::snapshot::Result<()> {
        let mut guard = self.state.lock().unwrap();
        let s = &mut *guard;
        for v in [&mut s.ier, &mut s.lcr, &mut s.mcr, &mut s.fcr, &mut s.scr, &mut s.dll, &mut s.dlm] {
            *v = r.u8()?;
        }
        s.thr_interrupt = r.bool()?;
        s.rx = r.bytes()?.iter().copied().collect();
        Ok(())
    }

    /// Queues bytes typed on the host console.
    pub fn push_input(&self, bytes: &[u8]) {
        let mut s = self.state.lock().unwrap();
        s.rx.extend(bytes);
        self.update_irq(&s);
    }

    fn update_irq(&self, s: &State) {
        let level = (s.ier & IER_RDI != 0 && !s.rx.is_empty()) || (s.ier & IER_THRI != 0 && s.thr_interrupt);
        self.plic.set_level(self.irq, level);
    }
}

impl rv_core::Device for Uart {
    fn read(&self, offset: u64, _size: u8) -> u64 {
        let mut s = self.state.lock().unwrap();
        let dlab = s.lcr & LCR_DLAB != 0;
        let value = match offset {
            0 if dlab => s.dll,
            0 => {
                let b = s.rx.pop_front().unwrap_or(0);
                self.update_irq(&s);
                b
            }
            1 if dlab => s.dlm,
            1 => s.ier,
            2 => {
                let fifo = if s.fcr & FCR_FIFO_ENABLE != 0 { IIR_FIFO_ENABLED } else { 0 };
                let id = if s.ier & IER_RDI != 0 && !s.rx.is_empty() {
                    IIR_RDI
                } else if s.ier & IER_THRI != 0 && s.thr_interrupt {
                    s.thr_interrupt = false;
                    self.update_irq(&s);
                    IIR_THRI
                } else {
                    IIR_NONE
                };
                fifo | id
            }
            3 => s.lcr,
            4 => s.mcr,
            5 => LSR_THRE | LSR_TEMT | if s.rx.is_empty() { 0 } else { LSR_DR },
            // Without loopback, report CTS, DSR and DCD asserted.
            6 if s.mcr & MCR_LOOP != 0 => (s.mcr & 0x0f) << 4,
            6 => 0xb0,
            7 => s.scr,
            _ => 0,
        };
        value as u64
    }

    fn write(&self, offset: u64, _size: u8, value: u64) {
        let mut s = self.state.lock().unwrap();
        let value = value as u8;
        let dlab = s.lcr & LCR_DLAB != 0;
        match offset {
            0 if dlab => s.dll = value,
            0 => {
                if s.mcr & MCR_LOOP != 0 {
                    s.rx.push_back(value);
                } else {
                    let _ = s.output.write_all(&[value]);
                    let _ = s.output.flush();
                }
                // The byte is "sent" immediately, so the holding register is empty again.
                s.thr_interrupt = true;
                self.update_irq(&s);
            }
            1 if dlab => s.dlm = value,
            1 => {
                let enabling_thri = value & IER_THRI != 0 && s.ier & IER_THRI == 0;
                s.ier = value & 0x0f;
                if enabling_thri {
                    s.thr_interrupt = true;
                }
                self.update_irq(&s);
            }
            2 => {
                s.fcr = value;
                if value & FCR_CLEAR_RX != 0 {
                    s.rx.clear();
                    self.update_irq(&s);
                }
            }
            3 => s.lcr = value,
            4 => s.mcr = value & 0x1f,
            7 => s.scr = value,
            _ => {}
        }
    }
}
