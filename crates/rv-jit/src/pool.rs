//! A JIT shared by all harts: one code cache, a pool of compiler threads, and a table of
//! compiled blocks that every hart consults, so each block is compiled once rather than once
//! per hart, and a hart can run code another hart asked for.
//!
//! Emptying the shared cache needs every hart out of compiled code. Harts announce that they
//! are using compiled code through [`BlockCompiler::enter`]/[`BlockCompiler::leave`] (a flag per
//! hart, checked against a `resetting` flag Dekker-style); the thread that finds the cache full
//! raises `resetting`, waits for the flags to drop, empties the cache and the table, and bumps
//! the cache generation. A hart that sees a new generation drops its references to compiled
//! code before using any again.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use rv_core::jit::{
    BlockCompiler, BlockInfo, ChainSlot, CodeRef, Compiled, Enter, JitCode, JitEntry, Region,
};

use crate::cache::CodeCache;
use crate::{Compiler, Stats};

const SHARDS: usize = 64;

#[derive(Clone, Copy)]
enum Entry {
    /// Being compiled for the given page generation.
    Pending(u32),
    /// Compiled for the given page generation (`None`: cannot be compiled).
    Done(u32, Option<CodeRef>),
}

/// Host address, data privilege and tier.
type Key = (u64, u8, u8);

struct Job {
    region: Arc<Region>,
    info: BlockInfo,
    /// Cache generation the job was queued in.
    cache_generation: u32,
}

struct Installed {
    cache: CodeCache,
    /// Chain slots of the installed code; it holds their addresses.
    slots: Vec<Box<[ChainSlot]>>,
}

#[repr(align(128))]
struct InJit(AtomicBool);

pub struct JitPool {
    installed: Mutex<Installed>,
    table: Box<[Mutex<HashMap<Key, Entry>>]>,
    jobs: Mutex<VecDeque<Job>>,
    work: Condvar,
    generation: AtomicU32,
    resetting: AtomicBool,
    in_jit: Box<[InJit]>,
    /// Jobs finished, so that harts only scan for results when there are new ones.
    completed: AtomicU64,
    /// Live [`PoolHandle`]s; the workers stop when the last one goes.
    handles: AtomicUsize,
    shutdown: AtomicBool,
    entry: JitEntry,
    stats: Arc<Stats>,
}

impl JitPool {
    /// A pool for `harts` harts with `workers` compiler threads and a code cache of
    /// `cache_size` bytes.
    pub fn new(harts: usize, workers: usize, cache_size: usize) -> Result<Arc<Self>, String> {
        let stats = Arc::new(Stats::default());
        let mut cache = CodeCache::new(cache_size)?;
        let entry = Compiler::bare(stats.clone())?.emit_entry(&mut cache)?;
        cache.reserve_current();
        let pool = Arc::new(JitPool {
            installed: Mutex::new(Installed { cache, slots: Vec::new() }),
            table: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            jobs: Mutex::new(VecDeque::new()),
            work: Condvar::new(),
            generation: AtomicU32::new(0),
            resetting: AtomicBool::new(false),
            in_jit: (0..harts).map(|_| InJit(AtomicBool::new(false))).collect(),
            completed: AtomicU64::new(0),
            handles: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            entry,
            stats,
        });
        for i in 0..workers.max(1) {
            let pool = pool.clone();
            std::thread::Builder::new()
                .name(format!("jit{i}"))
                .spawn(move || {
                    // Each worker has its own Cranelift context, created on its thread.
                    match Compiler::bare(pool.stats.clone()) {
                        Ok(compiler) => pool.worker(compiler),
                        Err(e) => eprintln!("rv-jit: worker {i}: {e}\r"),
                    }
                    // Only reached on shutdown: a panic would leave the harts interpreting
                    // forever without anyone noticing, so it aborts instead (see `worker`).
                })
                .map_err(|e| e.to_string())?;
        }
        Ok(pool)
    }

    /// The compiler interface for hart `hart`.
    pub fn handle(self: &Arc<Self>, hart: usize) -> PoolHandle {
        self.handles.fetch_add(1, Ordering::AcqRel);
        PoolHandle {
            pool: self.clone(),
            hart,
            synced: self.generation.load(Ordering::Acquire),
            pending: Vec::new(),
            ready: Vec::new(),
            seen: 0,
        }
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    fn shard(&self, key: Key) -> &Mutex<HashMap<Key, Entry>> {
        &self.table[((key.0 >> 1) as usize ^ key.1 as usize ^ key.2 as usize) % SHARDS]
    }

    fn worker(&self, mut compiler: Compiler) {
        loop {
            let job = {
                let mut jobs = self.jobs.lock().unwrap();
                loop {
                    if self.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(job) = jobs.pop_front() {
                        break job;
                    }
                    jobs = self.work.wait(jobs).unwrap();
                }
            };
            if job.cache_generation != self.generation.load(Ordering::Acquire) {
                continue; // queued before the cache was emptied: nobody waits for it any more
            }
            // A compiler panic is a bug; take the process down rather than leave the harts
            // waiting for code that never comes.
            let generated = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                compiler.generate(&job.region, job.info)
            })) {
                Ok(g) => g,
                Err(_) => std::process::abort(),
            };
            let mut installed = self.installed.lock().unwrap();
            let code = match generated {
                None => None,
                Some(g) => match installed.cache.install(&g.bytes) {
                    Some(code) => {
                        installed.slots.push(g.slots);
                        Some(CodeRef { code: code as JitCode, hooks: g.hooks })
                    }
                    None => {
                        self.reset(&mut installed);
                        continue;
                    }
                },
            };
            // Resets happen under `installed`, so the generation cannot change under us here.
            if job.cache_generation == self.generation.load(Ordering::Relaxed) {
                let key = (job.info.host, job.info.data_priv as u8, job.info.tier);
                self.shard(key).lock().unwrap().insert(key, Entry::Done(job.info.generation, code));
            }
            drop(installed);
            self.completed.fetch_add(1, Ordering::Release);
        }
    }

    /// Empties the code cache once no hart is in compiled code.
    fn reset(&self, installed: &mut Installed) {
        self.resetting.store(true, Ordering::SeqCst);
        for flag in self.in_jit.iter() {
            while flag.0.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
        installed.cache.reset();
        installed.slots.clear();
        for shard in self.table.iter() {
            shard.lock().unwrap().clear();
        }
        self.jobs.lock().unwrap().clear();
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.resetting.store(false, Ordering::SeqCst);
        self.stats.flushes.fetch_add(1, Ordering::Relaxed);
        self.completed.fetch_add(1, Ordering::Release);
    }
}

/// One hart's view of a [`JitPool`].
pub struct PoolHandle {
    pool: Arc<JitPool>,
    hart: usize,
    /// Cache generation whose code this hart may reference.
    synced: u32,
    /// Requests whose results the hart has not collected yet.
    pending: Vec<BlockInfo>,
    /// Results found but not yet handed out by `poll`.
    ready: Vec<(BlockInfo, Compiled)>,
    /// `completed` when `pending` was last scanned.
    seen: u64,
}

impl BlockCompiler for PoolHandle {
    fn compile(&mut self, region: &Arc<Region>, info: BlockInfo) -> Compiled {
        let key = (info.host, info.data_priv as u8, info.tier);
        let mut shard = self.pool.shard(key).lock().unwrap();
        match shard.get(&key) {
            Some(&Entry::Done(g, code)) if g == info.generation => {
                return code.map_or(Compiled::Unsupported, Compiled::Code);
            }
            Some(&Entry::Pending(g)) if g == info.generation => {}
            _ => {
                shard.insert(key, Entry::Pending(info.generation));
                drop(shard);
                let job = Job { region: region.clone(), info, cache_generation: self.synced };
                self.pool.jobs.lock().unwrap().push_back(job);
                self.pool.work.notify_one();
            }
        }
        self.pending.push(info);
        Compiled::Pending
    }

    fn poll(&mut self) -> Option<(BlockInfo, Compiled)> {
        if self.ready.is_empty() {
            let completed = self.pool.completed.load(Ordering::Acquire);
            if completed == self.seen {
                return None;
            }
            self.seen = completed;
            let pool = &self.pool;
            let ready = &mut self.ready;
            self.pending.retain(|info| {
                let key = (info.host, info.data_priv as u8, info.tier);
                let result = match pool.shard(key).lock().unwrap().get(&key) {
                    Some(&Entry::Pending(g)) if g == info.generation => return true,
                    Some(&Entry::Done(g, code)) if g == info.generation => {
                        code.map_or(Compiled::Unsupported, Compiled::Code)
                    }
                    // Superseded by a request for another version of the block.
                    _ => Compiled::Unsupported,
                };
                ready.push((*info, result));
                false
            });
        }
        self.ready.pop()
    }

    fn entry(&self) -> JitEntry {
        self.pool.entry
    }

    fn reset(&mut self) {
        self.synced = self.pool.generation.load(Ordering::Acquire);
        self.pending.clear();
        self.ready.clear();
    }

    fn enter(&mut self) -> Enter {
        let flag = &self.pool.in_jit[self.hart].0;
        flag.store(true, Ordering::SeqCst);
        if self.pool.resetting.load(Ordering::SeqCst) {
            flag.store(false, Ordering::Release);
            return Enter::Busy;
        }
        if self.pool.generation.load(Ordering::SeqCst) != self.synced {
            flag.store(false, Ordering::Release);
            return Enter::Stale;
        }
        Enter::Go
    }

    fn leave(&mut self) {
        self.pool.in_jit[self.hart].0.store(false, Ordering::Release);
    }
}

impl Drop for PoolHandle {
    fn drop(&mut self) {
        if self.pool.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _jobs = self.pool.jobs.lock().unwrap();
            self.pool.shutdown.store(true, Ordering::Release);
            self.pool.work.notify_all();
        }
    }
}
