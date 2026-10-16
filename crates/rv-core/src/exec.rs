//! The reference interpreter.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use rv_isa::{Insn, Op, decode};

use crate::block::{Block, decode_block};
use crate::csr::status;
use crate::hart::{Hart, Priv, Reservation};
use crate::mem::PAGE_SIZE;
use crate::mmu::Access;
use crate::trap::{Exception, cause};

/// Why [`Hart::run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The instruction budget was used up.
    Budget,
    /// The hart is in WFI with no interrupt pending; the caller should wait for one.
    Wfi,
    /// Another thread asked the hart to stop via `exit_request`.
    Requested,
}

#[inline(always)]
const fn sext32(v: u64) -> u64 {
    v as i32 as i64 as u64
}

impl Hart {
    /// Runs up to `budget` instructions (possibly a few more, to finish a block).
    pub fn run(&mut self, budget: u64) -> Exit {
        let mut executed = 0;
        while executed < budget {
            if self.shared.exit_request.load(Ordering::Relaxed) != 0 {
                self.shared.exit_request.store(0, Ordering::Relaxed);
                // Re-read mip after clearing the request so that no raise can be lost.
                fence(Ordering::SeqCst);
                if !self.wfi {
                    return Exit::Requested;
                }
            }
            if self.wfi {
                if self.shared.mip.load(Ordering::Relaxed) & self.state.csr.mie == 0 {
                    return Exit::Wfi;
                }
                self.wfi = false;
            }
            if let Some(irq) = self.pending_interrupt() {
                self.take_trap(irq, 0);
                continue;
            }
            executed += self.run_block();
        }
        Exit::Budget
    }

    /// Executes the block at `pc`. Returns the number of instructions attempted.
    fn run_block(&mut self) -> u64 {
        let block = match self.lookup_block() {
            Ok(Some(b)) => b,
            // An instruction straddling a page boundary: execute it on its own.
            Ok(None) => {
                self.step_one();
                return 1;
            }
            Err(e) => {
                self.take_trap(e.cause, e.tval);
                return 1;
            }
        };
        let len = self.blocks.block(block).insns.len();
        for i in 0..len {
            let insn = self.blocks.block(block).insns[i];
            let pc = self.state.pc;
            match self.execute(&insn) {
                Ok(()) => self.retire(),
                Err(e) => {
                    self.take_trap(e.cause, e.tval);
                    return i as u64 + 1;
                }
            }
            if self.state.pc != pc.wrapping_add(insn.len as u64) {
                return i as u64 + 1;
            }
        }
        len as u64
    }

    /// Finds (decoding if needed) the block starting at `pc`.
    fn lookup_block(&mut self) -> Result<Option<u32>, Exception> {
        let host = self.fetch_ptr(self.state.pc)?;
        let page = self.bus.ram_page_of(host);
        if let Some(idx) = self.blocks.get(host)
            && self.blocks.block(idx).generation == self.bus.code().generation(page)
        {
            return Ok(Some(idx));
        }
        // Mark the page before reading its generation and contents, so that any write from
        // here on is detected.
        if self.bus.code().mark(page) {
            let page_host = (host as usize & !(PAGE_SIZE as usize - 1)) as *const u8;
            self.flush_store_tlb_host_page(page_host);
        }
        let generation = self.bus.code().generation(page);
        // SAFETY: `fetch_ptr` returned a pointer into guest RAM with at least 2 bytes left
        // in the page.
        let insns = unsafe { decode_block(host) };
        if insns.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.blocks.insert(host, Block { insns: insns.into_boxed_slice(), generation })))
    }

    #[inline(always)]
    fn retire(&mut self) {
        if !std::mem::take(&mut self.suppress_instret) {
            self.state.csr.minstret = self.state.csr.minstret.wrapping_add(1);
        }
        self.state.csr.mcycle = self.state.csr.mcycle.wrapping_add(1);
    }

    /// Executes one instruction, or takes one pending interrupt, bypassing the block cache.
    pub fn step(&mut self) {
        if let Some(irq) = self.pending_interrupt() {
            self.wfi = false;
            self.take_trap(irq, 0);
            return;
        }
        self.step_one();
    }

    fn step_one(&mut self) {
        match self.fetch().and_then(|insn| self.execute(&insn)) {
            Ok(()) => self.retire(),
            Err(e) => self.take_trap(e.cause, e.tval),
        }
    }

    /// Fetches and decodes the instruction at `pc`.
    pub fn fetch(&mut self) -> Result<Insn, Exception> {
        let pc = self.state.pc;
        let low = self.fetch16(pc)?;
        if low & 3 != 3 {
            return Ok(decode(low as u32));
        }
        let high = self.fetch16(pc.wrapping_add(2))?;
        Ok(decode(((high as u32) << 16) | low as u32))
    }

    /// Executes `insn` located at the current `pc`, advancing `pc` on success.
    pub fn execute(&mut self, insn: &Insn) -> Result<(), Exception> {
        let pc = self.state.pc;
        let mut next = pc.wrapping_add(insn.len as u64);
        let rs1 = self.x(insn.rs1);
        let rs2 = self.x(insn.rs2);
        let imm = insn.imm as i64 as u64;
        let rd = insn.rd;

        match insn.op {
            Op::Illegal => return Err(Exception::illegal(insn.raw)),

            Op::Lui => self.set_x(rd, imm),
            Op::Auipc => self.set_x(rd, pc.wrapping_add(imm)),
            Op::Jal => {
                self.set_x(rd, next);
                next = pc.wrapping_add(imm);
            }
            Op::Jalr => {
                let target = rs1.wrapping_add(imm) & !1;
                self.set_x(rd, next);
                next = target;
            }
            Op::Beq | Op::Bne | Op::Blt | Op::Bge | Op::Bltu | Op::Bgeu => {
                let taken = match insn.op {
                    Op::Beq => rs1 == rs2,
                    Op::Bne => rs1 != rs2,
                    Op::Blt => (rs1 as i64) < (rs2 as i64),
                    Op::Bge => (rs1 as i64) >= (rs2 as i64),
                    Op::Bltu => rs1 < rs2,
                    _ => rs1 >= rs2,
                };
                if taken {
                    next = pc.wrapping_add(imm);
                }
            }

            Op::Lb | Op::Lh | Op::Lw | Op::Ld | Op::Lbu | Op::Lhu | Op::Lwu => {
                let (size, signed) = match insn.op {
                    Op::Lb => (1, true),
                    Op::Lh => (2, true),
                    Op::Lw => (4, true),
                    Op::Ld => (8, false),
                    Op::Lbu => (1, false),
                    Op::Lhu => (2, false),
                    _ => (4, false),
                };
                let value = self.load(rs1.wrapping_add(imm), size)?;
                let shift = 64 - 8 * size as u32;
                self.set_x(rd, if signed { (((value << shift) as i64) >> shift) as u64 } else { value });
            }
            Op::Sb => self.store(rs1.wrapping_add(imm), 1, rs2)?,
            Op::Sh => self.store(rs1.wrapping_add(imm), 2, rs2)?,
            Op::Sw => self.store(rs1.wrapping_add(imm), 4, rs2)?,
            Op::Sd => self.store(rs1.wrapping_add(imm), 8, rs2)?,

            Op::Addi => self.set_x(rd, rs1.wrapping_add(imm)),
            Op::Slti => self.set_x(rd, ((rs1 as i64) < (imm as i64)) as u64),
            Op::Sltiu => self.set_x(rd, (rs1 < imm) as u64),
            Op::Xori => self.set_x(rd, rs1 ^ imm),
            Op::Ori => self.set_x(rd, rs1 | imm),
            Op::Andi => self.set_x(rd, rs1 & imm),
            Op::Slli => self.set_x(rd, rs1 << (imm & 63)),
            Op::Srli => self.set_x(rd, rs1 >> (imm & 63)),
            Op::Srai => self.set_x(rd, ((rs1 as i64) >> (imm & 63)) as u64),
            Op::Add => self.set_x(rd, rs1.wrapping_add(rs2)),
            Op::Sub => self.set_x(rd, rs1.wrapping_sub(rs2)),
            Op::Sll => self.set_x(rd, rs1 << (rs2 & 63)),
            Op::Slt => self.set_x(rd, ((rs1 as i64) < (rs2 as i64)) as u64),
            Op::Sltu => self.set_x(rd, (rs1 < rs2) as u64),
            Op::Xor => self.set_x(rd, rs1 ^ rs2),
            Op::Srl => self.set_x(rd, rs1 >> (rs2 & 63)),
            Op::Sra => self.set_x(rd, ((rs1 as i64) >> (rs2 & 63)) as u64),
            Op::Or => self.set_x(rd, rs1 | rs2),
            Op::And => self.set_x(rd, rs1 & rs2),
            Op::Addiw => self.set_x(rd, sext32(rs1.wrapping_add(imm))),
            Op::Slliw => self.set_x(rd, sext32(((rs1 as u32) << (imm & 31)) as u64)),
            Op::Srliw => self.set_x(rd, sext32(((rs1 as u32) >> (imm & 31)) as u64)),
            Op::Sraiw => self.set_x(rd, ((rs1 as i32) >> (imm & 31)) as i64 as u64),
            Op::Addw => self.set_x(rd, sext32(rs1.wrapping_add(rs2))),
            Op::Subw => self.set_x(rd, sext32(rs1.wrapping_sub(rs2))),
            Op::Sllw => self.set_x(rd, sext32(((rs1 as u32) << (rs2 & 31)) as u64)),
            Op::Srlw => self.set_x(rd, sext32(((rs1 as u32) >> (rs2 & 31)) as u64)),
            Op::Sraw => self.set_x(rd, ((rs1 as i32) >> (rs2 & 31)) as i64 as u64),

            Op::Mul => self.set_x(rd, rs1.wrapping_mul(rs2)),
            Op::Mulh => self.set_x(rd, ((rs1 as i64 as i128 * rs2 as i64 as i128) >> 64) as u64),
            Op::Mulhsu => self.set_x(rd, ((rs1 as i64 as i128 * rs2 as i128) >> 64) as u64),
            Op::Mulhu => self.set_x(rd, ((rs1 as u128 * rs2 as u128) >> 64) as u64),
            Op::Div => {
                self.set_x(rd, if rs2 == 0 { u64::MAX } else { (rs1 as i64).wrapping_div(rs2 as i64) as u64 })
            }
            Op::Divu => self.set_x(rd, rs1.checked_div(rs2).unwrap_or(u64::MAX)),
            Op::Rem => {
                self.set_x(rd, if rs2 == 0 { rs1 } else { (rs1 as i64).wrapping_rem(rs2 as i64) as u64 })
            }
            Op::Remu => self.set_x(rd, if rs2 == 0 { rs1 } else { rs1 % rs2 }),
            Op::Mulw => self.set_x(rd, sext32(rs1.wrapping_mul(rs2))),
            Op::Divw => {
                let (a, b) = (rs1 as i32, rs2 as i32);
                self.set_x(rd, if b == 0 { u64::MAX } else { a.wrapping_div(b) as i64 as u64 });
            }
            Op::Divuw => {
                let (a, b) = (rs1 as u32, rs2 as u32);
                self.set_x(rd, sext32(a.checked_div(b).unwrap_or(u32::MAX) as u64));
            }
            Op::Remw => {
                let (a, b) = (rs1 as i32, rs2 as i32);
                self.set_x(rd, if b == 0 { a as i64 as u64 } else { a.wrapping_rem(b) as i64 as u64 });
            }
            Op::Remuw => {
                let (a, b) = (rs1 as u32, rs2 as u32);
                self.set_x(rd, sext32(if b == 0 { a } else { a % b } as u64));
            }

            Op::LrW | Op::LrD => {
                let size = if insn.op == Op::LrW { 4 } else { 8 };
                let (ptr, paddr) = self.atomic_target(rs1, size, Access::Load)?;
                // SAFETY: `atomic_target` returns aligned pointers into guest RAM.
                let value = unsafe { load_atomic(ptr, size) };
                self.reservation = Some(Reservation { paddr, size, value });
                self.set_x(rd, if size == 4 { sext32(value) } else { value });
            }
            Op::ScW | Op::ScD => {
                let size = if insn.op == Op::ScW { 4 } else { 8 };
                let (ptr, paddr) = self.atomic_target(rs1, size, Access::Store)?;
                let success = match self.reservation.take() {
                    // SAFETY: as above.
                    Some(r) if r.paddr == paddr && r.size == size => unsafe { cas(ptr, size, r.value, rs2) },
                    _ => false,
                };
                if success {
                    self.bus.code().note_write(self.bus.ram_page_of(ptr));
                }
                self.set_x(rd, !success as u64);
            }
            Op::AmoswapW
            | Op::AmoaddW
            | Op::AmoxorW
            | Op::AmoandW
            | Op::AmoorW
            | Op::AmominW
            | Op::AmomaxW
            | Op::AmominuW
            | Op::AmomaxuW => {
                let (ptr, _) = self.atomic_target(rs1, 4, Access::Store)?;
                // SAFETY: `atomic_target` returns aligned pointers into guest RAM.
                let a = unsafe { &*(ptr as *const AtomicU32) };
                let v = rs2 as u32;
                let old = match insn.op {
                    Op::AmoswapW => a.swap(v, Ordering::SeqCst),
                    Op::AmoaddW => a.fetch_add(v, Ordering::SeqCst),
                    Op::AmoxorW => a.fetch_xor(v, Ordering::SeqCst),
                    Op::AmoandW => a.fetch_and(v, Ordering::SeqCst),
                    Op::AmoorW => a.fetch_or(v, Ordering::SeqCst),
                    Op::AmominuW => a.fetch_min(v, Ordering::SeqCst),
                    Op::AmomaxuW => a.fetch_max(v, Ordering::SeqCst),
                    Op::AmominW => a
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |o| {
                            Some((o as i32).min(v as i32) as u32)
                        })
                        .unwrap(),
                    _ => a
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |o| {
                            Some((o as i32).max(v as i32) as u32)
                        })
                        .unwrap(),
                };
                self.bus.code().note_write(self.bus.ram_page_of(ptr));
                self.set_x(rd, sext32(old as u64));
            }
            Op::AmoswapD
            | Op::AmoaddD
            | Op::AmoxorD
            | Op::AmoandD
            | Op::AmoorD
            | Op::AmominD
            | Op::AmomaxD
            | Op::AmominuD
            | Op::AmomaxuD => {
                let (ptr, _) = self.atomic_target(rs1, 8, Access::Store)?;
                // SAFETY: `atomic_target` returns aligned pointers into guest RAM.
                let a = unsafe { &*(ptr as *const AtomicU64) };
                let old = match insn.op {
                    Op::AmoswapD => a.swap(rs2, Ordering::SeqCst),
                    Op::AmoaddD => a.fetch_add(rs2, Ordering::SeqCst),
                    Op::AmoxorD => a.fetch_xor(rs2, Ordering::SeqCst),
                    Op::AmoandD => a.fetch_and(rs2, Ordering::SeqCst),
                    Op::AmoorD => a.fetch_or(rs2, Ordering::SeqCst),
                    Op::AmominuD => a.fetch_min(rs2, Ordering::SeqCst),
                    Op::AmomaxuD => a.fetch_max(rs2, Ordering::SeqCst),
                    Op::AmominD => a
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |o| {
                            Some((o as i64).min(rs2 as i64) as u64)
                        })
                        .unwrap(),
                    _ => a
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |o| {
                            Some((o as i64).max(rs2 as i64) as u64)
                        })
                        .unwrap(),
                };
                self.bus.code().note_write(self.bus.ram_page_of(ptr));
                self.set_x(rd, old);
            }

            Op::Fence => fence(Ordering::SeqCst),
            // Writes to cached code are detected through code-page generations, so there is
            // nothing to flush; the block simply ends here.
            Op::FenceI => fence(Ordering::SeqCst),

            Op::Csrrw | Op::Csrrs | Op::Csrrc | Op::Csrrwi | Op::Csrrsi | Op::Csrrci => {
                self.exec_csr(insn, rs1)?;
            }

            Op::Ecall => return Err(Exception::new(self.ecall_cause(), 0)),
            Op::Ebreak => return Err(Exception::new(cause::BREAKPOINT, pc)),
            Op::Mret => {
                if self.state.privilege != Priv::Machine {
                    return Err(Exception::illegal(insn.raw));
                }
                self.mret();
                return Ok(());
            }
            Op::Sret => {
                let p = self.state.privilege;
                if p == Priv::User || (p == Priv::Supervisor && self.state.csr.mstatus & status::TSR != 0) {
                    return Err(Exception::illegal(insn.raw));
                }
                self.sret();
                return Ok(());
            }
            Op::Wfi => {
                let p = self.state.privilege;
                if p == Priv::User || (p != Priv::Machine && self.state.csr.mstatus & status::TW != 0) {
                    return Err(Exception::illegal(insn.raw));
                }
                self.wfi = true;
            }
            Op::SfenceVma => {
                let p = self.state.privilege;
                if p == Priv::User || (p == Priv::Supervisor && self.state.csr.mstatus & status::TVM != 0) {
                    return Err(Exception::illegal(insn.raw));
                }
                self.state.tlb.flush();
            }

            // The FPU is not implemented yet: misa.F/D are clear and mstatus.FS is off.
            Op::Flw | Op::Fld | Op::Fsw | Op::Fsd => return Err(Exception::illegal(insn.raw)),
        }
        self.state.pc = next;
        Ok(())
    }

    fn exec_csr(&mut self, insn: &Insn, rs1_value: u64) -> Result<(), Exception> {
        let csr = insn.csr();
        let immediate = matches!(insn.op, Op::Csrrwi | Op::Csrrsi | Op::Csrrci);
        let operand = if immediate { insn.rs1 as u64 } else { rs1_value };
        let (reads, writes) = match insn.op {
            Op::Csrrw | Op::Csrrwi => (insn.rd != 0, true),
            // CSRRS/CSRRC with x0 / zero immediate do not write.
            _ => (true, insn.rs1 != 0),
        };
        self.csr_check(csr, writes, insn.raw)?;
        // Read even when the result is discarded, to reject non-existent CSRs.
        let old = self.csr_read(csr, insn.raw)?;
        if writes {
            let new = match insn.op {
                Op::Csrrw | Op::Csrrwi => operand,
                Op::Csrrs | Op::Csrrsi => old | operand,
                _ => old & !operand,
            };
            self.csr_write(csr, new);
        }
        if reads {
            self.set_x(insn.rd, old);
        }
        Ok(())
    }
}

/// # Safety
/// `ptr` must be a naturally aligned pointer into guest RAM.
unsafe fn load_atomic(ptr: *mut u8, size: u8) -> u64 {
    // SAFETY: by contract.
    unsafe {
        if size == 4 {
            (*(ptr as *const AtomicU32)).load(Ordering::SeqCst) as u64
        } else {
            (*(ptr as *const AtomicU64)).load(Ordering::SeqCst)
        }
    }
}

/// # Safety
/// `ptr` must be a naturally aligned pointer into guest RAM.
unsafe fn cas(ptr: *mut u8, size: u8, expected: u64, new: u64) -> bool {
    // SAFETY: by contract.
    unsafe {
        if size == 4 {
            (*(ptr as *const AtomicU32))
                .compare_exchange(expected as u32, new as u32, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        } else {
            (*(ptr as *const AtomicU64))
                .compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        }
    }
}
