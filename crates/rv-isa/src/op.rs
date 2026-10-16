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

    // F/D loads and stores (decoded now so that C.FLD & co. have somewhere to go;
    // executed once the FPU lands).
    Flw => "flw", FLoad;
    Fld => "fld", FLoad;
    Fsw => "fsw", FStore;
    Fsd => "fsd", FStore;

    // Privileged
    Mret => "mret", None;
    Sret => "sret", None;
    Wfi => "wfi", None;
    SfenceVma => "sfence.vma", SfenceVma;
}
