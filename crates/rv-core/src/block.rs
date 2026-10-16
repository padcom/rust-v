//! Decoded-instruction cache.
//!
//! Instructions are decoded once into basic blocks keyed by their host RAM address, so the
//! cache is shared by every virtual alias of a physical page and is immune to `satp` changes.
//! A block ends after any instruction that may change control flow, privilege, translation or
//! interrupt state, so the interpreter only needs to re-check those between blocks. Staleness
//! is detected through [`crate::code::CodePages`] generations.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use rv_isa::{Insn, Op, decode};

use crate::hart::Priv;
use crate::jit::JitCode;
use crate::mem::{PAGE_SIZE, host_load};

pub(crate) const MAX_BLOCK_INSNS: usize = 64;
/// The cache is simply dropped when it grows past this many blocks.
const MAX_BLOCKS: usize = 1 << 16;

pub(crate) struct Block {
    pub insns: Box<[Insn]>,
    /// Code generation of the block's page when it was decoded.
    pub generation: u32,
    /// Times the block has been interpreted; drives compilation.
    pub hits: u32,
    /// Native code, compiled for data accesses at the given privilege.
    pub jit: Option<(JitCode, Priv)>,
    /// Compilation was attempted and failed (unsupported instructions).
    pub jit_failed: bool,
    /// A background compilation is in flight.
    pub jit_pending: bool,
    /// The hart's FENCE.I epoch at which the block was last known to match memory.
    pub fence_epoch: u32,
}

impl Block {
    pub fn new(insns: Box<[Insn]>, generation: u32, fence_epoch: u32) -> Self {
        Block { insns, generation, hits: 0, jit: None, jit_failed: false, jit_pending: false, fence_epoch }
    }

    /// Whether the instruction bits at `start` still match the decoded block.
    ///
    /// # Safety
    /// `start` must point into guest RAM, with the whole block inside one page.
    pub unsafe fn matches_memory(&self, start: *const u8) -> bool {
        let mut off = 0;
        self.insns.iter().all(|insn| {
            // SAFETY: the block was decoded from this range.
            let raw = unsafe {
                if insn.len == 2 {
                    host_load(start.add(off), 2) as u32
                } else {
                    host_load(start.add(off), 2) as u32 | (host_load(start.add(off + 2), 2) as u32) << 16
                }
            };
            off += insn.len as usize;
            raw == insn.raw
        })
    }
}

/// Multiplicative hash for pointer keys (SipHash is needlessly slow here).
#[derive(Default)]
pub(crate) struct PtrHasher(u64);

impl Hasher for PtrHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }

    fn write_usize(&mut self, n: usize) {
        self.0 = (n as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[derive(Default)]
pub(crate) struct BlockCache {
    map: HashMap<usize, u32, BuildHasherDefault<PtrHasher>>,
    blocks: Vec<Block>,
    /// Host address of each block, by index.
    hosts: Vec<usize>,
}

impl BlockCache {
    #[inline]
    pub fn get(&self, host: *const u8) -> Option<u32> {
        self.map.get(&(host as usize)).copied()
    }

    #[inline(always)]
    pub fn block(&self, idx: u32) -> &Block {
        &self.blocks[idx as usize]
    }

    #[inline(always)]
    pub fn host(&self, idx: u32) -> *const u8 {
        self.hosts[idx as usize] as *const u8
    }

    #[inline(always)]
    pub fn block_mut(&mut self, idx: u32) -> &mut Block {
        &mut self.blocks[idx as usize]
    }

    pub fn insert(&mut self, host: *const u8, block: Block) -> u32 {
        if let Some(&idx) = self.map.get(&(host as usize)) {
            self.blocks[idx as usize] = block;
            return idx;
        }
        if self.blocks.len() >= MAX_BLOCKS {
            self.clear();
        }
        let idx = self.blocks.len() as u32;
        self.blocks.push(block);
        self.hosts.push(host as usize);
        self.map.insert(host as usize, idx);
        idx
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.blocks.clear();
        self.hosts.clear();
    }
}

/// Whether execution must leave the block after `op`.
pub(crate) fn ends_block(op: Op) -> bool {
    matches!(
        op,
        Op::Illegal
            | Op::Jal
            | Op::Jalr
            | Op::Beq
            | Op::Bne
            | Op::Blt
            | Op::Bge
            | Op::Bltu
            | Op::Bgeu
            | Op::Ecall
            | Op::Ebreak
            | Op::Mret
            | Op::Sret
            | Op::Wfi
            | Op::SfenceVma
            | Op::SinvalVma
            | Op::SfenceWInval
            | Op::SfenceInvalIr
            | Op::FenceI
            | Op::Csrrw
            | Op::Csrrs
            | Op::Csrrc
            | Op::Csrrwi
            | Op::Csrrsi
            | Op::Csrrci
    )
}

/// Decodes the block starting at host address `start`, never reading past the end of its page.
/// Returns an empty list if the first instruction straddles the page boundary.
///
/// # Safety
/// `start` must point into guest RAM, at least 2 bytes before the end of a page.
pub(crate) unsafe fn decode_block(start: *const u8) -> Vec<Insn> {
    let mut insns = Vec::new();
    let room = PAGE_SIZE as usize - (start as usize % PAGE_SIZE as usize);
    let mut off = 0;
    while insns.len() < MAX_BLOCK_INSNS && off + 2 <= room {
        // SAFETY: within the page, which is RAM by contract.
        let low = unsafe { host_load(start.add(off), 2) } as u32;
        let raw = if low & 3 == 3 {
            if off + 4 > room {
                break;
            }
            // SAFETY: as above.
            low | (unsafe { host_load(start.add(off + 2), 2) } as u32) << 16
        } else {
            low
        };
        let insn = decode(raw);
        off += insn.len as usize;
        insns.push(insn);
        if ends_block(insn.op) {
            break;
        }
    }
    insns
}
