//! The reference interpreter.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use rv_isa::{Insn, Op, decode};

use crate::block::{Block, Region, RegionBlock, decode_block, decode_region};
use crate::csr::status;
use crate::hart::{Hart, Priv, Reservation};
use crate::jit::{
    BlockInfo, ChainSlot, CodeRef, Compiled, Enter, HOOK_REDIRECT, JitCode, JumpCacheEntry, jump_cache_index,
};
use crate::mem::PAGE_SIZE;
use crate::mmu::Access;
use crate::trap::{Exception, cause};

/// Cache-block size for Zicbom/Zicboz/Zicbop (Zic64b).
pub const CACHE_BLOCK: u64 = 64;

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
        // Only link chain slots within one run: the caller may change pc in between.
        self.state.jit.chain_from = 0;
        self.state.jit.fence_epoch = self.bus.code().fence_epoch();
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
            executed += self.run_block(budget - executed);
        }
        Exit::Budget
    }

    /// Executes the block at `pc` (and, when compiled, whatever it chains to, within `budget`).
    /// Returns the number of instructions attempted.
    fn run_block(&mut self, budget: u64) -> u64 {
        let block = match self.lookup_block() {
            Ok(Some(b)) => b,
            // An instruction straddling a page boundary: execute it on its own. The block that
            // follows it is not the last exit's successor, so drop the pending chain link.
            Ok(None) => {
                self.state.jit.chain_from = 0;
                self.step_one();
                return 1;
            }
            Err(e) => {
                self.take_trap(e.cause, e.tval);
                return 1;
            }
        };
        // Compiled code inlines the TLB lookup without pointer masking.
        if self.compiler.is_some() && self.pm_len[crate::mmu::tlb_index(self.data_priv())] == 0 {
            match self.compiler.as_mut().unwrap().enter() {
                Enter::Go => {
                    let ran = self.compiled_code(block).map(|code| {
                        let host = self.blocks.host(block);
                        let page = self.bus.ram_page_of(host);
                        self.link_chain(page, code);
                        self.fill_jump_cache(block, host, page, code);
                        let n = self.run_compiled(code, budget);
                        let hot = std::mem::take(&mut self.state.jit.tierup);
                        if hot != 0 {
                            self.tier_up(hot as *const u8);
                        }
                        n
                    });
                    self.compiler.as_mut().unwrap().leave();
                    if let Some(n) = ran {
                        return n;
                    }
                }
                // The shared code cache was emptied: forget all compiled code we know of.
                Enter::Stale => self.flush_jit(),
                // It is being emptied right now; interpret meanwhile.
                Enter::Busy => {}
            }
        }
        self.state.jit.chain_from = 0;
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
            if self.blocks.block(idx).fence_epoch == self.state.jit.fence_epoch {
                return Ok(Some(idx));
            }
            // A FENCE.I happened since the block was last validated. Other harts may have
            // written the page through store-TLB entries that predate it becoming a code
            // page, so compare the instruction bits.
            // SAFETY: the block was decoded from `host`, within one page of guest RAM.
            if unsafe { self.blocks.block(idx).matches_memory(host) } {
                self.blocks.block_mut(idx).fence_epoch = self.state.jit.fence_epoch;
                return Ok(Some(idx));
            }
            // Stale: make everything cached for this page stale everywhere (other harts and
            // the shared compiled-code table key their copies by the page generation).
            self.bus.code().invalidate_page(page);
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
        Ok(Some(
            self.blocks
                .insert(host, Block::new(insns.into_boxed_slice(), generation, self.state.jit.fence_epoch)),
        ))
    }

    /// Makes `code` reachable from compiled indirect jumps.
    fn fill_jump_cache(&mut self, block: u32, host: *const u8, page: usize, code: JitCode) {
        let b = self.blocks.block(block);
        self.state.jit.jump_cache[jump_cache_index(host as u64)] = JumpCacheEntry {
            host: host as u64,
            code,
            page_generation: self.bus.code().generation_ptr(page) as u64,
            generation: b.generation,
            epoch: self.state.jit.fence_epoch,
            data_priv: self.state.privilege as u32,
            _pad: 0,
        };
    }

    /// Compiled code for `block` at the current data privilege, compiling it once it is hot.
    /// Code is only compiled and run while instruction fetch and data accesses use the same
    /// privilege (i.e. not under MPRV), which compiled indirect jumps rely on.
    fn compiled_code(&mut self, block: u32) -> Option<JitCode> {
        let data_priv = self.data_priv();
        if data_priv != self.state.privilege {
            return None;
        }
        if self.jit_pending > 0 {
            self.install_compiled();
        }
        let b = self.blocks.block_mut(block);
        match b.jit {
            Some((code, p)) if p == data_priv => return Some(code),
            Some(_) => return None,
            None if b.jit_failed || b.jit_pending => return None,
            None => {}
        }
        b.hits += 1;
        if b.hits < self.jit_threshold {
            return None;
        }
        let insns = b.insns.clone();
        let generation = b.generation;
        let host = self.blocks.host(block);
        let entry = RegionBlock { offset: (host as u64 & (PAGE_SIZE - 1)) as u16, insns };
        let page = self.bus.ram_page_of(host);
        // First tier: the block alone.
        let region = std::sync::Arc::new(Region { blocks: vec![entry] });
        let info = BlockInfo {
            host: host as u64,
            data_priv,
            page_generation: self.bus.code().generation_ptr(page),
            generation,
            page,
            cache_epoch: self.jit_cache_epoch,
            vlenb: self.vlenb() as u32,
            vtype: self.state.csr.vtype,
            tier: 1,
        };
        let result = self.compiler.as_mut().unwrap().compile(&region, info);
        let b = self.blocks.block_mut(block);
        match result {
            Compiled::Code(code) => {
                b.jit = Some((code.code, data_priv));
                b.tier = 1;
                b.hooks = code.hooks;
                Some(code.code)
            }
            Compiled::Unsupported => {
                b.jit_failed = true;
                None
            }
            Compiled::Pending => {
                b.jit_pending = true;
                self.jit_pending += 1;
                None
            }
            Compiled::CacheFull => {
                self.flush_jit();
                None
            }
        }
    }

    /// Drops every reference to compiled code and empties the code cache.
    fn flush_jit(&mut self) {
        for b in self.blocks.blocks_mut() {
            b.jit = None;
            b.region = None;
            b.tier = 0;
            b.hooks = 0;
            b.jit_pending = false;
            b.jit_failed = false;
            b.hits = 0;
        }
        self.state.jit.jump_cache.fill(Default::default());
        self.state.jit.chain_from = 0;
        self.jit_cache_epoch = self.jit_cache_epoch.wrapping_add(1);
        if let Some(c) = self.compiler.as_mut() {
            c.reset();
        }
    }

    /// Installs finished background compilations into blocks that are still current.
    fn install_compiled(&mut self) {
        while let Some((info, result)) = self.compiler.as_mut().and_then(|c| c.poll()) {
            self.jit_pending -= 1;
            if info.cache_epoch != self.jit_cache_epoch {
                continue;
            }
            if result == Compiled::CacheFull {
                self.flush_jit();
                continue;
            }
            let Some(idx) = self.blocks.get(info.host as *const u8) else { continue };
            let b = self.blocks.block_mut(idx);
            // The block may have been replaced (new generation) since the request was made.
            if !b.jit_pending || b.generation != info.generation {
                continue;
            }
            b.jit_pending = false;
            match (info.tier, result) {
                (1, Compiled::Code(code)) => {
                    b.jit = Some((code.code, info.data_priv));
                    b.tier = 1;
                    b.hooks = code.hooks;
                }
                (_, Compiled::Code(code)) => Self::install_tier2(b, code, info.data_priv),
                // Keep the first-tier code if the region cannot be compiled.
                (2, _) => b.tier = 2,
                _ => b.jit_failed = true,
            }
        }
    }

    /// Requests second-tier code for the first-tier block at `host`, whose execution counter
    /// ran out.
    fn tier_up(&mut self, host: *const u8) {
        let Some(idx) = self.blocks.get(host) else { return };
        let b = self.blocks.block(idx);
        let Some((_, data_priv)) = b.jit else { return };
        if b.tier != 1 || b.jit_pending || data_priv != self.data_priv() {
            return;
        }
        let entry = RegionBlock { offset: (host as u64 & (PAGE_SIZE - 1)) as u16, insns: b.insns.clone() };
        let generation = b.generation;
        let page = self.bus.ram_page_of(host);
        let page_host = (host as usize & !(PAGE_SIZE as usize - 1)) as *const u8;
        // SAFETY: the block was decoded from this page of guest RAM.
        let region = std::sync::Arc::new(unsafe { decode_region(page_host, entry) });
        let info = BlockInfo {
            host: host as u64,
            data_priv,
            page_generation: self.bus.code().generation_ptr(page),
            generation,
            page,
            cache_epoch: self.jit_cache_epoch,
            vlenb: self.vlenb() as u32,
            vtype: self.state.csr.vtype,
            tier: 2,
        };
        self.blocks.block_mut(idx).region = Some(region.clone());
        let result = self.compiler.as_mut().unwrap().compile(&region, info);
        let b = self.blocks.block_mut(idx);
        match result {
            Compiled::Code(code) => Self::install_tier2(b, code, data_priv),
            Compiled::Pending => {
                b.jit_pending = true;
                self.jit_pending += 1;
            }
            Compiled::Unsupported => b.tier = 2,
            Compiled::CacheFull => self.flush_jit(),
        }
    }

    /// Switches a block to its second-tier code, and points its first-tier code there too
    /// (chain links and jump-cache entries elsewhere still lead to it).
    fn install_tier2(b: &mut Block, code: CodeRef, data_priv: crate::hart::Priv) {
        if b.hooks != 0 {
            // SAFETY: the hooks belong to the block's first-tier code, which is still in the
            // code cache (it is only emptied after every hart dropped its references).
            unsafe {
                AtomicU64::from_ptr((b.hooks + HOOK_REDIRECT) as *mut u64).store(code.code, Ordering::Release)
            };
        }
        b.jit = Some((code.code, data_priv));
        b.tier = 2;
        b.hooks = 0;
    }

    /// If the last compiled block left through an unlinked chain slot and `code` is in the
    /// same page, link the slot so that next time it jumps straight here.
    fn link_chain(&mut self, page: usize, code: JitCode) {
        let slot = std::mem::take(&mut self.state.jit.chain_from);
        if slot == 0 {
            return;
        }
        let slot = slot as *mut ChainSlot;
        // SAFETY: chain slots live as long as the code that refers to them, which this hart
        // still references. Slots may be shared between harts, so they are written atomically
        // (the epoch first: a reader that sees the new target with the old epoch does not chain).
        unsafe {
            if (*slot).page as usize == page {
                AtomicU32::from_ptr(&raw mut (*slot).epoch)
                    .store(self.state.jit.fence_epoch, Ordering::Relaxed);
                AtomicU64::from_ptr(&raw mut (*slot).target).store(code, Ordering::Release);
            }
        }
    }

    #[inline(always)]
    fn retire(&mut self) {
        if !std::mem::take(&mut self.suppress_instret) {
            self.state.csr.minstret = self.state.csr.minstret.wrapping_add(1);
        }
        self.state.csr.mcycle = self.state.csr.mcycle.wrapping_add(1);
    }

    /// Executes one instruction, or takes one pending interrupt, bypassing the block cache.
    /// Does nothing while waiting in WFI with no interrupt pending.
    pub fn step(&mut self) {
        if self.wfi {
            if self.shared.mip.load(Ordering::Relaxed) & self.state.csr.mie == 0 {
                return;
            }
            self.wfi = false;
        }
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

            Op::Sh1add => self.set_x(rd, rs2.wrapping_add(rs1 << 1)),
            Op::Sh2add => self.set_x(rd, rs2.wrapping_add(rs1 << 2)),
            Op::Sh3add => self.set_x(rd, rs2.wrapping_add(rs1 << 3)),
            Op::AddUw => self.set_x(rd, rs2.wrapping_add(rs1 as u32 as u64)),
            Op::Sh1addUw => self.set_x(rd, rs2.wrapping_add((rs1 as u32 as u64) << 1)),
            Op::Sh2addUw => self.set_x(rd, rs2.wrapping_add((rs1 as u32 as u64) << 2)),
            Op::Sh3addUw => self.set_x(rd, rs2.wrapping_add((rs1 as u32 as u64) << 3)),
            Op::SlliUw => self.set_x(rd, (rs1 as u32 as u64) << (imm & 63)),
            Op::Andn => self.set_x(rd, rs1 & !rs2),
            Op::Orn => self.set_x(rd, rs1 | !rs2),
            Op::Xnor => self.set_x(rd, !(rs1 ^ rs2)),
            Op::Clz => self.set_x(rd, rs1.leading_zeros() as u64),
            Op::Clzw => self.set_x(rd, (rs1 as u32).leading_zeros() as u64),
            Op::Ctz => self.set_x(rd, rs1.trailing_zeros() as u64),
            Op::Ctzw => self.set_x(rd, (rs1 as u32).trailing_zeros() as u64),
            Op::Cpop => self.set_x(rd, rs1.count_ones() as u64),
            Op::Cpopw => self.set_x(rd, (rs1 as u32).count_ones() as u64),
            Op::Max => self.set_x(rd, (rs1 as i64).max(rs2 as i64) as u64),
            Op::Maxu => self.set_x(rd, rs1.max(rs2)),
            Op::Min => self.set_x(rd, (rs1 as i64).min(rs2 as i64) as u64),
            Op::Minu => self.set_x(rd, rs1.min(rs2)),
            Op::SextB => self.set_x(rd, rs1 as i8 as u64),
            Op::SextH => self.set_x(rd, rs1 as i16 as u64),
            Op::ZextH => self.set_x(rd, rs1 as u16 as u64),
            Op::Rol => self.set_x(rd, rs1.rotate_left((rs2 & 63) as u32)),
            Op::Ror => self.set_x(rd, rs1.rotate_right((rs2 & 63) as u32)),
            Op::Rori => self.set_x(rd, rs1.rotate_right((imm & 63) as u32)),
            Op::Rolw => self.set_x(rd, (rs1 as u32).rotate_left((rs2 & 31) as u32) as i32 as u64),
            Op::Rorw => self.set_x(rd, (rs1 as u32).rotate_right((rs2 & 31) as u32) as i32 as u64),
            Op::Roriw => self.set_x(rd, (rs1 as u32).rotate_right((imm & 31) as u32) as i32 as u64),
            Op::OrcB => {
                let v = (0..8).fold(0u64, |acc, i| {
                    if (rs1 >> (8 * i)) & 0xff != 0 { acc | (0xff << (8 * i)) } else { acc }
                });
                self.set_x(rd, v);
            }
            Op::Rev8 => self.set_x(rd, rs1.swap_bytes()),
            Op::Bclr => self.set_x(rd, rs1 & !(1 << (rs2 & 63))),
            Op::Bclri => self.set_x(rd, rs1 & !(1 << (imm & 63))),
            Op::Bext => self.set_x(rd, (rs1 >> (rs2 & 63)) & 1),
            Op::Bexti => self.set_x(rd, (rs1 >> (imm & 63)) & 1),
            Op::Binv => self.set_x(rd, rs1 ^ (1 << (rs2 & 63))),
            Op::Binvi => self.set_x(rd, rs1 ^ (1 << (imm & 63))),
            Op::Bset => self.set_x(rd, rs1 | (1 << (rs2 & 63))),
            Op::Bseti => self.set_x(rd, rs1 | (1 << (imm & 63))),
            Op::CzeroEqz => self.set_x(rd, if rs2 == 0 { 0 } else { rs1 }),
            Op::CzeroNez => self.set_x(rd, if rs2 != 0 { 0 } else { rs1 }),
            Op::MopR | Op::MopRr => self.set_x(rd, 0),
            // Waiting is optional: completing immediately is a permitted spurious wake-up.
            Op::WrsNto | Op::WrsSto => {}
            Op::CboInval | Op::CboClean | Op::CboFlush | Op::CboZero => self.exec_cbo(insn, rs1)?,
            Op::SinvalVma => {
                let p = self.state.privilege;
                if p == Priv::User || (p == Priv::Supervisor && self.state.csr.mstatus & status::TVM != 0) {
                    return Err(Exception::illegal(insn.raw));
                }
                self.state.tlb.flush();
            }
            Op::SfenceWInval | Op::SfenceInvalIr => {
                if self.state.privilege == Priv::User {
                    return Err(Exception::illegal(insn.raw));
                }
            }

            Op::Fence => fence(Ordering::SeqCst),
            // Writes through the bus or this hart's TLB bump code-page generations. Writes by
            // other harts may not have, so cached blocks are re-checked lazily against memory.
            Op::FenceI => {
                fence(Ordering::SeqCst);
                self.state.jit.fence_epoch = self.bus.code().new_fence_epoch();
            }

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

            Op::Vsetvli | Op::Vsetivli | Op::Vsetvl | Op::Vop | Op::Vload | Op::Vstore => {
                self.execute_vector(insn)?
            }
            Op::Flh | Op::Flw | Op::Fld => self.fp_load(insn, rs1.wrapping_add(imm))?,
            Op::Fsh | Op::Fsw | Op::Fsd => self.fp_store(insn, rs1.wrapping_add(imm))?,
            _ => self.execute_fp(insn)?,
        }
        self.state.pc = next;
        Ok(())
    }

    /// Zicbom/Zicboz. Caches are not modelled, so only permissions and `cbo.zero` matter.
    fn exec_cbo(&mut self, insn: &Insn, addr: u64) -> Result<(), Exception> {
        use crate::csr::envcfg;
        let (menv, senv) = (self.state.csr.menvcfg, self.state.csr.senvcfg);
        let enabled = |field: u64| match self.state.privilege {
            Priv::Machine => true,
            Priv::Supervisor => menv & field != 0,
            Priv::User => menv & senv & field != 0,
        };
        let allowed = match insn.op {
            Op::CboZero => enabled(envcfg::CBZE),
            Op::CboClean | Op::CboFlush => enabled(envcfg::CBCFE),
            // CBIE = 00 makes cbo.inval illegal; 01 and 11 both allow it.
            _ => enabled(envcfg::CBIE_FLUSH),
        };
        if !allowed {
            return Err(Exception::illegal(insn.raw));
        }
        let p = self.data_priv();
        let addr = self.mask_pointer(addr, p);
        let block = addr & !(CACHE_BLOCK - 1);
        if insn.op == Op::CboZero {
            let paddr = self.translate(block, Access::Store, p).map_err(|e| Exception::new(e.cause, addr))?;
            for i in 0..CACHE_BLOCK / 8 {
                if !self.bus.write(paddr + 8 * i, 8, 0) {
                    return Err(Exception::new(cause::STORE_ACCESS, addr));
                }
            }
            return Ok(());
        }
        // Permitted whenever a load or a store would be; faults are reported as stores.
        let paddr = self
            .translate(block, Access::Load, p)
            .or_else(|_| self.translate(block, Access::Store, p))
            .map_err(|_| Exception::new(cause::STORE_PAGE_FAULT, addr))?;
        if self.bus.read(paddr, 1).is_none() {
            return Err(Exception::new(cause::STORE_ACCESS, addr));
        }
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
