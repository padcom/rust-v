//! Byte-level state serialisation for machine snapshots, and the hart's part of it.
//!
//! Snapshots are only read back by the same build (the file records the version), so the
//! format is plain little-endian fields in a fixed order.

use std::sync::atomic::Ordering;

use crate::hart::{Csrs, Hart, Priv, Reservation};

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Length-prefixed bytes.
    pub fn bytes(&mut self, v: &[u8]) {
        self.u64(v.len() as u64);
        self.buf.extend_from_slice(v);
    }
    pub fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
}

pub struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

pub type Result<T> = std::result::Result<T, String>;

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let b = self.data.get(self.at..self.at + n).ok_or("snapshot is truncated")?;
        self.at += n;
        Ok(b)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn u128(&mut self) -> Result<u128> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u64()? as usize;
        self.take(n)
    }
    pub fn string(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| "bad string in snapshot".to_string())
    }
    pub fn done(&self) -> bool {
        self.at == self.data.len()
    }
}

/// `Csrs` is `repr(C)` and made only of `u64`s, so it is saved as such.
const CSR_WORDS: usize = size_of::<Csrs>() / 8;
const _: () = assert!(size_of::<Csrs>().is_multiple_of(8));

impl Hart {
    /// Architectural state (caches such as the TLB and compiled code are not saved).
    pub fn save_state(&self, w: &mut Writer) {
        let s = &*self.state;
        for &x in &s.x {
            w.u64(x);
        }
        w.u64(s.pc);
        for &f in &s.f {
            w.u64(f);
        }
        w.bytes(&s.v[..32 * self.vlenb()]);
        w.u8(s.privilege as u8);
        // SAFETY: `Csrs` is `repr(C)` and consists of `u64`s only.
        let words = unsafe { std::slice::from_raw_parts(&s.csr as *const Csrs as *const u64, CSR_WORDS) };
        for &v in words {
            w.u64(v);
        }
        w.bool(self.wfi);
        w.bool(self.suppress_instret);
        match self.reservation {
            Some(r) => {
                w.bool(true);
                w.u64(r.paddr);
                w.u8(r.size);
                w.u64(r.value);
            }
            None => w.bool(false),
        }
        w.u64(self.shared.mip.load(Ordering::SeqCst));
        w.bytes(&self.pm_len);
    }

    /// Restores what [`Hart::save_state`] wrote, and drops every cached translation and block.
    pub fn restore_state(&mut self, r: &mut Reader) -> Result<()> {
        let vlenb = self.vlenb();
        let s = &mut *self.state;
        for x in &mut s.x {
            *x = r.u64()?;
        }
        s.pc = r.u64()?;
        for f in &mut s.f {
            *f = r.u64()?;
        }
        let v = r.bytes()?;
        if v.len() != 32 * vlenb {
            return Err("snapshot was taken with a different VLEN".into());
        }
        s.v[..v.len()].copy_from_slice(v);
        s.privilege = Priv::from_bits(r.u8()? as u64);
        // SAFETY: as in `save_state`.
        let words = unsafe { std::slice::from_raw_parts_mut(&mut s.csr as *mut Csrs as *mut u64, CSR_WORDS) };
        for v in words {
            *v = r.u64()?;
        }
        self.wfi = r.bool()?;
        self.suppress_instret = r.bool()?;
        self.reservation = if r.bool()? {
            Some(Reservation { paddr: r.u64()?, size: r.u8()?, value: r.u64()? })
        } else {
            None
        };
        self.shared.mip.store(r.u64()?, Ordering::SeqCst);
        let pm = r.bytes()?;
        if pm.len() != self.pm_len.len() {
            return Err("bad pointer-masking state in snapshot".into());
        }
        self.pm_len.copy_from_slice(pm);
        self.forget_caches();
        Ok(())
    }

    /// Drops the TLB, decoded blocks and references to compiled code.
    pub fn forget_caches(&mut self) {
        self.state.tlb.flush();
        self.flush_jit_for_restore();
        self.blocks.clear();
    }
}
