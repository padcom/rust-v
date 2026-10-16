//! Disassembly of vector instructions (decoding for execution lives in the vector unit).

use std::fmt;

use crate::{Insn, Op};

const fn field(raw: u32, hi: u32, lo: u32) -> u32 {
    (raw >> lo) & ((1 << (hi - lo + 1)) - 1)
}

/// Mnemonic stem for an OP-V instruction, without the operand-type suffix.
fn opv_stem(funct3: u32, funct6: u32, vs1: u32) -> Option<&'static str> {
    let int = matches!(funct3, 0 | 3 | 4);
    let mvx = matches!(funct3, 2 | 6);
    Some(match funct6 {
        0b000000 if int => "vadd",
        0b000001 if int => "vandn",
        0b000010 if int => "vsub",
        0b000011 if int => "vrsub",
        0b000100 if int => "vminu",
        0b000101 if int => "vmin",
        0b000110 if int => "vmaxu",
        0b000111 if int => "vmax",
        0b001001 if int => "vand",
        0b001010 if int => "vor",
        0b001011 if int => "vxor",
        0b001100 if int => "vrgather",
        0b001110 if funct3 == 0 => "vrgatherei16",
        0b001110 if int => "vslideup",
        0b001111 if int => "vslidedown",
        0b010000 if int => "vadc",
        0b010001 if int => "vmadc",
        0b010010 if int => "vsbc",
        0b010011 if int => "vmsbc",
        0b010100 if int => "vror",
        0b010101 if int && funct3 != 3 => "vrol",
        0b010101 if int => "vror",
        0b010111 if int => "vmerge",
        0b011000 if int => "vmseq",
        0b011001 if int => "vmsne",
        0b011010 if int => "vmsltu",
        0b011011 if int => "vmslt",
        0b011100 if int => "vmsleu",
        0b011101 if int => "vmsle",
        0b011110 if int => "vmsgtu",
        0b011111 if int => "vmsgt",
        0b100000 if int => "vsaddu",
        0b100001 if int => "vsadd",
        0b100010 if int => "vssubu",
        0b100011 if int => "vssub",
        0b100101 if int => "vsll",
        0b100111 if funct3 == 3 => "vmv<nr>r",
        0b100111 if int => "vsmul",
        0b101000 if int => "vsrl",
        0b101001 if int => "vsra",
        0b101010 if int => "vssrl",
        0b101011 if int => "vssra",
        0b101100 if int => "vnsrl",
        0b101101 if int => "vnsra",
        0b101110 if int => "vnclipu",
        0b101111 if int => "vnclip",
        0b110000 if int => "vwredsumu",
        0b110001 if int => "vwredsum",
        0b110101 if int => "vwsll",

        0b000000 if mvx => "vredsum",
        0b000001 if mvx => "vredand",
        0b000010 if mvx => "vredor",
        0b000011 if mvx => "vredxor",
        0b000100 if mvx => "vredminu",
        0b000101 if mvx => "vredmin",
        0b000110 if mvx => "vredmaxu",
        0b000111 if mvx => "vredmax",
        0b001000 if mvx => "vaaddu",
        0b001001 if mvx => "vaadd",
        0b001010 if mvx => "vasubu",
        0b001011 if mvx => "vasub",
        0b001110 if mvx => "vslide1up",
        0b001111 if mvx => "vslide1down",
        0b010000 if funct3 == 6 => "vmv.s.x",
        0b010000 if mvx => match vs1 {
            0b00000 => "vmv.x.s",
            0b10000 => "vcpop.m",
            0b10001 => "vfirst.m",
            _ => return None,
        },
        0b010010 if mvx => match vs1 {
            0b00010 => "vzext.vf8",
            0b00011 => "vsext.vf8",
            0b00100 => "vzext.vf4",
            0b00101 => "vsext.vf4",
            0b00110 => "vzext.vf2",
            0b00111 => "vsext.vf2",
            0b01000 => "vbrev8.v",
            0b01001 => "vrev8.v",
            0b01010 => "vbrev.v",
            0b01100 => "vclz.v",
            0b01101 => "vctz.v",
            0b01110 => "vcpop.v",
            _ => return None,
        },
        0b010100 if mvx => match vs1 {
            0b00001 => "vmsbf.m",
            0b00010 => "vmsof.m",
            0b00011 => "vmsif.m",
            0b10000 => "viota.m",
            0b10001 => "vid.v",
            _ => return None,
        },
        0b010111 if mvx => "vcompress",
        0b011000 if mvx => "vmandn",
        0b011001 if mvx => "vmand",
        0b011010 if mvx => "vmor",
        0b011011 if mvx => "vmxor",
        0b011100 if mvx => "vmorn",
        0b011101 if mvx => "vmnand",
        0b011110 if mvx => "vmnor",
        0b011111 if mvx => "vmxnor",
        0b100000 if mvx => "vdivu",
        0b100001 if mvx => "vdiv",
        0b100010 if mvx => "vremu",
        0b100011 if mvx => "vrem",
        0b100100 if mvx => "vmulhu",
        0b100101 if mvx => "vmul",
        0b100110 if mvx => "vmulhsu",
        0b100111 if mvx => "vmulh",
        0b101001 if mvx => "vmadd",
        0b101011 if mvx => "vnmsub",
        0b101101 if mvx => "vmacc",
        0b101111 if mvx => "vnmsac",
        0b110000 if mvx => "vwaddu",
        0b110001 if mvx => "vwadd",
        0b110010 if mvx => "vwsubu",
        0b110011 if mvx => "vwsub",
        0b110100 if mvx => "vwaddu.w",
        0b110101 if mvx => "vwadd.w",
        0b110110 if mvx => "vwsubu.w",
        0b110111 if mvx => "vwsub.w",
        0b111000 if mvx => "vwmulu",
        0b111010 if mvx => "vwmulsu",
        0b111011 if mvx => "vwmul",
        0b111100 if mvx => "vwmaccu",
        0b111101 if mvx => "vwmacc",
        0b111110 if mvx => "vwmaccus",
        0b111111 if mvx => "vwmaccsu",

        // OPFVV / OPFVF
        0b000000 => "vfadd",
        0b000001 => "vfredusum",
        0b000010 => "vfsub",
        0b000011 => "vfredosum",
        0b000100 => "vfmin",
        0b000101 => "vfredmin",
        0b000110 => "vfmax",
        0b000111 => "vfredmax",
        0b001000 => "vfsgnj",
        0b001001 => "vfsgnjn",
        0b001010 => "vfsgnjx",
        0b001110 => "vfslide1up",
        0b001111 => "vfslide1down",
        0b010000 if funct3 == 5 => "vfmv.s.f",
        0b010000 => "vfmv.f.s",
        0b010010 => match vs1 {
            0b00000 => "vfcvt.xu.f.v",
            0b00001 => "vfcvt.x.f.v",
            0b00010 => "vfcvt.f.xu.v",
            0b00011 => "vfcvt.f.x.v",
            0b00110 => "vfcvt.rtz.xu.f.v",
            0b00111 => "vfcvt.rtz.x.f.v",
            0b01000 => "vfwcvt.xu.f.v",
            0b01001 => "vfwcvt.x.f.v",
            0b01010 => "vfwcvt.f.xu.v",
            0b01011 => "vfwcvt.f.x.v",
            0b01100 => "vfwcvt.f.f.v",
            0b01110 => "vfwcvt.rtz.xu.f.v",
            0b01111 => "vfwcvt.rtz.x.f.v",
            0b10000 => "vfncvt.xu.f.w",
            0b10001 => "vfncvt.x.f.w",
            0b10010 => "vfncvt.f.xu.w",
            0b10011 => "vfncvt.f.x.w",
            0b10100 => "vfncvt.f.f.w",
            0b10101 => "vfncvt.rod.f.f.w",
            0b10110 => "vfncvt.rtz.xu.f.w",
            0b10111 => "vfncvt.rtz.x.f.w",
            _ => return None,
        },
        0b010011 => match vs1 {
            0b00000 => "vfsqrt.v",
            0b00100 => "vfrsqrt7.v",
            0b00101 => "vfrec7.v",
            0b10000 => "vfclass.v",
            _ => return None,
        },
        0b010111 => "vfmerge",
        0b011000 => "vmfeq",
        0b011001 => "vmfle",
        0b011011 => "vmflt",
        0b011100 => "vmfne",
        0b011101 => "vmfgt",
        0b011111 => "vmfge",
        0b100000 => "vfdiv",
        0b100001 => "vfrdiv",
        0b100100 => "vfmul",
        0b100111 => "vfrsub",
        0b101000 => "vfmadd",
        0b101001 => "vfnmadd",
        0b101010 => "vfmsub",
        0b101011 => "vfnmsub",
        0b101100 => "vfmacc",
        0b101101 => "vfnmacc",
        0b101110 => "vfmsac",
        0b101111 => "vfnmsac",
        0b110000 => "vfwadd",
        0b110001 => "vfwredusum",
        0b110010 => "vfwsub",
        0b110011 => "vfwredosum",
        0b110100 => "vfwadd.w",
        0b110110 => "vfwsub.w",
        0b111000 => "vfwmul",
        0b111100 => "vfwmacc",
        0b111101 => "vfwnmacc",
        0b111110 => "vfwmsac",
        0b111111 => "vfwnmsac",
        _ => return None,
    })
}

/// Writes a vector instruction in assembler-like syntax.
pub(crate) fn fmt_vector(insn: &Insn, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let raw = insn.raw;
    let vm = if field(raw, 25, 25) == 0 { ", v0.t" } else { "" };
    match insn.op {
        Op::Vload | Op::Vstore => {
            let eew = match field(raw, 14, 12) {
                0 => 8,
                5 => 16,
                6 => 32,
                _ => 64,
            };
            let nf = field(raw, 31, 29) + 1;
            let seg = if nf > 1 { format!("seg{nf}") } else { String::new() };
            let dir = if insn.op == Op::Vload { 'l' } else { 's' };
            let base = crate::reg_name(insn.rs1);
            match (field(raw, 27, 26), insn.rs2) {
                (0, 0b01000) => write!(f, "v{dir}{nf}r.v v{}, ({base})", insn.rd),
                (0, 0b01011) => write!(f, "v{dir}m.v v{}, ({base})", insn.rd),
                (0, 0b10000) => write!(f, "v{dir}{seg}e{eew}ff.v v{}, ({base}){vm}", insn.rd),
                (0, _) => write!(f, "v{dir}{seg}e{eew}.v v{}, ({base}){vm}", insn.rd),
                (2, rs2) => {
                    write!(f, "v{dir}s{seg}e{eew}.v v{}, ({base}), {}{vm}", insn.rd, crate::reg_name(rs2))
                }
                (mop, vs2) => {
                    let order = if mop == 3 { 'o' } else { 'u' };
                    write!(f, "v{dir}{order}xseg{nf}ei{eew}.v v{}, ({base}), v{vs2}{vm}", insn.rd)
                }
            }
        }
        _ => {
            let funct3 = field(raw, 14, 12);
            let Some(stem) = opv_stem(funct3, field(raw, 31, 26), insn.rs1 as u32) else {
                return write!(f, "vop {raw:#010x}");
            };
            let (vd, vs2) = (insn.rd, insn.rs2);
            if stem.contains('.') {
                // Unary forms with a fixed operand layout.
                return match stem {
                    "vmv.x.s" | "vcpop.m" | "vfirst.m" => {
                        write!(f, "{stem} {}, v{vs2}{vm}", crate::reg_name(vd))
                    }
                    "vmv.s.x" => write!(f, "{stem} v{vd}, {}", crate::reg_name(insn.rs1)),
                    "vfmv.f.s" => write!(f, "{stem} f{vd}, v{vs2}"),
                    "vfmv.s.f" => write!(f, "{stem} v{vd}, f{}", insn.rs1),
                    _ => write!(f, "{stem} v{vd}, v{vs2}{vm}"),
                };
            }
            let (suffix, op1) = match funct3 {
                0..=2 => ("vv", format!("v{}", insn.rs1)),
                3 => ("vi", format!("{}", ((insn.rs1 as i8) << 3) >> 3)),
                4 | 6 => ("vx", crate::reg_name(insn.rs1).to_string()),
                _ => ("vf", format!("f{}", insn.rs1)),
            };
            write!(f, "{stem}.{suffix} v{vd}, v{vs2}, {op1}{vm}")
        }
    }
}
