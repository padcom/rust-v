//! The set of operations the decoder can produce.
//!
//! Compressed instructions are expanded to their 32-bit equivalents, so there is no `C*` op here;
//! [`crate::Insn::len`] tells the two apart.

/// Operand layout of an instruction, used by the disassembler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// No operands.
    None,
    /// `rd, rs1, rs2`
    R,
    /// `rd, rs1, imm`
    I,
    /// `rd, imm(rs1)`
    Load,
    /// `rs2, imm(rs1)`
    Store,
    /// `rs1, rs2, target`
    Branch,
    /// `rd, imm[31:12]`
    U,
    /// `rd, target`
    J,
    /// `rd, csr, rs1`
    Csr,
    /// `rd, csr, uimm`
    CsrI,
    /// `rd, rs2, (rs1)`
    Amo,
    /// `rd, (rs1)`
    Lr,
    /// `rs1, rs2`
    SfenceVma,
    /// `pred, succ`
    Fence,
    /// `fd, imm(rs1)` / `fs2, imm(rs1)`
    FLoad,
    FStore,
    /// `fd, fs1, fs2`
    FR,
    /// `fd, fs1`
    FR1,
    /// `fd, fs1, fs2, fs3`
    FR4,
    /// `rd, fs1`
    FToX,
    /// `fd, rs1`
    XToF,
    /// `rd, fs1, fs2`
    FCmp,
    /// `fd, index`
    FLi,
    /// `rd, rs1`
    R1,
    /// `(rs1)`
    Cbo,
    /// Vector instruction; operands are decoded from the raw encoding.
    Vector,
}

macro_rules! ops {
    ($($name:ident => $mnemonic:literal, $format:ident;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
        #[repr(u16)]
        pub enum Op {
            #[default]
            $($name,)*
        }

        impl Op {
            pub const fn mnemonic(self) -> &'static str {
                match self { $(Op::$name => $mnemonic,)* }
            }

            pub const fn format(self) -> Format {
                match self { $(Op::$name => Format::$format,)* }
            }
        }
    };
}

ops! {
    Illegal => "illegal", None;

    // RV64I
    Lui => "lui", U;
    Auipc => "auipc", U;
    Jal => "jal", J;
    Jalr => "jalr", Load;
    Beq => "beq", Branch;
    Bne => "bne", Branch;
    Blt => "blt", Branch;
    Bge => "bge", Branch;
    Bltu => "bltu", Branch;
    Bgeu => "bgeu", Branch;
    Lb => "lb", Load;
    Lh => "lh", Load;
    Lw => "lw", Load;
    Ld => "ld", Load;
    Lbu => "lbu", Load;
    Lhu => "lhu", Load;
    Lwu => "lwu", Load;
    Sb => "sb", Store;
    Sh => "sh", Store;
    Sw => "sw", Store;
    Sd => "sd", Store;
    Addi => "addi", I;
    Slti => "slti", I;
    Sltiu => "sltiu", I;
    Xori => "xori", I;
    Ori => "ori", I;
    Andi => "andi", I;
    Slli => "slli", I;
    Srli => "srli", I;
    Srai => "srai", I;
    Add => "add", R;
    Sub => "sub", R;
    Sll => "sll", R;
    Slt => "slt", R;
    Sltu => "sltu", R;
    Xor => "xor", R;
    Srl => "srl", R;
    Sra => "sra", R;
    Or => "or", R;
    And => "and", R;
    Addiw => "addiw", I;
    Slliw => "slliw", I;
    Srliw => "srliw", I;
    Sraiw => "sraiw", I;
    Addw => "addw", R;
    Subw => "subw", R;
    Sllw => "sllw", R;
    Srlw => "srlw", R;
    Sraw => "sraw", R;
    Fence => "fence", Fence;
    Ecall => "ecall", None;
    Ebreak => "ebreak", None;

    // Zifencei
    FenceI => "fence.i", None;

    // Zicsr
    Csrrw => "csrrw", Csr;
    Csrrs => "csrrs", Csr;
    Csrrc => "csrrc", Csr;
    Csrrwi => "csrrwi", CsrI;
    Csrrsi => "csrrsi", CsrI;
    Csrrci => "csrrci", CsrI;

    // M
    Mul => "mul", R;
    Mulh => "mulh", R;
    Mulhsu => "mulhsu", R;
    Mulhu => "mulhu", R;
    Div => "div", R;
    Divu => "divu", R;
    Rem => "rem", R;
    Remu => "remu", R;
    Mulw => "mulw", R;
    Divw => "divw", R;
    Divuw => "divuw", R;
    Remw => "remw", R;
    Remuw => "remuw", R;

    // A
    LrW => "lr.w", Lr;
    ScW => "sc.w", Amo;
    AmoswapW => "amoswap.w", Amo;
    AmoaddW => "amoadd.w", Amo;
    AmoxorW => "amoxor.w", Amo;
    AmoandW => "amoand.w", Amo;
    AmoorW => "amoor.w", Amo;
    AmominW => "amomin.w", Amo;
    AmomaxW => "amomax.w", Amo;
    AmominuW => "amominu.w", Amo;
    AmomaxuW => "amomaxu.w", Amo;
    LrD => "lr.d", Lr;
    ScD => "sc.d", Amo;
    AmoswapD => "amoswap.d", Amo;
    AmoaddD => "amoadd.d", Amo;
    AmoxorD => "amoxor.d", Amo;
    AmoandD => "amoand.d", Amo;
    AmoorD => "amoor.d", Amo;
    AmominD => "amomin.d", Amo;
    AmomaxD => "amomax.d", Amo;
    AmominuD => "amominu.d", Amo;
    AmomaxuD => "amomaxu.d", Amo;

    // F, D, Zfa and Zfhmin. FP ops keep the rounding mode in `imm`, the third source in `rs3`
    // and the `fli` index in `rs1`.
    Flh => "flh", FLoad;
    Flw => "flw", FLoad;
    Fld => "fld", FLoad;
    Fsh => "fsh", FStore;
    Fsw => "fsw", FStore;
    Fsd => "fsd", FStore;
    FaddS => "fadd.s", FR;
    FsubS => "fsub.s", FR;
    FmulS => "fmul.s", FR;
    FdivS => "fdiv.s", FR;
    FsqrtS => "fsqrt.s", FR1;
    FsgnjS => "fsgnj.s", FR;
    FsgnjnS => "fsgnjn.s", FR;
    FsgnjxS => "fsgnjx.s", FR;
    FminS => "fmin.s", FR;
    FmaxS => "fmax.s", FR;
    FminmS => "fminm.s", FR;
    FmaxmS => "fmaxm.s", FR;
    FeqS => "feq.s", FCmp;
    FltS => "flt.s", FCmp;
    FleS => "fle.s", FCmp;
    FltqS => "fltq.s", FCmp;
    FleqS => "fleq.s", FCmp;
    FclassS => "fclass.s", FToX;
    FmaddS => "fmadd.s", FR4;
    FmsubS => "fmsub.s", FR4;
    FnmsubS => "fnmsub.s", FR4;
    FnmaddS => "fnmadd.s", FR4;
    FliS => "fli.s", FLi;
    FroundS => "fround.s", FR1;
    FroundnxS => "froundnx.s", FR1;
    FcvtWS => "fcvt.w.s", FToX;
    FcvtWuS => "fcvt.wu.s", FToX;
    FcvtLS => "fcvt.l.s", FToX;
    FcvtLuS => "fcvt.lu.s", FToX;
    FcvtSW => "fcvt.s.w", XToF;
    FcvtSWu => "fcvt.s.wu", XToF;
    FcvtSL => "fcvt.s.l", XToF;
    FcvtSLu => "fcvt.s.lu", XToF;
    FaddD => "fadd.d", FR;
    FsubD => "fsub.d", FR;
    FmulD => "fmul.d", FR;
    FdivD => "fdiv.d", FR;
    FsqrtD => "fsqrt.d", FR1;
    FsgnjD => "fsgnj.d", FR;
    FsgnjnD => "fsgnjn.d", FR;
    FsgnjxD => "fsgnjx.d", FR;
    FminD => "fmin.d", FR;
    FmaxD => "fmax.d", FR;
    FminmD => "fminm.d", FR;
    FmaxmD => "fmaxm.d", FR;
    FeqD => "feq.d", FCmp;
    FltD => "flt.d", FCmp;
    FleD => "fle.d", FCmp;
    FltqD => "fltq.d", FCmp;
    FleqD => "fleq.d", FCmp;
    FclassD => "fclass.d", FToX;
    FmaddD => "fmadd.d", FR4;
    FmsubD => "fmsub.d", FR4;
    FnmsubD => "fnmsub.d", FR4;
    FnmaddD => "fnmadd.d", FR4;
    FliD => "fli.d", FLi;
    FroundD => "fround.d", FR1;
    FroundnxD => "froundnx.d", FR1;
    FcvtWD => "fcvt.w.d", FToX;
    FcvtWuD => "fcvt.wu.d", FToX;
    FcvtLD => "fcvt.l.d", FToX;
    FcvtLuD => "fcvt.lu.d", FToX;
    FcvtDW => "fcvt.d.w", XToF;
    FcvtDWu => "fcvt.d.wu", XToF;
    FcvtDL => "fcvt.d.l", XToF;
    FcvtDLu => "fcvt.d.lu", XToF;
    FmvXW => "fmv.x.w", FToX;
    FmvWX => "fmv.w.x", XToF;
    FmvXD => "fmv.x.d", FToX;
    FmvDX => "fmv.d.x", XToF;
    FmvXH => "fmv.x.h", FToX;
    FmvHX => "fmv.h.x", XToF;
    FcvtSD => "fcvt.s.d", FR1;
    FcvtDS => "fcvt.d.s", FR1;
    FcvtSH => "fcvt.s.h", FR1;
    FcvtHS => "fcvt.h.s", FR1;
    FcvtDH => "fcvt.d.h", FR1;
    FcvtHD => "fcvt.h.d", FR1;
    FcvtmodWD => "fcvtmod.w.d", FToX;

    // Zba
    Sh1add => "sh1add", R;
    Sh2add => "sh2add", R;
    Sh3add => "sh3add", R;
    AddUw => "add.uw", R;
    Sh1addUw => "sh1add.uw", R;
    Sh2addUw => "sh2add.uw", R;
    Sh3addUw => "sh3add.uw", R;
    SlliUw => "slli.uw", I;

    // Zbb
    Andn => "andn", R;
    Orn => "orn", R;
    Xnor => "xnor", R;
    Clz => "clz", R1;
    Clzw => "clzw", R1;
    Ctz => "ctz", R1;
    Ctzw => "ctzw", R1;
    Cpop => "cpop", R1;
    Cpopw => "cpopw", R1;
    Max => "max", R;
    Maxu => "maxu", R;
    Min => "min", R;
    Minu => "minu", R;
    SextB => "sext.b", R1;
    SextH => "sext.h", R1;
    ZextH => "zext.h", R1;
    Rol => "rol", R;
    Rolw => "rolw", R;
    Ror => "ror", R;
    Rori => "rori", I;
    Roriw => "roriw", I;
    Rorw => "rorw", R;
    OrcB => "orc.b", R1;
    Rev8 => "rev8", R1;

    // Zbs
    Bclr => "bclr", R;
    Bclri => "bclri", I;
    Bext => "bext", R;
    Bexti => "bexti", I;
    Binv => "binv", R;
    Binvi => "binvi", I;
    Bset => "bset", R;
    Bseti => "bseti", I;

    // Zicond
    CzeroEqz => "czero.eqz", R;
    CzeroNez => "czero.nez", R;

    // Zimop / Zcmop: write zero to rd unless redefined by another extension.
    MopR => "mop.r", R1;
    MopRr => "mop.rr", R;

    // Zawrs
    WrsNto => "wrs.nto", None;
    WrsSto => "wrs.sto", None;

    // Zicbom / Zicboz
    CboInval => "cbo.inval", Cbo;
    CboClean => "cbo.clean", Cbo;
    CboFlush => "cbo.flush", Cbo;
    CboZero => "cbo.zero", Cbo;

    // Svinval
    SinvalVma => "sinval.vma", SfenceVma;
    SfenceWInval => "sfence.w.inval", None;
    SfenceInvalIr => "sfence.inval.ir", None;

    // V. Only the major forms are distinguished here; the vector unit decodes funct6, funct3,
    // vm, width, mop and nf from `raw`.
    Vsetvli => "vsetvli", Vector;
    Vsetivli => "vsetivli", Vector;
    Vsetvl => "vsetvl", Vector;
    Vop => "vop", Vector;
    Vload => "vload", Vector;
    Vstore => "vstore", Vector;

    // Privileged
    Mret => "mret", None;
    Sret => "sret", None;
    Wfi => "wfi", None;
    SfenceVma => "sfence.vma", SfenceVma;
}
