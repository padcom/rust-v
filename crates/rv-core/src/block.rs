//! Decoded-instruction cache.
//!
//! Instructions are decoded once into basic blocks keyed by their host RAM address, so the
//! cache is shared by every virtual alias of a physical page and is immune to `satp` changes.
//! A block ends after any instruction that may change control flow, privilege, translation or
//! interrupt state, so the interpreter only needs to re-check those between blocks. Staleness
//! is detected through [`crate::code::CodePages`] generations.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use rv_isa::{Insn, Op, decode};

use crate::hart::Priv;
use crate::jit::JitCode;
use crate::mem::{PAGE_SIZE, host_load};

pub(crate) const MAX_BLOCK_INSNS: usize = 64;
/// The cache is simply dropped when it grows past this many blocks. A desktop distribution
/// touches millions of blocks while booting, so this must be large enough to hold the working
/// set, or hot blocks lose their compiled code and get recompiled over and over.
const MAX_BLOCKS: usize = 1 << 19;

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
    /// The region the compiled code was made from (all of which must still match memory).
    pub region: Option<Arc<Region>>,
    /// Tier of `jit` (see [`crate::jit::BlockInfo::tier`]); 0 before any.
    pub tier: u8,
    /// Hooks of first-tier code (see [`crate::jit::CodeRef`]).
    pub hooks: u64,
}

impl Block {
    pub fn new(insns: Box<[Insn]>, generation: u32, fence_epoch: u32) -> Self {
        Block {
            insns,
            generation,
            hits: 0,
            jit: None,
            jit_failed: false,
            jit_pending: false,
            fence_epoch,
            region: None,
            tier: 0,
            hooks: 0,
        }
    }

    /// Whether the instruction bits at `start` still match the decoded block (and the rest
    /// of its region, if it has one).
    ///
    /// # Safety
    /// `start` must point into guest RAM, with the whole block inside one page.
    pub unsafe fn matches_memory(&self, start: *const u8) -> bool {
        let page = (start as usize & !(PAGE_SIZE as usize - 1)) as *const u8;
        // SAFETY: by contract; region blocks lie in the same page.
        unsafe {
            insns_match(start, &self.insns)
                && self.region.as_ref().is_none_or(|r| {
                    r.blocks.iter().all(|b| insns_match(page.add(b.offset as usize), &b.insns))
                })
        }
    }
}

/// # Safety
/// `start..` must be guest RAM covering the instructions.
unsafe fn insns_match(start: *const u8, insns: &[Insn]) -> bool {
    let mut off = 0;
    insns.iter().all(|insn| {
        // SAFETY: by contract.
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

/// A compilation unit: an entry block and the blocks reachable from it through direct jumps
/// and branches within its page. Compiled together, control stays in native code (and guest
/// registers in host registers) along those edges.
#[derive(Clone, Debug)]
pub struct Region {
    /// The entry block first.
    pub blocks: Vec<RegionBlock>,
}

#[derive(Clone, Debug)]
pub struct RegionBlock {
    /// Byte offset within the page.
    pub offset: u16,
    pub insns: Box<[Insn]>,
}

const MAX_REGION_BLOCKS: usize = 16;
const MAX_REGION_INSNS: usize = 256;

/// Page offsets a block continues at directly: branch and jump targets and the fall-through.
fn static_successors(block: &RegionBlock) -> impl Iterator<Item = i64> {
    let mut at = block.offset as i64;
    let mut last = None;
    for insn in block.insns.iter() {
        last = Some((at, insn));
        at += insn.len as i64;
    }
    let (last_at, last) = last.expect("empty block");
    let (a, b) = match last.op {
        Op::Jal => (Some(last_at + last.imm as i64), None),
        Op::Beq | Op::Bne | Op::Blt | Op::Bge | Op::Bltu | Op::Bgeu => {
            (Some(last_at + last.imm as i64), Some(at))
        }
        // The block was cut short (size limit, or an instruction straddling the page).
        _ if !ends_block_at(last) => (Some(at), None),
        _ => (None, None),
    };
    a.into_iter().chain(b)
}

/// The region for `entry` in the page at `page`: the blocks of loops through `entry` (those
/// reachable from it within the page that lead back to it). That is where keeping control
/// and registers in native code pays; compiling straight-line successors as well would
/// mostly duplicate code that gets compiled on its own anyway.
///
/// # Safety
/// `page` must point to a page of guest RAM.
pub(crate) unsafe fn decode_region(page: *const u8, entry: RegionBlock) -> Region {
    // SAFETY: by contract.
    let mut region = unsafe { reachable_blocks(page, entry) };
    let offsets: Vec<i64> = region.blocks.iter().map(|b| b.offset as i64).collect();
    let successors: Vec<Vec<usize>> = region
        .blocks
        .iter()
        .map(|b| static_successors(b).filter_map(|s| offsets.iter().position(|&o| o == s)).collect())
        .collect();
    // Blocks from which the entry can be reached again.
    let mut loops_back = vec![false; region.blocks.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for (i, succ) in successors.iter().enumerate() {
            if !loops_back[i] && succ.iter().any(|&s| s == 0 || loops_back[s]) {
                loops_back[i] = true;
                changed = true;
            }
        }
    }
    // Also keep the blocks a short distance ahead of the entry (if/else diamonds and the like),
    // which saves exits on the common path without compiling everything reachable.
    let mut depth = vec![usize::MAX; region.blocks.len()];
    depth[0] = 0;
    for _ in 0..FORWARD_DEPTH {
        for i in 0..region.blocks.len() {
            if depth[i] < FORWARD_DEPTH {
                for &s in &successors[i] {
                    depth[s] = depth[s].min(depth[i] + 1);
                }
            }
        }
    }
    let mut i = 0;
    region.blocks.retain(|_| {
        let keep = i == 0 || loops_back[i] || depth[i] <= FORWARD_DEPTH;
        i += 1;
        keep
    });
    region
}

/// How many direct successors deep a region extends ahead of its entry outside loops.
const FORWARD_DEPTH: usize = 2;

/// The blocks reachable from `entry` through direct jumps and branches within its page,
/// within the region size limits.
///
/// # Safety
/// As for [`decode_region`].
unsafe fn reachable_blocks(page: *const u8, entry: RegionBlock) -> Region {
    let mut region = Region { blocks: vec![entry] };
    let mut total = region.blocks[0].insns.len();
    let mut next = 0;
    while next < region.blocks.len() && region.blocks.len() < MAX_REGION_BLOCKS {
        let successors: Vec<i64> = static_successors(&region.blocks[next]).collect();
        next += 1;
        for s in successors {
            let in_page = (0..PAGE_SIZE as i64 - 1).contains(&s) && s % 2 == 0;
            if !in_page || region.blocks.iter().any(|b| b.offset as i64 == s) {
                continue;
            }
            if region.blocks.len() >= MAX_REGION_BLOCKS || total >= MAX_REGION_INSNS {
                return region;
            }
            // SAFETY: within the page, at least 2 bytes before its end.
            let insns = unsafe { decode_block(page.add(s as usize)) };
            if insns.is_empty() {
                continue;
            }
            total += insns.len();
            region.blocks.push(RegionBlock { offset: s as u16, insns: insns.into_boxed_slice() });
        }
    }
    region
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

    pub fn blocks_mut(&mut self) -> impl Iterator<Item = &mut Block> {
        self.blocks.iter_mut()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.blocks.clear();
        self.hosts.clear();
    }
}

/// Whether execution must leave the block after `insn`. CSR accesses that cannot change
/// privilege, translation or interrupt state (see [`benign_csr`]) do not end it.
pub fn ends_block_at(insn: &Insn) -> bool {
    ends_block(insn.op) && !benign_csr(insn)
}

/// CSR accesses that only read, or that write the floating-point and vector status CSRs.
/// Executing them in the middle of a block is safe for both the interpreter and compiled code.
pub fn benign_csr(insn: &Insn) -> bool {
    use crate::csr;
    match insn.op {
        // CSRRS/CSRRC with x0 (or a zero immediate) do not write.
        Op::Csrrs | Op::Csrrc | Op::Csrrsi | Op::Csrrci if insn.rs1 == 0 => true,
        Op::Csrrw | Op::Csrrs | Op::Csrrc | Op::Csrrwi | Op::Csrrsi | Op::Csrrci => {
            matches!(insn.csr(), csr::FFLAGS | csr::FRM | csr::FCSR | csr::VXSAT | csr::VXRM | csr::VCSR)
        }
        _ => false,
    }
}

/// Whether execution must leave the block after `op`, whatever its operands.
pub fn ends_block(op: Op) -> bool {
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
        if ends_block_at(&insn) {
            break;
        }
    }
    insns
}
