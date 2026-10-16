//! Translates decoded RISC-V basic blocks to native code with Cranelift.
//!
//! Covers RV64I, M, Zba/Zbb/Zbs and Zicond: loads and stores with the TLB lookup inlined and a
//! helper-call slow path, and branches/jumps that end the block. Anything else ends the compiled
//! prefix of a block; the interpreter takes it from there.
//!
//! Blocks use Cranelift's `tail` calling convention so that static exits can tail-call their
//! successor's code through a chain slot (see `rv_core::jit`); a single trampoline in the
//! platform convention enters the first block of a chain.

mod cache;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I16, I32, I64};
use cranelift_codegen::ir::{
    AbiParam, Function, InstBuilder, MemFlagsData, SigRef, Signature, StackSlotData, StackSlotKind, Type,
    Value,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use rv_core::Priv;
use rv_core::jit::{
    BlockCompiler, BlockInfo, ChainSlot, Compiled, EXCEPTION, JitCode, JitEntry, abi, jit_load, jit_store,
};
use rv_core::mmu::Access;
use rv_isa::{Insn, Op};

/// Compilation statistics, shared so that they can be read while the compiler is owned by a hart.
#[derive(Default, Debug)]
pub struct Stats {
    pub blocks: AtomicU64,
    pub instructions: AtomicU64,
    pub rejected: AtomicU64,
    pub nanos: AtomicU64,
    pub code_bytes: AtomicU64,
    /// Times the code cache filled up and was emptied.
    pub flushes: AtomicU64,
}

pub struct Compiler {
    isa: Arc<dyn TargetIsa>,
    cache: cache::CodeCache,
    ctx: Context,
    fctx: FunctionBuilderContext,
    /// Signature of the `jit_load`/`jit_store` helpers (platform calling convention).
    helper_sig: Signature,
    /// Signature of the `jit_exec` helper.
    exec_sig: Signature,
    /// Signature of compiled blocks: `(state, hart) -> status`, tail calling convention.
    block_sig: Signature,
    entry: JitEntry,
    /// Chain slots handed out to compiled code; they must live as long as the code.
    #[allow(clippy::vec_box, reason = "generated code holds the slots' addresses, so they must not move")]
    slots: Vec<Box<[ChainSlot; 2]>>,
    stats: Arc<Stats>,
    /// Debugging switches (environment variables `RV_JIT_NO_CHAIN`, `RV_JIT_NO_JUMP_CACHE`).
    chaining: bool,
    jump_cache: bool,
}

/// Whether the compiler can translate `op`.
fn supported(op: Op) -> bool {
    use Op::*;
    matches!(
        op,
        Lui | Auipc
            | Jal
            | Jalr
            | Beq
            | Bne
            | Blt
            | Bge
            | Bltu
            | Bgeu
            | Lb
            | Lh
            | Lw
            | Ld
            | Lbu
            | Lhu
            | Lwu
            | Sb
            | Sh
            | Sw
            | Sd
            | Addi
            | Slti
            | Sltiu
            | Xori
            | Ori
            | Andi
            | Slli
            | Srli
            | Srai
            | Add
            | Sub
            | Sll
            | Slt
            | Sltu
            | Xor
            | Srl
            | Sra
            | Or
            | And
            | Addiw
            | Slliw
            | Srliw
            | Sraiw
            | Addw
            | Subw
            | Sllw
            | Srlw
            | Sraw
            | Mul
            | Mulh
            | Mulhsu
            | Mulhu
            | Div
            | Divu
            | Rem
            | Remu
            | Mulw
            | Divw
            | Divuw
            | Remw
            | Remuw
            | Fence
            | Sh1add
            | Sh2add
            | Sh3add
            | AddUw
            | Sh1addUw
            | Sh2addUw
            | Sh3addUw
            | SlliUw
            | Andn
            | Orn
            | Xnor
            | Clz
            | Clzw
            | Ctz
            | Ctzw
            | Cpop
            | Cpopw
            | Max
            | Maxu
            | Min
            | Minu
            | SextB
            | SextH
            | ZextH
            | Rol
            | Rolw
            | Ror
            | Rori
            | Roriw
            | Rorw
            | OrcB
            | Rev8
            | Bclr
            | Bclri
            | Bext
            | Bexti
            | Binv
            | Binvi
            | Bset
            | Bseti
            | CzeroEqz
            | CzeroNez
            | MopR
            | MopRr
    )
}

/// Default size of each compiler's code cache.
pub const CODE_CACHE_SIZE: usize = 256 << 20;

impl Compiler {
    pub fn new() -> Result<Self, String> {
        Self::with_cache_size(CODE_CACHE_SIZE)
    }

    /// A compiler whose code cache holds `cache_size` bytes.
    pub fn with_cache_size(cache_size: usize) -> Result<Self, String> {
        let mut flags = settings::builder();
        // Blocks are small; Cranelift's optimisations cost more compile time than they save.
        let opt = std::env::var("RV_JIT_OPT").unwrap_or_else(|_| "none".into());
        flags.set("opt_level", &opt).map_err(|e| e.to_string())?;
        flags.set("is_pic", "false").map_err(|e| e.to_string())?;
        flags.set("enable_verifier", "false").map_err(|e| e.to_string())?;
        let regalloc = std::env::var("RV_JIT_REGALLOC").unwrap_or_else(|_| "backtracking".into());
        flags.set("regalloc_algorithm", &regalloc).map_err(|e| e.to_string())?;
        let isa = cranelift_native::builder()
            .map_err(|e| e.to_string())?
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())?;

        let ptr = isa.pointer_type();
        let mut helper_sig = Signature::new(isa.default_call_conv());
        helper_sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(I64),
            AbiParam::new(I64),
            AbiParam::new(I64),
        ]);
        helper_sig.returns.push(AbiParam::new(I64));

        let mut exec_sig = Signature::new(isa.default_call_conv());
        exec_sig.params.extend([AbiParam::new(ptr), AbiParam::new(I64), AbiParam::new(I64)]);
        exec_sig.returns.push(AbiParam::new(I64));

        let mut block_sig = Signature::new(CallConv::Tail);
        block_sig.params.extend([AbiParam::new(ptr), AbiParam::new(ptr)]);
        block_sig.returns.push(AbiParam::new(I64));

        let mut compiler = Compiler {
            cache: cache::CodeCache::new(cache_size)?,
            isa,
            ctx: Context::new(),
            fctx: FunctionBuilderContext::new(),
            helper_sig,
            exec_sig,
            block_sig,
            entry: dummy_entry,
            slots: Vec::new(),
            stats: Arc::new(Stats::default()),
            chaining: std::env::var_os("RV_JIT_NO_CHAIN").is_none(),
            jump_cache: std::env::var_os("RV_JIT_NO_JUMP_CACHE").is_none(),
        };
        compiler.entry = compiler.build_entry()?;
        compiler.cache.reserve_current();
        Ok(compiler)
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    /// Compiles `self.ctx.func` and copies it into the code cache.
    fn emit(&mut self) -> Result<*const u8, String> {
        let code = self
            .ctx
            .compile(&*self.isa, &mut ControlPlane::default())
            .map_err(|e| format!("{:?}", e.inner))?;
        // All calls and jumps are indirect through absolute addresses, so nothing to relocate.
        assert!(code.buffer.relocs().is_empty(), "generated code needs relocation");
        let bytes = code.code_buffer();
        self.stats.code_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.cache.install(bytes).ok_or_else(|| "code cache full".to_string())
    }

    /// Builds the trampoline `entry(state, hart, code)` that calls a tail-convention block.
    fn build_entry(&mut self) -> Result<JitEntry, String> {
        let ptr = self.isa.pointer_type();
        let mut sig = Signature::new(self.isa.default_call_conv());
        sig.params.extend([AbiParam::new(ptr), AbiParam::new(ptr), AbiParam::new(I64)]);
        sig.returns.push(AbiParam::new(I64));
        self.ctx.clear();
        self.ctx.func = Function::with_name_signature(Default::default(), sig);
        let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
        let sig_ref = b.import_signature(self.block_sig.clone());
        let block = b.create_block();
        b.append_block_params_for_function_params(block);
        b.switch_to_block(block);
        b.seal_block(block);
        let (state, hart, code) =
            (b.block_params(block)[0], b.block_params(block)[1], b.block_params(block)[2]);
        let call = b.ins().call_indirect(sig_ref, code, &[state, hart]);
        let status = b.inst_results(call)[0];
        b.ins().return_(&[status]);
        b.finalize(self.isa.frontend_config());
        let entry = self.emit()?;
        // SAFETY: generated with the `JitEntry` signature in the platform calling convention.
        Ok(unsafe { std::mem::transmute::<*const u8, JitEntry>(entry) })
    }
}

/// Placeholder until the real trampoline has been generated.
unsafe extern "C" fn dummy_entry(_: *mut rv_core::hart::HartState, _: *mut rv_core::Hart, _: JitCode) -> u64 {
    unreachable!("JIT entry used before initialisation")
}

impl BlockCompiler for Compiler {
    fn entry(&self) -> JitEntry {
        self.entry
    }

    fn compile(&mut self, insns: &[Insn], info: BlockInfo) -> Compiled {
        self.compile_now(insns, info)
    }

    fn reset(&mut self) {
        self.cache.reset();
        self.slots.clear();
        self.stats.flushes.fetch_add(1, Ordering::Relaxed);
    }
}

impl Compiler {
    fn compile_now(&mut self, insns: &[Insn], info: BlockInfo) -> Compiled {
        let data_priv = info.data_priv;
        // Instructions the translator does not handle natively run through the interpreter
        // helper, as long as they cannot change control flow, privilege or translation.
        let count = insns
            .iter()
            .take_while(|i| {
                supported(i.op) || (i.op != Op::Illegal && rv_core::jit::executable_by_helper(i.op))
            })
            .count();
        if count == 0 {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            return Compiled::Unsupported;
        }
        let insns = &insns[..count];
        let start = Instant::now();

        self.ctx.clear();
        self.ctx.func = Function::with_name_signature(Default::default(), self.block_sig.clone());
        let mut slots: Box<[ChainSlot; 2]> = Box::default();
        for slot in slots.iter_mut() {
            slot.page = info.page as u32;
        }
        let slot_addr = [&slots[0] as *const ChainSlot as i64, &slots[1] as *const ChainSlot as i64];
        self.slots.push(slots);
        let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
        let block_sig_ref = b.import_signature(self.block_sig.clone());
        let helper_sig_ref = b.import_signature(self.helper_sig.clone());
        let exec_sig_ref = b.import_signature(self.exec_sig.clone());
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        b.seal_block(entry);
        let state = b.block_params(entry)[0];
        let hart = b.block_params(entry)[1];
        let scratch = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let trusted = MemFlagsData::trusted();
        let guest = MemFlagsData::new().with_notrap();

        let mut t = Translator {
            regs: [Variable::from_u32(0); 32],
            b,
            state,
            hart,
            helper_sig_ref,
            exec_sig_ref,
            scratch,
            trusted,
            guest,
            data_priv,
            pc0: state, // replaced below
            dirty: 0,
            block_sig_ref,
            slot_addr,
            page_generation: info.page_generation as i64,
            generation: info.generation,
            chaining: self.chaining,
            jump_cache: self.jump_cache,
        };
        t.load_registers(insns);
        t.pc0 = t.b.ins().load(I64, t.trusted, state, abi::PC as i32);

        let mut offset = 0i64;
        let mut ended = false;
        for (i, insn) in insns.iter().enumerate() {
            ended = t.translate(insn, i as u64, offset);
            offset += insn.len as i64;
        }
        if !ended {
            let next = t.b.ins().iadd_imm_s(t.pc0, offset);
            t.chain_exit(next, insns.len() as u64, 0);
        }
        t.b.seal_all_blocks();
        t.b.finalize(self.isa.frontend_config());
        let code = match self.emit() {
            Ok(code) => code,
            Err(_) => return Compiled::CacheFull,
        };

        self.stats.blocks.fetch_add(1, Ordering::Relaxed);
        self.stats.instructions.fetch_add(insns.len() as u64, Ordering::Relaxed);
        self.stats.nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Compiled::Code(code as JitCode)
    }
}

/// A request to the background compiler thread.
enum Request {
    Compile { insns: Box<[Insn]>, info: BlockInfo },
    Reset,
}

/// Compiles on a dedicated thread, so that the hart keeps interpreting a hot block until its
/// code is ready instead of stalling for the compilation.
pub struct ThreadedCompiler {
    requests: std::sync::mpsc::Sender<Request>,
    results: std::sync::mpsc::Receiver<(BlockInfo, Compiled)>,
    entry: JitEntry,
    stats: Arc<Stats>,
}

impl ThreadedCompiler {
    pub fn new(name: &str) -> Result<Self, String> {
        Self::with_cache_size(name, CODE_CACHE_SIZE)
    }

    pub fn with_cache_size(name: &str, cache_size: usize) -> Result<Self, String> {
        let (requests, rx_requests) = std::sync::mpsc::channel::<Request>();
        let (tx_results, results) = std::sync::mpsc::channel();
        let (tx_init, rx_init) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                // The Cranelift module is created and used only on this thread.
                let mut compiler = match Compiler::with_cache_size(cache_size) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx_init.send(Err(e));
                        return;
                    }
                };
                let _ = tx_init.send(Ok((compiler.entry, compiler.stats())));
                // Runs until the hart (and with it the request sender) goes away.
                while let Ok(req) = rx_requests.recv() {
                    match req {
                        Request::Compile { insns, info } => {
                            let result = compiler.compile(&insns, info);
                            if tx_results.send((info, result)).is_err() {
                                break;
                            }
                        }
                        Request::Reset => compiler.reset(),
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let (entry, stats) = rx_init.recv().map_err(|e| e.to_string())??;
        Ok(ThreadedCompiler { requests, results, entry, stats })
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }
}

impl BlockCompiler for ThreadedCompiler {
    fn compile(&mut self, insns: &[Insn], info: BlockInfo) -> Compiled {
        match self.requests.send(Request::Compile { insns: insns.into(), info }) {
            Ok(()) => Compiled::Pending,
            Err(_) => Compiled::Unsupported,
        }
    }

    fn poll(&mut self) -> Option<(BlockInfo, Compiled)> {
        self.results.try_recv().ok()
    }

    fn reset(&mut self) {
        let _ = self.requests.send(Request::Reset);
    }

    fn entry(&self) -> JitEntry {
        self.entry
    }
}

struct Translator<'a> {
    b: FunctionBuilder<'a>,
    state: Value,
    hart: Value,
    helper_sig_ref: SigRef,
    exec_sig_ref: SigRef,
    scratch: cranelift_codegen::ir::StackSlot,
    /// Flags for accesses to `HartState` (aligned, never trap).
    trusted: MemFlagsData,
    /// Flags for accesses to guest memory (possibly misaligned, never trap).
    guest: MemFlagsData,
    data_priv: Priv,
    /// Guest `pc` of the first instruction.
    pc0: Value,
    regs: [Variable; 32],
    /// Registers written so far, which must be stored back on exit.
    dirty: u32,
    block_sig_ref: SigRef,
    /// Addresses of this block's two chain slots.
    slot_addr: [i64; 2],
    /// Address of the page's code generation and the generation the block was decoded at.
    page_generation: i64,
    generation: u32,
    chaining: bool,
    jump_cache: bool,
}

const fn reg_offset(r: u8) -> i32 {
    (abi::X + 8 * r as usize) as i32
}

impl Translator<'_> {
    /// Declares a variable per register and loads those the block reads.
    fn load_registers(&mut self, insns: &[Insn]) {
        let mut used = 0u32;
        for i in insns {
            used |= (1 << i.rs1) | (1 << i.rs2);
        }
        for r in 1..32u8 {
            self.regs[r as usize] = self.b.declare_var(I64);
            if used & (1 << r) != 0 {
                let v = self.b.ins().load(I64, self.trusted, self.state, reg_offset(r));
                self.b.def_var(self.regs[r as usize], v);
            }
        }
    }

    fn get(&mut self, r: u8) -> Value {
        if r == 0 { self.b.ins().iconst(I64, 0) } else { self.b.use_var(self.regs[r as usize]) }
    }

    fn set(&mut self, r: u8, v: Value) {
        if r != 0 {
            self.b.def_var(self.regs[r as usize], v);
            self.dirty |= 1 << r;
        }
    }

    /// Writes back dirty registers and `pc`, and adds `retired` to the retired count.
    fn write_back(&mut self, pc: Value, retired: u64) {
        for r in 1..32u8 {
            if self.dirty & (1 << r) != 0 {
                let v = self.b.use_var(self.regs[r as usize]);
                self.b.ins().store(self.trusted, v, self.state, reg_offset(r));
            }
        }
        self.b.ins().store(self.trusted, pc, self.state, abi::PC as i32);
        let total = self.b.ins().load(I64, self.trusted, self.state, abi::JIT_RETIRED as i32);
        let total = self.b.ins().iadd_imm_s(total, retired as i64);
        self.b.ins().store(self.trusted, total, self.state, abi::JIT_RETIRED as i32);
    }

    /// Leaves the block for the dispatcher without a chain slot (indirect jumps, exceptions).
    fn exit(&mut self, pc: Value, retired: u64, status: u64) {
        self.write_back(pc, retired);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().store(self.trusted, zero, self.state, abi::JIT_CHAIN_FROM as i32);
        let status = self.b.ins().iconst(I64, status as i64);
        self.b.ins().return_(&[status]);
    }

    /// Leaves the block for a static successor: tail-calls it if chain slot `slot` is linked
    /// and still valid, otherwise returns to the dispatcher (which may link the slot).
    fn chain_exit(&mut self, pc: Value, retired: u64, slot: usize) {
        let slot_addr = self.b.ins().iconst(I64, self.slot_addr[slot]);
        self.chain_exit_via(pc, retired, slot_addr);
    }

    /// [`chain_exit`](Self::chain_exit) with the chain slot's address as a value, so that both
    /// arms of a branch can share one exit sequence.
    fn chain_exit_via(&mut self, pc: Value, retired: u64, slot_addr: Value) {
        if !self.chaining {
            return self.exit(pc, retired, 0);
        }
        self.write_back(pc, retired);
        let t = self.trusted;
        self.b.ins().store(t, slot_addr, self.state, abi::JIT_CHAIN_FROM as i32);

        let ret = self.b.create_block();
        let check_epoch = self.b.create_block();
        let chain = self.b.create_block();
        self.continue_guards(retired, ret);

        // Linked?
        let target = self.b.ins().load(I64, t, slot_addr, abi::SLOT_TARGET as i32);
        self.b.ins().brif(target, check_epoch, &[], ret, &[]);

        // Linked before the last FENCE.I, and the page unchanged since this block was decoded?
        self.b.switch_to_block(check_epoch);
        let link_epoch = self.b.ins().load(I32, t, slot_addr, abi::SLOT_EPOCH as i32);
        let epoch = self.b.ins().load(I32, t, self.state, abi::JIT_FENCE_EPOCH as i32);
        let same_epoch = self.b.ins().icmp(IntCC::Equal, link_epoch, epoch);
        let gen_ptr = self.b.ins().iconst(I64, self.page_generation);
        let generation = self.b.ins().load(I32, MemFlagsData::new().with_notrap(), gen_ptr, 0);
        let same_gen = self.b.ins().icmp_imm_s(IntCC::Equal, generation, self.generation as i64);
        let valid = self.b.ins().band(same_epoch, same_gen);
        self.b.ins().brif(valid, chain, &[], ret, &[]);

        self.b.switch_to_block(chain);
        self.b.ins().return_call_indirect(self.block_sig_ref, target, &[self.state, self.hart]);

        self.b.switch_to_block(ret);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().return_(&[zero]);
    }

    /// Charges `retired` against the budget and checks for requests to stop. Falls through to
    /// a new current block if execution may continue in compiled code, otherwise jumps to `ret`.
    fn continue_guards(&mut self, retired: u64, ret: cranelift_codegen::ir::Block) {
        let t = self.trusted;
        let check_request = self.b.create_block();
        let go = self.b.create_block();
        let budget = self.b.ins().load(I64, t, self.state, abi::JIT_BUDGET as i32);
        let budget = self.b.ins().iadd_imm_s(budget, -(retired as i64));
        self.b.ins().store(t, budget, self.state, abi::JIT_BUDGET as i32);
        let left = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, budget, 0);
        self.b.ins().brif(left, check_request, &[], ret, &[]);
        // Interrupts, IPIs and other requests to stop.
        self.b.switch_to_block(check_request);
        let request_ptr = self.b.ins().load(I64, t, self.state, abi::JIT_EXIT_REQUEST as i32);
        let request = self.b.ins().load(I32, MemFlagsData::new().with_notrap(), request_ptr, 0);
        self.b.ins().brif(request, ret, &[], go, &[]);
        self.b.switch_to_block(go);
    }

    /// Leaves the block through an indirect jump to `pc`: tail-calls the target if the fetch
    /// TLB maps it and the jump cache holds valid code for it, otherwise returns.
    fn indirect_exit(&mut self, pc: Value, retired: u64) {
        if !self.jump_cache {
            return self.exit(pc, retired, 0);
        }
        self.write_back(pc, retired);
        let t = self.trusted;
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().store(t, zero, self.state, abi::JIT_CHAIN_FROM as i32);
        let ret = self.b.create_block();
        let hit = self.b.create_block();
        let check = self.b.create_block();
        let chain = self.b.create_block();
        self.continue_guards(retired, ret);

        // Fetch TLB: the mapping of the target page.
        let vpn = self.b.ins().ushr_imm_s(pc, 12);
        let idx = self.b.ins().band_imm_s(vpn, (abi::TLB_SIZE - 1) as i64);
        let off = self.b.ins().imul_imm_s(idx, abi::TLB_ENTRY_SIZE as i64);
        let entry = self.b.ins().iadd(self.state, off);
        let table = abi::tlb_table(self.data_priv, Access::Fetch) as i32;
        let tag = self.b.ins().load(I64, t, entry, table + abi::TLB_TAG as i32);
        let mapped = self.b.ins().icmp(IntCC::Equal, tag, vpn);
        self.b.ins().brif(mapped, hit, &[], ret, &[]);

        // Jump cache, keyed by host address.
        self.b.switch_to_block(hit);
        let addend = self.b.ins().load(I64, t, entry, table + abi::TLB_ADDEND as i32);
        let host = self.b.ins().iadd(pc, addend);
        let slot = self.b.ins().ushr_imm_s(host, 1);
        let slot = self.b.ins().band_imm_s(slot, (rv_core::jit::JUMP_CACHE_ENTRIES - 1) as i64);
        let slot = self.b.ins().imul_imm_s(slot, abi::JC_ENTRY_SIZE as i64);
        let jc = self.b.ins().iadd(self.state, slot);
        let base = abi::JUMP_CACHE as i32;
        let cached_host = self.b.ins().load(I64, t, jc, base + abi::JC_HOST as i32);
        let same_host = self.b.ins().icmp(IntCC::Equal, cached_host, host);
        let cached_priv = self.b.ins().load(I32, t, jc, base + abi::JC_DATA_PRIV as i32);
        let same_priv = self.b.ins().icmp_imm_s(IntCC::Equal, cached_priv, self.data_priv as i64);
        let ok = self.b.ins().band(same_host, same_priv);
        self.b.ins().brif(ok, check, &[], ret, &[]);

        self.b.switch_to_block(check);
        let cached_epoch = self.b.ins().load(I32, t, jc, base + abi::JC_EPOCH as i32);
        let epoch = self.b.ins().load(I32, t, self.state, abi::JIT_FENCE_EPOCH as i32);
        let same_epoch = self.b.ins().icmp(IntCC::Equal, cached_epoch, epoch);
        let gen_ptr = self.b.ins().load(I64, t, jc, base + abi::JC_PAGE_GENERATION as i32);
        let generation = self.b.ins().load(I32, MemFlagsData::new().with_notrap(), gen_ptr, 0);
        let cached_gen = self.b.ins().load(I32, t, jc, base + abi::JC_GENERATION as i32);
        let same_gen = self.b.ins().icmp(IntCC::Equal, generation, cached_gen);
        let valid = self.b.ins().band(same_epoch, same_gen);
        self.b.ins().brif(valid, chain, &[], ret, &[]);

        self.b.switch_to_block(chain);
        let code = self.b.ins().load(I64, t, jc, base + abi::JC_CODE as i32);
        self.b.ins().return_call_indirect(self.block_sig_ref, code, &[self.state, self.hart]);

        self.b.switch_to_block(ret);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().return_(&[zero]);
    }

    /// Exit taken when instruction `index` (at byte `offset`) raised an exception.
    fn exception_exit(&mut self, index: u64, offset: i64) {
        let pc = self.b.ins().iadd_imm_s(self.pc0, offset);
        self.exit(pc, index, EXCEPTION);
    }

    /// Runs `insn` with the interpreter: writes cached registers back, calls `jit_exec`, and
    /// picks up the destination register afterwards.
    fn exec_via_helper(&mut self, insn: &Insn, index: u64, offset: i64) {
        let t = self.trusted;
        for r in 1..32u8 {
            if self.dirty & (1 << r) != 0 {
                let v = self.b.use_var(self.regs[r as usize]);
                self.b.ins().store(t, v, self.state, reg_offset(r));
            }
        }
        // The cached values now match HartState.
        self.dirty = 0;
        let pc = self.b.ins().iadd_imm_s(self.pc0, offset);
        self.b.ins().store(t, pc, self.state, abi::PC as i32);
        // SAFETY: `Insn` is plain data of exactly 16 bytes.
        let [lo, hi] = unsafe { std::mem::transmute::<Insn, [u64; 2]>(*insn) };
        let helper = self.b.ins().iconst(I64, rv_core::jit::jit_exec as *const () as usize as i64);
        let lo = self.b.ins().iconst(I64, lo as i64);
        let hi = self.b.ins().iconst(I64, hi as i64);
        let call = self.b.ins().call_indirect(self.exec_sig_ref, helper, &[self.hart, lo, hi]);
        let status = self.b.inst_results(call)[0];
        let fault = self.b.create_block();
        let ok = self.b.create_block();
        self.b.ins().brif(status, fault, &[], ok, &[]);
        self.b.switch_to_block(fault);
        self.exception_exit(index, offset);
        self.b.switch_to_block(ok);
        // The helper may have written rd (FP compares and moves, AMOs, vsetvl, ...).
        if insn.rd != 0 {
            let v = self.b.ins().load(I64, t, self.state, reg_offset(insn.rd));
            self.b.def_var(self.regs[insn.rd as usize], v);
        }
    }

    fn zext32(&mut self, v: Value) -> Value {
        let n = self.b.ins().ireduce(I32, v);
        self.b.ins().uextend(I64, n)
    }

    /// bclr/bset/binv/bext with the bit index `n` (taken modulo 64).
    fn bit_op(&mut self, op: Op, rd: u8, a: Value, n: Value) {
        let n = self.b.ins().band_imm_s(n, 63);
        let v = if op == Op::Bext {
            let shifted = self.b.ins().ushr(a, n);
            self.b.ins().band_imm_s(shifted, 1)
        } else {
            let one = self.b.ins().iconst(I64, 1);
            let bit = self.b.ins().ishl(one, n);
            match op {
                Op::Bclr => self.b.ins().band_not(a, bit),
                Op::Bset => self.b.ins().bor(a, bit),
                _ => self.b.ins().bxor(a, bit),
            }
        };
        self.set(rd, v);
    }

    fn sext32(&mut self, v: Value) -> Value {
        let n = self.b.ins().ireduce(I32, v);
        self.b.ins().sextend(I64, n)
    }

    /// Inline TLB lookup. Branches to `fast` with the host address, or to `slow`.
    fn tlb_lookup(
        &mut self,
        addr: Value,
        size: u8,
        access: Access,
        fast: cranelift_codegen::ir::Block,
        slow: cranelift_codegen::ir::Block,
    ) {
        let b = &mut self.b;
        let vpn = b.ins().ushr_imm_s(addr, 12);
        let idx = b.ins().band_imm_s(vpn, (abi::TLB_SIZE - 1) as i64);
        let off = b.ins().imul_imm_s(idx, abi::TLB_ENTRY_SIZE as i64);
        let entry = b.ins().iadd(self.state, off);
        let table = abi::tlb_table(self.data_priv, access) as i32;
        let tag = b.ins().load(I64, self.trusted, entry, table + abi::TLB_TAG as i32);
        let mut hit = b.ins().icmp(IntCC::Equal, tag, vpn);
        if size > 1 {
            let page_off = b.ins().band_imm_s(addr, 0xfff);
            let fits = b.ins().icmp_imm_s(IntCC::UnsignedLessThanOrEqual, page_off, 4096 - size as i64);
            hit = b.ins().band(hit, fits);
        }
        let addend = b.ins().load(I64, self.trusted, entry, table + abi::TLB_ADDEND as i32);
        let host = b.ins().iadd(addr, addend);
        b.ins().brif(hit, fast, &[host.into()], slow, &[]);
    }

    fn mem_load(&mut self, insn: &Insn, index: u64, offset: i64) {
        let (size, signed): (u8, bool) = match insn.op {
            Op::Lb => (1, true),
            Op::Lh => (2, true),
            Op::Lw => (4, true),
            Op::Ld => (8, false),
            Op::Lbu => (1, false),
            Op::Lhu => (2, false),
            _ => (4, false),
        };
        let base = self.get(insn.rs1);
        let addr = self.b.ins().iadd_imm_s(base, insn.imm as i64);

        let fast = self.b.create_block();
        let host = self.b.append_block_param(fast, I64);
        let slow = self.b.create_block();
        let fault = self.b.create_block();
        let ok = self.b.create_block();
        let done = self.b.create_block();
        let value = self.b.append_block_param(done, I64);
        self.tlb_lookup(addr, size, Access::Load, fast, slow);

        self.b.switch_to_block(fast);
        let flags = self.guest;
        let v = match (size, signed) {
            (1, true) => self.b.ins().sload8(I64, flags, host, 0),
            (1, false) => self.b.ins().uload8(I64, flags, host, 0),
            (2, true) => self.b.ins().sload16(I64, flags, host, 0),
            (2, false) => self.b.ins().uload16(I64, flags, host, 0),
            (4, true) => self.b.ins().sload32(flags, host, 0),
            (4, false) => self.b.ins().uload32(flags, host, 0),
            _ => self.b.ins().load(I64, flags, host, 0),
        };
        self.b.ins().jump(done, &[v.into()]);

        self.b.switch_to_block(slow);
        let out = self.b.ins().stack_addr(I64, self.scratch, 0);
        let size_v = self.b.ins().iconst(I64, size as i64);
        let helper = self.b.ins().iconst(I64, jit_load as *const () as usize as i64);
        let call = self.b.ins().call_indirect(self.helper_sig_ref, helper, &[self.hart, addr, size_v, out]);
        let status = self.b.inst_results(call)[0];
        self.b.ins().brif(status, fault, &[], ok, &[]);

        self.b.switch_to_block(fault);
        self.exception_exit(index, offset);

        self.b.switch_to_block(ok);
        let raw = self.b.ins().stack_load(I64, I64, self.scratch, 0);
        let v = if signed && size < 8 {
            let ty: Type = match size {
                1 => I8,
                2 => I16,
                _ => I32,
            };
            let n = self.b.ins().ireduce(ty, raw);
            self.b.ins().sextend(I64, n)
        } else {
            raw
        };
        self.b.ins().jump(done, &[v.into()]);

        self.b.switch_to_block(done);
        self.set(insn.rd, value);
    }

    fn mem_store(&mut self, insn: &Insn, index: u64, offset: i64) {
        let size: u8 = match insn.op {
            Op::Sb => 1,
            Op::Sh => 2,
            Op::Sw => 4,
            _ => 8,
        };
        let base = self.get(insn.rs1);
        let addr = self.b.ins().iadd_imm_s(base, insn.imm as i64);
        let value = self.get(insn.rs2);

        let fast = self.b.create_block();
        let host = self.b.append_block_param(fast, I64);
        let slow = self.b.create_block();
        let fault = self.b.create_block();
        let done = self.b.create_block();
        self.tlb_lookup(addr, size, Access::Store, fast, slow);

        self.b.switch_to_block(fast);
        let flags = self.guest;
        match size {
            1 => self.b.ins().istore8(flags, value, host, 0),
            2 => self.b.ins().istore16(flags, value, host, 0),
            4 => self.b.ins().istore32(flags, value, host, 0),
            _ => self.b.ins().store(flags, value, host, 0),
        };
        self.b.ins().jump(done, &[]);

        self.b.switch_to_block(slow);
        let size_v = self.b.ins().iconst(I64, size as i64);
        let helper = self.b.ins().iconst(I64, jit_store as *const () as usize as i64);
        let call = self.b.ins().call_indirect(self.helper_sig_ref, helper, &[self.hart, addr, size_v, value]);
        let status = self.b.inst_results(call)[0];
        self.b.ins().brif(status, fault, &[], done, &[]);

        self.b.switch_to_block(fault);
        self.exception_exit(index, offset);

        self.b.switch_to_block(done);
    }

    /// Signed/unsigned division and remainder with RISC-V semantics (no traps).
    fn divrem(&mut self, op: Op, a: Value, b: Value, ty: Type) -> Value {
        let bi = &mut self.b;
        let zero = bi.ins().icmp_imm_s(IntCC::Equal, b, 0);
        let one = bi.ins().iconst(ty, 1);
        let signed = matches!(op, Op::Div | Op::Rem | Op::Divw | Op::Remw);
        let mut divisor = bi.ins().select(zero, one, b);
        if signed {
            // MIN / -1 overflows; dividing by 1 instead gives the right quotient and remainder.
            let min = if ty == I32 { i32::MIN as i64 } else { i64::MIN };
            let is_min = bi.ins().icmp_imm_s(IntCC::Equal, a, min);
            let is_neg1 = bi.ins().icmp_imm_s(IntCC::Equal, b, -1);
            let overflow = bi.ins().band(is_min, is_neg1);
            divisor = bi.ins().select(overflow, one, divisor);
        }
        let all_ones = bi.ins().iconst(ty, -1);
        match op {
            Op::Div | Op::Divw => {
                let q = bi.ins().sdiv(a, divisor);
                bi.ins().select(zero, all_ones, q)
            }
            Op::Divu | Op::Divuw => {
                let q = bi.ins().udiv(a, divisor);
                bi.ins().select(zero, all_ones, q)
            }
            Op::Rem | Op::Remw => {
                let r = bi.ins().srem(a, divisor);
                bi.ins().select(zero, a, r)
            }
            _ => {
                let r = bi.ins().urem(a, divisor);
                bi.ins().select(zero, a, r)
            }
        }
    }

    /// Translates one instruction. Returns `true` if it ended the block (control transfer).
    fn translate(&mut self, insn: &Insn, index: u64, offset: i64) -> bool {
        let imm = insn.imm as i64;
        let len = insn.len as i64;
        let rd = insn.rd;
        let retired = index + 1;
        macro_rules! binop {
            ($method:ident) => {{
                let a = self.get(insn.rs1);
                let b = self.get(insn.rs2);
                let v = self.b.ins().$method(a, b);
                self.set(rd, v);
            }};
        }
        macro_rules! immop {
            ($method:ident) => {{
                let a = self.get(insn.rs1);
                let v = self.b.ins().$method(a, imm);
                self.set(rd, v);
            }};
        }
        macro_rules! binop32 {
            ($method:ident) => {{
                let a = self.get(insn.rs1);
                let b = self.get(insn.rs2);
                let a = self.b.ins().ireduce(I32, a);
                let b = self.b.ins().ireduce(I32, b);
                let v = self.b.ins().$method(a, b);
                let v = self.b.ins().sextend(I64, v);
                self.set(rd, v);
            }};
        }
        macro_rules! setcc {
            ($cc:expr, $b:expr) => {{
                let a = self.get(insn.rs1);
                let c = self.b.ins().icmp($cc, a, $b);
                let v = self.b.ins().uextend(I64, c);
                self.set(rd, v);
            }};
        }

        match insn.op {
            Op::Lui => {
                let v = self.b.ins().iconst(I64, imm);
                self.set(rd, v);
            }
            Op::Auipc => {
                let v = self.b.ins().iadd_imm_s(self.pc0, offset + imm);
                self.set(rd, v);
            }
            Op::Jal => {
                let link = self.b.ins().iadd_imm_s(self.pc0, offset + len);
                self.set(rd, link);
                let target = self.b.ins().iadd_imm_s(self.pc0, offset + imm);
                self.chain_exit(target, retired, 0);
                return true;
            }
            Op::Jalr => {
                let base = self.get(insn.rs1);
                let t = self.b.ins().iadd_imm_s(base, imm);
                let target = self.b.ins().band_imm_s(t, !1);
                let link = self.b.ins().iadd_imm_s(self.pc0, offset + len);
                self.set(rd, link);
                self.indirect_exit(target, retired);
                return true;
            }
            Op::Beq | Op::Bne | Op::Blt | Op::Bge | Op::Bltu | Op::Bgeu => {
                let cc = match insn.op {
                    Op::Beq => IntCC::Equal,
                    Op::Bne => IntCC::NotEqual,
                    Op::Blt => IntCC::SignedLessThan,
                    Op::Bge => IntCC::SignedGreaterThanOrEqual,
                    Op::Bltu => IntCC::UnsignedLessThan,
                    _ => IntCC::UnsignedGreaterThanOrEqual,
                };
                let a = self.get(insn.rs1);
                let b = self.get(insn.rs2);
                let taken = self.b.ins().icmp(cc, a, b);
                // Select the successor and its chain slot, then share one exit sequence.
                let target = self.b.ins().iadd_imm_s(self.pc0, offset + imm);
                let fall = self.b.ins().iadd_imm_s(self.pc0, offset + len);
                let next = self.b.ins().select(taken, target, fall);
                let slot_taken = self.b.ins().iconst(I64, self.slot_addr[0]);
                let slot_fall = self.b.ins().iconst(I64, self.slot_addr[1]);
                let slot = self.b.ins().select(taken, slot_taken, slot_fall);
                self.chain_exit_via(next, retired, slot);
                return true;
            }
            Op::Lb | Op::Lh | Op::Lw | Op::Ld | Op::Lbu | Op::Lhu | Op::Lwu => {
                self.mem_load(insn, index, offset)
            }
            Op::Sb | Op::Sh | Op::Sw | Op::Sd => self.mem_store(insn, index, offset),

            Op::Addi => immop!(iadd_imm_s),
            Op::Xori => immop!(bxor_imm_s),
            Op::Ori => immop!(bor_imm_s),
            Op::Andi => immop!(band_imm_s),
            Op::Slli => immop!(ishl_imm_s),
            Op::Srli => immop!(ushr_imm_s),
            Op::Srai => immop!(sshr_imm_s),
            Op::Slti => {
                let b = self.b.ins().iconst(I64, imm);
                setcc!(IntCC::SignedLessThan, b)
            }
            Op::Sltiu => {
                let b = self.b.ins().iconst(I64, imm);
                setcc!(IntCC::UnsignedLessThan, b)
            }
            Op::Add => binop!(iadd),
            Op::Sub => binop!(isub),
            Op::Sll => binop!(ishl),
            Op::Srl => binop!(ushr),
            Op::Sra => binop!(sshr),
            Op::Xor => binop!(bxor),
            Op::Or => binop!(bor),
            Op::And => binop!(band),
            Op::Slt => {
                let b = self.get(insn.rs2);
                setcc!(IntCC::SignedLessThan, b)
            }
            Op::Sltu => {
                let b = self.get(insn.rs2);
                setcc!(IntCC::UnsignedLessThan, b)
            }
            Op::Addiw => {
                let a = self.get(insn.rs1);
                let v = self.b.ins().iadd_imm_s(a, imm);
                let v = self.sext32(v);
                self.set(rd, v);
            }
            Op::Slliw | Op::Srliw | Op::Sraiw => {
                let a = self.get(insn.rs1);
                let a = self.b.ins().ireduce(I32, a);
                let v = match insn.op {
                    Op::Slliw => self.b.ins().ishl_imm_s(a, imm),
                    Op::Srliw => self.b.ins().ushr_imm_s(a, imm),
                    _ => self.b.ins().sshr_imm_s(a, imm),
                };
                let v = self.b.ins().sextend(I64, v);
                self.set(rd, v);
            }
            Op::Addw => binop32!(iadd),
            Op::Subw => binop32!(isub),
            Op::Sllw => binop32!(ishl),
            Op::Srlw => binop32!(ushr),
            Op::Sraw => binop32!(sshr),
            Op::Mul => binop!(imul),
            Op::Mulh => binop!(smulhi),
            Op::Mulhu => binop!(umulhi),
            Op::Mulhsu => {
                // mulhsu(a, b) = mulhu(a, b) - (a < 0 ? b : 0)
                let a = self.get(insn.rs1);
                let b = self.get(insn.rs2);
                let hi = self.b.ins().umulhi(a, b);
                let sign = self.b.ins().sshr_imm_s(a, 63);
                let fix = self.b.ins().band(sign, b);
                let v = self.b.ins().isub(hi, fix);
                self.set(rd, v);
            }
            Op::Mulw => binop32!(imul),
            Op::Div | Op::Divu | Op::Rem | Op::Remu => {
                let a = self.get(insn.rs1);
                let b = self.get(insn.rs2);
                let v = self.divrem(insn.op, a, b, I64);
                self.set(rd, v);
            }
            Op::Divw | Op::Divuw | Op::Remw | Op::Remuw => {
                let a = self.get(insn.rs1);
                let b = self.get(insn.rs2);
                let a = self.b.ins().ireduce(I32, a);
                let b = self.b.ins().ireduce(I32, b);
                let v = self.divrem(insn.op, a, b, I32);
                let v = self.b.ins().sextend(I64, v);
                self.set(rd, v);
            }
            Op::Fence => {
                self.b.ins().fence();
            }

            // Zba
            Op::Sh1add | Op::Sh2add | Op::Sh3add | Op::AddUw | Op::Sh1addUw | Op::Sh2addUw | Op::Sh3addUw => {
                let a = self.get(insn.rs1);
                let b = self.get(insn.rs2);
                let uw = matches!(insn.op, Op::AddUw | Op::Sh1addUw | Op::Sh2addUw | Op::Sh3addUw);
                let a = if uw { self.zext32(a) } else { a };
                let shift = match insn.op {
                    Op::Sh1add | Op::Sh1addUw => 1,
                    Op::Sh2add | Op::Sh2addUw => 2,
                    Op::Sh3add | Op::Sh3addUw => 3,
                    _ => 0,
                };
                let a = self.b.ins().ishl_imm_s(a, shift);
                let v = self.b.ins().iadd(a, b);
                self.set(rd, v);
            }
            Op::SlliUw => {
                let a = self.get(insn.rs1);
                let a = self.zext32(a);
                let v = self.b.ins().ishl_imm_s(a, imm & 63);
                self.set(rd, v);
            }
            // Zbb
            Op::Andn => binop!(band_not),
            Op::Orn => binop!(bor_not),
            Op::Xnor => binop!(bxor_not),
            Op::Max => binop!(smax),
            Op::Maxu => binop!(umax),
            Op::Min => binop!(smin),
            Op::Minu => binop!(umin),
            Op::Rol => binop!(rotl),
            Op::Ror => binop!(rotr),
            Op::Rori => immop!(rotr_imm_s),
            Op::Clz | Op::Ctz | Op::Cpop | Op::Rev8 => {
                let a = self.get(insn.rs1);
                let v = match insn.op {
                    Op::Clz => self.b.ins().clz(a),
                    Op::Ctz => self.b.ins().ctz(a),
                    Op::Cpop => self.b.ins().popcnt(a),
                    _ => self.b.ins().bswap(a),
                };
                self.set(rd, v);
            }
            Op::Clzw | Op::Ctzw | Op::Cpopw => {
                let a = self.get(insn.rs1);
                let a = self.b.ins().ireduce(I32, a);
                let v = match insn.op {
                    Op::Clzw => self.b.ins().clz(a),
                    Op::Ctzw => self.b.ins().ctz(a),
                    _ => self.b.ins().popcnt(a),
                };
                let v = self.b.ins().uextend(I64, v);
                self.set(rd, v);
            }
            Op::Rolw | Op::Rorw | Op::Roriw => {
                let a = self.get(insn.rs1);
                let a = self.b.ins().ireduce(I32, a);
                let v = match insn.op {
                    Op::Rolw => {
                        let n = self.get(insn.rs2);
                        let n = self.b.ins().ireduce(I32, n);
                        self.b.ins().rotl(a, n)
                    }
                    Op::Rorw => {
                        let n = self.get(insn.rs2);
                        let n = self.b.ins().ireduce(I32, n);
                        self.b.ins().rotr(a, n)
                    }
                    _ => self.b.ins().rotr_imm_s(a, imm & 31),
                };
                let v = self.b.ins().sextend(I64, v);
                self.set(rd, v);
            }
            Op::SextB | Op::SextH | Op::ZextH => {
                let a = self.get(insn.rs1);
                let v = match insn.op {
                    Op::SextB => {
                        let n = self.b.ins().ireduce(I8, a);
                        self.b.ins().sextend(I64, n)
                    }
                    Op::SextH => {
                        let n = self.b.ins().ireduce(I16, a);
                        self.b.ins().sextend(I64, n)
                    }
                    _ => {
                        let n = self.b.ins().ireduce(I16, a);
                        self.b.ins().uextend(I64, n)
                    }
                };
                self.set(rd, v);
            }
            Op::OrcB => {
                // Per byte: 0xff if non-zero. ((x & 0x7f..) + 0x7f..) | x sets each byte's top bit
                // iff the byte is non-zero; spread that bit over the byte.
                let a = self.get(insn.rs1);
                let low7 = self.b.ins().band_imm_s(a, 0x7f7f_7f7f_7f7f_7f7f);
                let sum = self.b.ins().iadd_imm_s(low7, 0x7f7f_7f7f_7f7f_7f7f);
                let any = self.b.ins().bor(sum, a);
                let top = self.b.ins().band_imm_s(any, 0x8080_8080_8080_8080u64 as i64);
                let ones = self.b.ins().ushr_imm_s(top, 7);
                let v = self.b.ins().imul_imm_s(ones, 0xff);
                self.set(rd, v);
            }
            // Zbs
            Op::Bclr | Op::Bset | Op::Binv | Op::Bext => {
                let a = self.get(insn.rs1);
                let n = self.get(insn.rs2);
                self.bit_op(insn.op, rd, a, n);
            }
            Op::Bclri | Op::Bseti | Op::Binvi | Op::Bexti => {
                let a = self.get(insn.rs1);
                let n = self.b.ins().iconst(I64, imm & 63);
                let op = match insn.op {
                    Op::Bclri => Op::Bclr,
                    Op::Bseti => Op::Bset,
                    Op::Binvi => Op::Binv,
                    _ => Op::Bext,
                };
                self.bit_op(op, rd, a, n);
            }
            // Zicond
            Op::CzeroEqz | Op::CzeroNez => {
                let a = self.get(insn.rs1);
                let c = self.get(insn.rs2);
                let zero = self.b.ins().iconst(I64, 0);
                let v = if insn.op == Op::CzeroEqz {
                    self.b.ins().select(c, a, zero)
                } else {
                    self.b.ins().select(c, zero, a)
                };
                self.set(rd, v);
            }
            // Zimop / Zcmop
            Op::MopR | Op::MopRr => {
                let zero = self.b.ins().iconst(I64, 0);
                self.set(rd, zero);
            }
            _ => self.exec_via_helper(insn, index, offset),
        }
        false
    }
}
