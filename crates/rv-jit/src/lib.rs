//! Translates decoded RISC-V basic blocks to native code with Cranelift.
//!
//! This is the Phase 2.5 spike: RV64I + M arithmetic, loads/stores with the TLB lookup inlined
//! and a helper-call slow path, and branches/jumps that end the block. Anything else ends the
//! compiled prefix of a block; the interpreter takes it from there. There is no block chaining:
//! every block returns to the dispatcher.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I16, I32, I64};
use cranelift_codegen::ir::{
    AbiParam, FuncRef, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, Type, Value,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, default_libcall_names};
use rv_core::Priv;
use rv_core::jit::{BlockCompiler, EXCEPTION, JitFn, abi, jit_load, jit_store};
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
}

pub struct Compiler {
    module: JITModule,
    ctx: Context,
    fctx: FunctionBuilderContext,
    load_fn: FuncId,
    store_fn: FuncId,
    next_name: u64,
    stats: Arc<Stats>,
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
    )
}

impl Compiler {
    pub fn new() -> Result<Self, String> {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").map_err(|e| e.to_string())?;
        flags.set("is_pic", "false").map_err(|e| e.to_string())?;
        let isa = cranelift_native::builder()
            .map_err(|e| e.to_string())?
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())?;
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        builder.symbol("rv_jit_load", jit_load as *const u8);
        builder.symbol("rv_jit_store", jit_store as *const u8);
        let mut module = JITModule::new(builder);

        let ptr = module.target_config().pointer_type();
        let mut sig = module.make_signature();
        sig.params.extend([AbiParam::new(ptr), AbiParam::new(I64), AbiParam::new(I64), AbiParam::new(I64)]);
        sig.returns.push(AbiParam::new(I64));
        let load_fn =
            module.declare_function("rv_jit_load", Linkage::Import, &sig).map_err(|e| e.to_string())?;
        let store_fn =
            module.declare_function("rv_jit_store", Linkage::Import, &sig).map_err(|e| e.to_string())?;

        Ok(Compiler {
            ctx: module.make_context(),
            module,
            fctx: FunctionBuilderContext::new(),
            load_fn,
            store_fn,
            next_name: 0,
            stats: Arc::new(Stats::default()),
        })
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }
}

impl BlockCompiler for Compiler {
    fn compile(&mut self, insns: &[Insn], data_priv: Priv) -> Option<JitFn> {
        let count = insns.iter().take_while(|i| supported(i.op)).count();
        if count == 0 {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let insns = &insns[..count];
        let start = Instant::now();

        self.module.clear_context(&mut self.ctx);
        let ptr = self.module.target_config().pointer_type();
        self.ctx.func.signature.params.extend([AbiParam::new(ptr), AbiParam::new(ptr)]);
        self.ctx.func.signature.returns.push(AbiParam::new(I64));
        let load_ref = self.module.declare_func_in_func(self.load_fn, &mut self.ctx.func);
        let store_ref = self.module.declare_func_in_func(self.store_fn, &mut self.ctx.func);

        let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
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
            load_ref,
            store_ref,
            scratch,
            trusted,
            guest,
            data_priv,
            pc0: state, // replaced below
            dirty: 0,
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
            t.exit(next, insns.len() as u64);
        }
        t.b.seal_all_blocks();
        t.b.finalize(self.module.target_config());

        let name = format!("b{}", self.next_name);
        self.next_name += 1;
        let id = self.module.declare_function(&name, Linkage::Local, &self.ctx.func.signature).ok()?;
        self.module.define_function(id, &mut self.ctx).ok()?;
        let size = self.ctx.compiled_code().map_or(0, |c| c.code_buffer().len());
        self.module.finalize_definitions().ok()?;
        let code = self.module.get_finalized_function(id);

        self.stats.blocks.fetch_add(1, Ordering::Relaxed);
        self.stats.instructions.fetch_add(insns.len() as u64, Ordering::Relaxed);
        self.stats.code_bytes.fetch_add(size as u64, Ordering::Relaxed);
        self.stats.nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        // SAFETY: the function was generated with the `JitFn` signature.
        Some(unsafe { std::mem::transmute::<*const u8, JitFn>(code) })
    }
}

struct Translator<'a> {
    b: FunctionBuilder<'a>,
    state: Value,
    hart: Value,
    load_ref: FuncRef,
    store_ref: FuncRef,
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

    /// Writes back dirty registers and `pc`, and returns `ret` from the block.
    fn exit(&mut self, pc: Value, ret: u64) {
        for r in 1..32u8 {
            if self.dirty & (1 << r) != 0 {
                let v = self.b.use_var(self.regs[r as usize]);
                self.b.ins().store(self.trusted, v, self.state, reg_offset(r));
            }
        }
        self.b.ins().store(self.trusted, pc, self.state, abi::PC as i32);
        let ret = self.b.ins().iconst(I64, ret as i64);
        self.b.ins().return_(&[ret]);
    }

    /// Exit taken when instruction `index` (at byte `offset`) raised an exception.
    fn exception_exit(&mut self, index: u64, offset: i64) {
        let pc = self.b.ins().iadd_imm_s(self.pc0, offset);
        self.exit(pc, EXCEPTION | index);
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
        let call = self.b.ins().call(self.load_ref, &[self.hart, addr, size_v, out]);
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
        let call = self.b.ins().call(self.store_ref, &[self.hart, addr, size_v, value]);
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
                self.exit(target, retired);
                return true;
            }
            Op::Jalr => {
                let base = self.get(insn.rs1);
                let t = self.b.ins().iadd_imm_s(base, imm);
                let target = self.b.ins().band_imm_s(t, !1);
                let link = self.b.ins().iadd_imm_s(self.pc0, offset + len);
                self.set(rd, link);
                self.exit(target, retired);
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
                let target = self.b.ins().iadd_imm_s(self.pc0, offset + imm);
                let fall = self.b.ins().iadd_imm_s(self.pc0, offset + len);
                let next = self.b.ins().select(taken, target, fall);
                self.exit(next, retired);
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
            op => unreachable!("unsupported op {op:?} reached the translator"),
        }
        false
    }
}
