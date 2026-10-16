use crate::{Insn, Op};

#[inline(always)]
const fn bits(raw: u32, hi: u32, lo: u32) -> u32 {
    (raw >> lo) & ((1 << (hi - lo + 1)) - 1)
}

#[inline(always)]
const fn bit(raw: u32, n: u32) -> u32 {
    (raw >> n) & 1
}

/// Sign-extends the low `width` bits of `value`.
#[inline(always)]
const fn sext(value: u32, width: u32) -> i32 {
    let shift = 32 - width;
    ((value << shift) as i32) >> shift
}

/// Length in bytes of the instruction whose first 16-bit parcel is `low`.
#[inline]
pub const fn insn_len(low: u16) -> u8 {
    if low & 0b11 == 0b11 { 4 } else { 2 }
}

/// Decodes one instruction. `raw` holds the instruction in its low bits; for a compressed
/// instruction only the low 16 bits are looked at.
pub fn decode(raw: u32) -> Insn {
    if raw & 0b11 == 0b11 { decode32(raw) } else { decode16(raw as u16) }
}

const fn imm_i(raw: u32) -> i32 {
    (raw as i32) >> 20
}

const fn imm_s(raw: u32) -> i32 {
    (((raw as i32) >> 25) << 5) | bits(raw, 11, 7) as i32
}

const fn imm_b(raw: u32) -> i32 {
    let v = (bit(raw, 31) << 12) | (bit(raw, 7) << 11) | (bits(raw, 30, 25) << 5) | (bits(raw, 11, 8) << 1);
    sext(v, 13)
}

const fn imm_u(raw: u32) -> i32 {
    (raw & 0xffff_f000) as i32
}

const fn imm_j(raw: u32) -> i32 {
    let v =
        (bit(raw, 31) << 20) | (bits(raw, 19, 12) << 12) | (bit(raw, 20) << 11) | (bits(raw, 30, 21) << 1);
    sext(v, 21)
}

fn decode32(raw: u32) -> Insn {
    let rd = bits(raw, 11, 7) as u8;
    let rs1 = bits(raw, 19, 15) as u8;
    let rs2 = bits(raw, 24, 20) as u8;
    let f3 = bits(raw, 14, 12);
    let f7 = bits(raw, 31, 25);

    let insn = |op, rd, rs1, rs2, imm| Insn { op, rd, rs1, rs2, rs3: 0, len: 4, imm, raw };
    let r = |op| insn(op, rd, rs1, rs2, 0);
    let i = |op| insn(op, rd, rs1, 0, imm_i(raw));
    let illegal = Insn::illegal(raw, 4);

    match bits(raw, 6, 0) {
        0x37 => insn(Op::Lui, rd, 0, 0, imm_u(raw)),
        0x17 => insn(Op::Auipc, rd, 0, 0, imm_u(raw)),
        0x6f => insn(Op::Jal, rd, 0, 0, imm_j(raw)),
        0x67 if f3 == 0 => i(Op::Jalr),
        0x63 => {
            let op = match f3 {
                0 => Op::Beq,
                1 => Op::Bne,
                4 => Op::Blt,
                5 => Op::Bge,
                6 => Op::Bltu,
                7 => Op::Bgeu,
                _ => return illegal,
            };
            insn(op, 0, rs1, rs2, imm_b(raw))
        }
        0x03 => {
            let op = match f3 {
                0 => Op::Lb,
                1 => Op::Lh,
                2 => Op::Lw,
                3 => Op::Ld,
                4 => Op::Lbu,
                5 => Op::Lhu,
                6 => Op::Lwu,
                _ => return illegal,
            };
            i(op)
        }
        0x23 => {
            let op = match f3 {
                0 => Op::Sb,
                1 => Op::Sh,
                2 => Op::Sw,
                3 => Op::Sd,
                _ => return illegal,
            };
            insn(op, 0, rs1, rs2, imm_s(raw))
        }
        0x07 => match f3 {
            0 | 5 | 6 | 7 => insn(Op::Vload, rd, rs1, rs2, 0),
            1 => i(Op::Flh),
            2 => i(Op::Flw),
            3 => i(Op::Fld),
            _ => illegal,
        },
        0x27 => match f3 {
            // For vector stores the "rd" field holds vs3, the data register.
            0 | 5 | 6 | 7 => insn(Op::Vstore, rd, rs1, rs2, 0),
            1 => insn(Op::Fsh, 0, rs1, rs2, imm_s(raw)),
            2 => insn(Op::Fsw, 0, rs1, rs2, imm_s(raw)),
            3 => insn(Op::Fsd, 0, rs1, rs2, imm_s(raw)),
            _ => illegal,
        },
        0x43 | 0x47 | 0x4b | 0x4f => decode_fma(raw, rd, rs1, rs2, f3),
        0x53 => decode_fp(raw, rd, rs1, rs2, f3),
        0x57 if f3 == 7 => match (bit(raw, 31), bit(raw, 30)) {
            (0, _) => insn(Op::Vsetvli, rd, rs1, 0, bits(raw, 30, 20) as i32),
            (1, 1) => insn(Op::Vsetivli, rd, rs1, 0, bits(raw, 29, 20) as i32),
            _ if bits(raw, 31, 25) == 0x40 => insn(Op::Vsetvl, rd, rs1, rs2, 0),
            _ => illegal,
        },
        0x57 => insn(Op::Vop, rd, rs1, rs2, 0),
        0x13 => {
            let shamt = bits(raw, 25, 20) as i32;
            let shift = |op| insn(op, rd, rs1, 0, shamt);
            let unary = |op| insn(op, rd, rs1, 0, 0);
            match (f3, bits(raw, 31, 26)) {
                (0, _) => i(Op::Addi),
                (2, _) => i(Op::Slti),
                (3, _) => i(Op::Sltiu),
                (4, _) => i(Op::Xori),
                (6, _) => i(Op::Ori),
                (7, _) => i(Op::Andi),
                (1, 0x00) => shift(Op::Slli),
                (1, 0x0a) => shift(Op::Bseti),
                (1, 0x12) => shift(Op::Bclri),
                (1, 0x1a) => shift(Op::Binvi),
                (1, 0x18) => match bits(raw, 25, 20) {
                    0x00 => unary(Op::Clz),
                    0x01 => unary(Op::Ctz),
                    0x02 => unary(Op::Cpop),
                    0x04 => unary(Op::SextB),
                    0x05 => unary(Op::SextH),
                    _ => illegal,
                },
                (5, 0x00) => shift(Op::Srli),
                (5, 0x10) => shift(Op::Srai),
                (5, 0x12) => shift(Op::Bexti),
                (5, 0x18) => shift(Op::Rori),
                (5, _) if bits(raw, 31, 20) == 0x287 => unary(Op::OrcB),
                (5, _) if bits(raw, 31, 20) == 0x6b8 => unary(Op::Rev8),
                _ => illegal,
            }
        }
        0x1b => {
            let shamt = bits(raw, 24, 20) as i32;
            match (f3, f7) {
                (0, _) => i(Op::Addiw),
                (1, 0) => insn(Op::Slliw, rd, rs1, 0, shamt),
                (1, 0x04 | 0x05) => insn(Op::SlliUw, rd, rs1, 0, bits(raw, 25, 20) as i32),
                (1, 0x30) => match rs2 {
                    0 => insn(Op::Clzw, rd, rs1, 0, 0),
                    1 => insn(Op::Ctzw, rd, rs1, 0, 0),
                    2 => insn(Op::Cpopw, rd, rs1, 0, 0),
                    _ => illegal,
                },
                (5, 0) => insn(Op::Srliw, rd, rs1, 0, shamt),
                (5, 0x20) => insn(Op::Sraiw, rd, rs1, 0, shamt),
                (5, 0x30) => insn(Op::Roriw, rd, rs1, 0, shamt),
                _ => illegal,
            }
        }
        0x33 => {
            let op = match (f7, f3) {
                (0, 0) => Op::Add,
                (0x20, 0) => Op::Sub,
                (0, 1) => Op::Sll,
                (0, 2) => Op::Slt,
                (0, 3) => Op::Sltu,
                (0, 4) => Op::Xor,
                (0, 5) => Op::Srl,
                (0x20, 5) => Op::Sra,
                (0, 6) => Op::Or,
                (0, 7) => Op::And,
                (1, 0) => Op::Mul,
                (1, 1) => Op::Mulh,
                (1, 2) => Op::Mulhsu,
                (1, 3) => Op::Mulhu,
                (1, 4) => Op::Div,
                (1, 5) => Op::Divu,
                (1, 6) => Op::Rem,
                (1, 7) => Op::Remu,
                (0x10, 2) => Op::Sh1add,
                (0x10, 4) => Op::Sh2add,
                (0x10, 6) => Op::Sh3add,
                (0x20, 7) => Op::Andn,
                (0x20, 6) => Op::Orn,
                (0x20, 4) => Op::Xnor,
                (0x05, 6) => Op::Max,
                (0x05, 7) => Op::Maxu,
                (0x05, 4) => Op::Min,
                (0x05, 5) => Op::Minu,
                (0x30, 1) => Op::Rol,
                (0x30, 5) => Op::Ror,
                (0x24, 1) => Op::Bclr,
                (0x24, 5) => Op::Bext,
                (0x34, 1) => Op::Binv,
                (0x14, 1) => Op::Bset,
                (0x07, 5) => Op::CzeroEqz,
                (0x07, 7) => Op::CzeroNez,
                _ => return illegal,
            };
            r(op)
        }
        0x3b => {
            let op = match (f7, f3) {
                (0, 0) => Op::Addw,
                (0x20, 0) => Op::Subw,
                (0, 1) => Op::Sllw,
                (0, 5) => Op::Srlw,
                (0x20, 5) => Op::Sraw,
                (1, 0) => Op::Mulw,
                (1, 4) => Op::Divw,
                (1, 5) => Op::Divuw,
                (1, 6) => Op::Remw,
                (1, 7) => Op::Remuw,
                (0x04, 0) => Op::AddUw,
                (0x04, 4) if rs2 == 0 => return insn(Op::ZextH, rd, rs1, 0, 0),
                (0x10, 2) => Op::Sh1addUw,
                (0x10, 4) => Op::Sh2addUw,
                (0x10, 6) => Op::Sh3addUw,
                (0x30, 1) => Op::Rolw,
                (0x30, 5) => Op::Rorw,
                _ => return illegal,
            };
            r(op)
        }
        0x0f => match f3 {
            // pred/succ (and fm) live in imm[11:0].
            0 => insn(Op::Fence, 0, 0, 0, bits(raw, 31, 20) as i32),
            1 => insn(Op::FenceI, 0, 0, 0, 0),
            2 if rd == 0 => match bits(raw, 31, 20) {
                0 => insn(Op::CboInval, 0, rs1, 0, 0),
                1 => insn(Op::CboClean, 0, rs1, 0, 0),
                2 => insn(Op::CboFlush, 0, rs1, 0, 0),
                4 => insn(Op::CboZero, 0, rs1, 0, 0),
                _ => illegal,
            },
            _ => illegal,
        },
        0x73 => decode_system(raw, rd, rs1, rs2, f3, f7),
        0x2f => decode_amo(raw, rd, rs1, rs2, f3),
        _ => illegal,
    }
}

fn decode_system(raw: u32, rd: u8, rs1: u8, rs2: u8, f3: u32, f7: u32) -> Insn {
    let insn = |op, rd, rs1, rs2, imm| Insn { op, rd, rs1, rs2, rs3: 0, len: 4, imm, raw };
    let csr = bits(raw, 31, 20) as i32;
    match f3 {
        0 => match raw {
            0x0000_0073 => insn(Op::Ecall, 0, 0, 0, 0),
            0x0010_0073 => insn(Op::Ebreak, 0, 0, 0, 0),
            0x3020_0073 => insn(Op::Mret, 0, 0, 0, 0),
            0x1020_0073 => insn(Op::Sret, 0, 0, 0, 0),
            0x1050_0073 => insn(Op::Wfi, 0, 0, 0, 0),
            0x00d0_0073 => insn(Op::WrsNto, 0, 0, 0, 0),
            0x01d0_0073 => insn(Op::WrsSto, 0, 0, 0, 0),
            0x1800_0073 => insn(Op::SfenceWInval, 0, 0, 0, 0),
            0x1810_0073 => insn(Op::SfenceInvalIr, 0, 0, 0, 0),
            _ if f7 == 0x09 && rd == 0 => insn(Op::SfenceVma, 0, rs1, rs2, 0),
            _ if f7 == 0x0b && rd == 0 => insn(Op::SinvalVma, 0, rs1, rs2, 0),
            _ => Insn::illegal(raw, 4),
        },
        // Zimop: mop.r.n is 1x00xx0111xx, mop.rr.n is 1x00xx1 in bits 31:25.
        4 if bit(raw, 31) == 1 && bits(raw, 29, 28) == 0 && bit(raw, 25) == 1 => {
            insn(Op::MopRr, rd, rs1, rs2, 0)
        }
        4 if bit(raw, 31) == 1 && bits(raw, 29, 28) == 0 && bits(raw, 25, 22) == 0b0111 => {
            insn(Op::MopR, rd, rs1, 0, 0)
        }
        1 => insn(Op::Csrrw, rd, rs1, 0, csr),
        2 => insn(Op::Csrrs, rd, rs1, 0, csr),
        3 => insn(Op::Csrrc, rd, rs1, 0, csr),
        // For the immediate forms rs1 holds the 5-bit zero-extended immediate.
        5 => insn(Op::Csrrwi, rd, rs1, 0, csr),
        6 => insn(Op::Csrrsi, rd, rs1, 0, csr),
        7 => insn(Op::Csrrci, rd, rs1, 0, csr),
        _ => Insn::illegal(raw, 4),
    }
}

fn decode_fma(raw: u32, rd: u8, rs1: u8, rs2: u8, rm: u32) -> Insn {
    let double = match bits(raw, 26, 25) {
        0 => false,
        1 => true,
        _ => return Insn::illegal(raw, 4),
    };
    let op = match (bits(raw, 6, 0), double) {
        (0x43, false) => Op::FmaddS,
        (0x47, false) => Op::FmsubS,
        (0x4b, false) => Op::FnmsubS,
        (0x4f, false) => Op::FnmaddS,
        (0x43, true) => Op::FmaddD,
        (0x47, true) => Op::FmsubD,
        (0x4b, true) => Op::FnmsubD,
        _ => Op::FnmaddD,
    };
    Insn { op, rd, rs1, rs2, rs3: bits(raw, 31, 27) as u8, len: 4, imm: rm as i32, raw }
}

fn decode_fp(raw: u32, rd: u8, rs1: u8, rs2: u8, rm: u32) -> Insn {
    use Op::*;
    let fmt = bits(raw, 26, 25);
    // Most operations exist for single (fmt 0) and double (fmt 1) precision only.
    let sd = |s: Op, d: Op| match fmt {
        0 => Some(s),
        1 => Some(d),
        _ => None,
    };
    let op = match bits(raw, 31, 27) {
        0x00 => sd(FaddS, FaddD),
        0x01 => sd(FsubS, FsubD),
        0x02 => sd(FmulS, FmulD),
        0x03 => sd(FdivS, FdivD),
        0x0b if rs2 == 0 => sd(FsqrtS, FsqrtD),
        0x04 => match rm {
            0 => sd(FsgnjS, FsgnjD),
            1 => sd(FsgnjnS, FsgnjnD),
            2 => sd(FsgnjxS, FsgnjxD),
            _ => None,
        },
        0x05 => match rm {
            0 => sd(FminS, FminD),
            1 => sd(FmaxS, FmaxD),
            2 => sd(FminmS, FminmD),
            3 => sd(FmaxmS, FmaxmD),
            _ => None,
        },
        0x08 => match (fmt, rs2) {
            (0, 1) => Some(FcvtSD),
            (1, 0) => Some(FcvtDS),
            (0, 2) => Some(FcvtSH),
            (2, 0) => Some(FcvtHS),
            (1, 2) => Some(FcvtDH),
            (2, 1) => Some(FcvtHD),
            (_, 4) => sd(FroundS, FroundD),
            (_, 5) => sd(FroundnxS, FroundnxD),
            _ => None,
        },
        0x14 => match rm {
            0 => sd(FleS, FleD),
            1 => sd(FltS, FltD),
            2 => sd(FeqS, FeqD),
            4 => sd(FleqS, FleqD),
            5 => sd(FltqS, FltqD),
            _ => None,
        },
        0x18 => match rs2 {
            0 => sd(FcvtWS, FcvtWD),
            1 => sd(FcvtWuS, FcvtWuD),
            2 => sd(FcvtLS, FcvtLD),
            3 => sd(FcvtLuS, FcvtLuD),
            8 if fmt == 1 && rm == 1 => Some(FcvtmodWD),
            _ => None,
        },
        0x1a => match rs2 {
            0 => sd(FcvtSW, FcvtDW),
            1 => sd(FcvtSWu, FcvtDWu),
            2 => sd(FcvtSL, FcvtDL),
            3 => sd(FcvtSLu, FcvtDLu),
            _ => None,
        },
        0x1c if rs2 == 0 => match (rm, fmt) {
            (0, 0) => Some(FmvXW),
            (0, 1) => Some(FmvXD),
            (0, 2) => Some(FmvXH),
            (1, _) => sd(FclassS, FclassD),
            _ => None,
        },
        0x1e if rm == 0 => match (rs2, fmt) {
            (0, 0) => Some(FmvWX),
            (0, 1) => Some(FmvDX),
            (0, 2) => Some(FmvHX),
            (1, _) => sd(FliS, FliD),
            _ => None,
        },
        _ => None,
    };
    match op {
        Some(op) => Insn { op, rd, rs1, rs2, rs3: 0, len: 4, imm: rm as i32, raw },
        None => Insn::illegal(raw, 4),
    }
}

fn decode_amo(raw: u32, rd: u8, rs1: u8, rs2: u8, f3: u32) -> Insn {
    let double = match f3 {
        2 => false,
        3 => true,
        _ => return Insn::illegal(raw, 4),
    };
    let pick = |w, d| if double { d } else { w };
    let op = match bits(raw, 31, 27) {
        0x02 if rs2 == 0 => pick(Op::LrW, Op::LrD),
        0x03 => pick(Op::ScW, Op::ScD),
        0x01 => pick(Op::AmoswapW, Op::AmoswapD),
        0x00 => pick(Op::AmoaddW, Op::AmoaddD),
        0x04 => pick(Op::AmoxorW, Op::AmoxorD),
        0x0c => pick(Op::AmoandW, Op::AmoandD),
        0x08 => pick(Op::AmoorW, Op::AmoorD),
        0x10 => pick(Op::AmominW, Op::AmominD),
        0x14 => pick(Op::AmomaxW, Op::AmomaxD),
        0x18 => pick(Op::AmominuW, Op::AmominuD),
        0x1c => pick(Op::AmomaxuW, Op::AmomaxuD),
        _ => return Insn::illegal(raw, 4),
    };
    // aq/rl go to rs3 so that the JIT can pick fence strength.
    Insn { op, rd, rs1, rs2, rs3: bits(raw, 26, 25) as u8, len: 4, imm: 0, raw }
}

fn decode16(raw16: u16) -> Insn {
    let raw = raw16 as u32;
    let illegal = Insn::illegal(raw, 2);
    let insn = |op, rd, rs1, rs2, imm| Insn { op, rd, rs1, rs2, rs3: 0, len: 2, imm, raw };

    // Full and "prime" (x8..x15) register fields.
    let rd_full = bits(raw, 11, 7) as u8;
    let rs2_full = bits(raw, 6, 2) as u8;
    let rd_p = bits(raw, 4, 2) as u8 + 8;
    let rs1_p = bits(raw, 9, 7) as u8 + 8;

    // Common immediates.
    let imm6 = sext((bit(raw, 12) << 5) | bits(raw, 6, 2), 6);
    let shamt6 = ((bit(raw, 12) << 5) | bits(raw, 6, 2)) as i32;
    // Scaled unsigned offsets for C.LW/C.SW and C.LD/C.SD/C.FLD/C.FSD.
    let uimm_w = ((bits(raw, 12, 10) << 3) | (bit(raw, 6) << 2) | (bit(raw, 5) << 6)) as i32;
    let uimm_d = ((bits(raw, 12, 10) << 3) | (bits(raw, 6, 5) << 6)) as i32;

    match (raw & 0b11, bits(raw, 15, 13)) {
        // ---- Quadrant 0 ----
        (0, 0b000) => {
            // C.ADDI4SPN
            let imm =
                (bits(raw, 12, 11) << 4) | (bits(raw, 10, 7) << 6) | (bit(raw, 6) << 2) | (bit(raw, 5) << 3);
            if imm == 0 {
                return illegal;
            }
            insn(Op::Addi, rd_p, 2, 0, imm as i32)
        }
        (0, 0b001) => insn(Op::Fld, rd_p, rs1_p, 0, uimm_d),
        (0, 0b010) => insn(Op::Lw, rd_p, rs1_p, 0, uimm_w),
        (0, 0b011) => insn(Op::Ld, rd_p, rs1_p, 0, uimm_d),
        (0, 0b100) => {
            // Zcb byte/halfword loads and stores.
            let b = (bit(raw, 5) << 1 | bit(raw, 6)) as i32;
            let h = (bit(raw, 5) << 1) as i32;
            match (bits(raw, 12, 10), bit(raw, 6)) {
                (0b000, _) => insn(Op::Lbu, rd_p, rs1_p, 0, b),
                (0b001, 0) => insn(Op::Lhu, rd_p, rs1_p, 0, h),
                (0b001, 1) => insn(Op::Lh, rd_p, rs1_p, 0, h),
                (0b010, _) => insn(Op::Sb, 0, rs1_p, rd_p, b),
                (0b011, 0) => insn(Op::Sh, 0, rs1_p, rd_p, h),
                _ => illegal,
            }
        }
        (0, 0b101) => insn(Op::Fsd, 0, rs1_p, rd_p, uimm_d),
        (0, 0b110) => insn(Op::Sw, 0, rs1_p, rd_p, uimm_w),
        (0, 0b111) => insn(Op::Sd, 0, rs1_p, rd_p, uimm_d),

        // ---- Quadrant 1 ----
        (1, 0b000) => insn(Op::Addi, rd_full, rd_full, 0, imm6), // C.ADDI / C.NOP
        (1, 0b001) => {
            if rd_full == 0 {
                return illegal;
            }
            insn(Op::Addiw, rd_full, rd_full, 0, imm6)
        }
        (1, 0b010) => insn(Op::Addi, rd_full, 0, 0, imm6), // C.LI
        (1, 0b011) => {
            if rd_full == 2 {
                // C.ADDI16SP
                let v = (bit(raw, 12) << 9)
                    | (bit(raw, 6) << 4)
                    | (bit(raw, 5) << 6)
                    | (bits(raw, 4, 3) << 7)
                    | (bit(raw, 2) << 5);
                if v == 0 {
                    return illegal;
                }
                insn(Op::Addi, 2, 2, 0, sext(v, 10))
            } else {
                // C.LUI; its reserved imm=0 encodings with odd rd < 16 are Zcmop's c.mop.n.
                if imm6 == 0 {
                    if bit(raw, 12) == 0 && rd_full % 2 == 1 && rd_full < 16 {
                        return insn(Op::MopR, 0, 0, 0, 0);
                    }
                    return illegal;
                }
                insn(Op::Lui, rd_full, 0, 0, imm6 << 12)
            }
        }
        (1, 0b100) => {
            let rd = rs1_p;
            match bits(raw, 11, 10) {
                0b00 => insn(Op::Srli, rd, rd, 0, shamt6),
                0b01 => insn(Op::Srai, rd, rd, 0, shamt6),
                0b10 => insn(Op::Andi, rd, rd, 0, imm6),
                _ => {
                    let rs2 = rd_p;
                    let op = match (bit(raw, 12), bits(raw, 6, 5)) {
                        (0, 0b00) => Op::Sub,
                        (0, 0b01) => Op::Xor,
                        (0, 0b10) => Op::Or,
                        (0, 0b11) => Op::And,
                        (1, 0b00) => Op::Subw,
                        (1, 0b01) => Op::Addw,
                        (1, 0b10) => Op::Mul,
                        (1, 0b11) => {
                            // Zcb unary operations, selected by bits 4:2.
                            return match bits(raw, 4, 2) {
                                0b000 => insn(Op::Andi, rd, rd, 0, 0xff),
                                0b001 => insn(Op::SextB, rd, rd, 0, 0),
                                0b010 => insn(Op::ZextH, rd, rd, 0, 0),
                                0b011 => insn(Op::SextH, rd, rd, 0, 0),
                                0b100 => insn(Op::AddUw, rd, rd, 0, 0),
                                0b101 => insn(Op::Xori, rd, rd, 0, -1),
                                _ => illegal,
                            };
                        }
                        _ => return illegal,
                    };
                    insn(op, rd, rd, rs2, 0)
                }
            }
        }
        (1, 0b101) => {
            // C.J
            let v = (bit(raw, 12) << 11)
                | (bit(raw, 11) << 4)
                | (bits(raw, 10, 9) << 8)
                | (bit(raw, 8) << 10)
                | (bit(raw, 7) << 6)
                | (bit(raw, 6) << 7)
                | (bits(raw, 5, 3) << 1)
                | (bit(raw, 2) << 5);
            insn(Op::Jal, 0, 0, 0, sext(v, 12))
        }
        (1, f3 @ (0b110 | 0b111)) => {
            // C.BEQZ / C.BNEZ
            let v = (bit(raw, 12) << 8)
                | (bits(raw, 11, 10) << 3)
                | (bits(raw, 6, 5) << 6)
                | (bits(raw, 4, 3) << 1)
                | (bit(raw, 2) << 5);
            let op = if f3 == 0b110 { Op::Beq } else { Op::Bne };
            insn(op, 0, rs1_p, 0, sext(v, 9))
        }

        // ---- Quadrant 2 ----
        (2, 0b000) => insn(Op::Slli, rd_full, rd_full, 0, shamt6),
        (2, 0b001) => {
            let off = (bit(raw, 12) << 5) | (bits(raw, 6, 5) << 3) | (bits(raw, 4, 2) << 6);
            insn(Op::Fld, rd_full, 2, 0, off as i32)
        }
        (2, 0b010) => {
            if rd_full == 0 {
                return illegal;
            }
            let off = (bit(raw, 12) << 5) | (bits(raw, 6, 4) << 2) | (bits(raw, 3, 2) << 6);
            insn(Op::Lw, rd_full, 2, 0, off as i32)
        }
        (2, 0b011) => {
            if rd_full == 0 {
                return illegal;
            }
            let off = (bit(raw, 12) << 5) | (bits(raw, 6, 5) << 3) | (bits(raw, 4, 2) << 6);
            insn(Op::Ld, rd_full, 2, 0, off as i32)
        }
        (2, 0b100) => match (bit(raw, 12), rd_full, rs2_full) {
            (0, 0, 0) => illegal,                          // reserved C.JR
            (0, rs1, 0) => insn(Op::Jalr, 0, rs1, 0, 0),   // C.JR
            (0, rd, rs2) => insn(Op::Add, rd, 0, rs2, 0),  // C.MV
            (1, 0, 0) => insn(Op::Ebreak, 0, 0, 0, 0),     // C.EBREAK
            (1, rs1, 0) => insn(Op::Jalr, 1, rs1, 0, 0),   // C.JALR
            (_, rd, rs2) => insn(Op::Add, rd, rd, rs2, 0), // C.ADD
        },
        (2, 0b101) => {
            let off = (bits(raw, 12, 10) << 3) | (bits(raw, 9, 7) << 6);
            insn(Op::Fsd, 0, 2, rs2_full, off as i32)
        }
        (2, 0b110) => {
            let off = (bits(raw, 12, 9) << 2) | (bits(raw, 8, 7) << 6);
            insn(Op::Sw, 0, 2, rs2_full, off as i32)
        }
        (2, 0b111) => {
            let off = (bits(raw, 12, 10) << 3) | (bits(raw, 9, 7) << 6);
            insn(Op::Sd, 0, 2, rs2_full, off as i32)
        }

        _ => illegal,
    }
}
