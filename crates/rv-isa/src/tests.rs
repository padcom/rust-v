use crate::{Op, decode};

/// (encoding, expected disassembly). Encodings were produced by GNU as from the assembly in
/// the trailing comment; compressed forms disassemble as their 32-bit expansion.
const VECTORS: &[(u32, &str)] = &[
    (0x12345537, "lui a0, 0x12345"),        // lui a0, 0x12345
    (0xffffffb7, "lui t6, 0xfffff"),        // lui t6, 0xfffff
    (0x80000117, "auipc sp, 0x80000"),      // auipc sp, 0x80000
    (0x001000ef, "jal ra, pc+2048"),        // jal ra, .+2048
    (0x8000006f, "jal zero, pc-1048576"),   // jal zero, .-1048576
    (0xfff780e7, "jalr ra, -1(a5)"),        // jalr ra, -1(a5)
    (0x7eb50fe3, "beq a0, a1, pc+4094"),    // beq a0, a1, .+4094
    (0x80629063, "bne t0, t1, pc-4096"),    // bne t0, t1, .-4096
    (0x00944463, "blt s0, s1, pc+8"),       // blt s0, s1, .+8
    (0xfed65fe3, "bge a2, a3, pc-2"),       // bge a2, a3, .-2
    (0x04f76063, "bltu a4, a5, pc+64"),     // bltu a4, a5, .+64
    (0xfd1870e3, "bgeu a6, a7, pc-64"),     // bgeu a6, a7, .-64
    (0x80040503, "lb a0, -2048(s0)"),       // lb a0, -2048(s0)
    (0x7ff41503, "lh a0, 2047(s0)"),        // lh a0, 2047(s0)
    (0x00412383, "lw t2, 4(sp)"),           // lw t2, 4(sp)
    (0xff8fbd83, "ld s11, -8(t6)"),         // ld s11, -8(t6)
    (0x00154503, "lbu a0, 1(a0)"),          // lbu a0, 1(a0)
    (0x00255503, "lhu a0, 2(a0)"),          // lhu a0, 2(a0)
    (0x00356503, "lwu a0, 3(a0)"),          // lwu a0, 3(a0)
    (0xfea58fa3, "sb a0, -1(a1)"),          // sb a0, -1(a1)
    (0x7ec69fa3, "sh a2, 2047(a3)"),        // sh a2, 2047(a3)
    (0x80e7a023, "sw a4, -2048(a5)"),       // sw a4, -2048(a5)
    (0x00113423, "sd ra, 8(sp)"),           // sd ra, 8(sp)
    (0xffb58513, "addi a0, a1, -5"),        // addi a0, a1, -5
    (0x0075a513, "slti a0, a1, 7"),         // slti a0, a1, 7
    (0xfff5b513, "sltiu a0, a1, -1"),       // sltiu a0, a1, -1
    (0x5555c513, "xori a0, a1, 1365"),      // xori a0, a1, 0x555
    (0xaaa5e513, "ori a0, a1, -1366"),      // ori a0, a1, -1366
    (0x0ff5f513, "andi a0, a1, 255"),       // andi a0, a1, 255
    (0x03f59513, "slli a0, a1, 63"),        // slli a0, a1, 63
    (0x0205d513, "srli a0, a1, 32"),        // srli a0, a1, 32
    (0x4015d513, "srai a0, a1, 1"),         // srai a0, a1, 1
    (0x00c58533, "add a0, a1, a2"),         // add a0, a1, a2
    (0x40c58533, "sub a0, a1, a2"),         // sub a0, a1, a2
    (0x00c59533, "sll a0, a1, a2"),         // sll a0, a1, a2
    (0x00c5a533, "slt a0, a1, a2"),         // slt a0, a1, a2
    (0x00c5b533, "sltu a0, a1, a2"),        // sltu a0, a1, a2
    (0x00c5c533, "xor a0, a1, a2"),         // xor a0, a1, a2
    (0x00c5d533, "srl a0, a1, a2"),         // srl a0, a1, a2
    (0x40c5d533, "sra a0, a1, a2"),         // sra a0, a1, a2
    (0x00c5e533, "or a0, a1, a2"),          // or a0, a1, a2
    (0x00c5f533, "and a0, a1, a2"),         // and a0, a1, a2
    (0xfff5851b, "addiw a0, a1, -1"),       // addiw a0, a1, -1
    (0x01f5951b, "slliw a0, a1, 31"),       // slliw a0, a1, 31
    (0x01f5d51b, "srliw a0, a1, 31"),       // srliw a0, a1, 31
    (0x41f5d51b, "sraiw a0, a1, 31"),       // sraiw a0, a1, 31
    (0x00c5853b, "addw a0, a1, a2"),        // addw a0, a1, a2
    (0x40c5853b, "subw a0, a1, a2"),        // subw a0, a1, a2
    (0x00c5953b, "sllw a0, a1, a2"),        // sllw a0, a1, a2
    (0x00c5d53b, "srlw a0, a1, a2"),        // srlw a0, a1, a2
    (0x40c5d53b, "sraw a0, a1, a2"),        // sraw a0, a1, a2
    (0x0310000f, "fence 0x3, 0x1"),         // fence rw, w
    (0x0000100f, "fence.i"),                // fence.i
    (0x00000073, "ecall"),                  // ecall
    (0x00100073, "ebreak"),                 // ebreak
    (0x30200073, "mret"),                   // mret
    (0x10200073, "sret"),                   // sret
    (0x10500073, "wfi"),                    // wfi
    (0x12b50073, "sfence.vma a0, a1"),      // sfence.vma a0, a1
    (0x30059573, "csrrw a0, 0x300, a1"),    // csrrw a0, 0x300, a1
    (0xc0002573, "csrrs a0, 0xc00, zero"),  // csrrs a0, 0xc00, zero
    (0xfff2b073, "csrrc zero, 0xfff, t0"),  // csrrc zero, 0xfff, t0
    (0x340fd573, "csrrwi a0, 0x340, 31"),   // csrrwi a0, 0x340, 31
    (0x1000e573, "csrrsi a0, 0x100, 1"),    // csrrsi a0, 0x100, 1
    (0x14417573, "csrrci a0, 0x144, 2"),    // csrrci a0, 0x144, 2
    (0x02c58533, "mul a0, a1, a2"),         // mul a0, a1, a2
    (0x02c59533, "mulh a0, a1, a2"),        // mulh a0, a1, a2
    (0x02c5a533, "mulhsu a0, a1, a2"),      // mulhsu a0, a1, a2
    (0x02c5b533, "mulhu a0, a1, a2"),       // mulhu a0, a1, a2
    (0x02c5c533, "div a0, a1, a2"),         // div a0, a1, a2
    (0x02c5d533, "divu a0, a1, a2"),        // divu a0, a1, a2
    (0x02c5e533, "rem a0, a1, a2"),         // rem a0, a1, a2
    (0x02c5f533, "remu a0, a1, a2"),        // remu a0, a1, a2
    (0x02c5853b, "mulw a0, a1, a2"),        // mulw a0, a1, a2
    (0x02c5c53b, "divw a0, a1, a2"),        // divw a0, a1, a2
    (0x02c5d53b, "divuw a0, a1, a2"),       // divuw a0, a1, a2
    (0x02c5e53b, "remw a0, a1, a2"),        // remw a0, a1, a2
    (0x02c5f53b, "remuw a0, a1, a2"),       // remuw a0, a1, a2
    (0x1005a52f, "lr.w a0, (a1)"),          // lr.w a0, (a1)
    (0x18c5a52f, "sc.w a0, a2, (a1)"),      // sc.w a0, a2, (a1)
    (0x08c5a52f, "amoswap.w a0, a2, (a1)"), // amoswap.w a0, a2, (a1)
    (0x00c5a52f, "amoadd.w a0, a2, (a1)"),  // amoadd.w a0, a2, (a1)
    (0x20c5a52f, "amoxor.w a0, a2, (a1)"),  // amoxor.w a0, a2, (a1)
    (0x60c5a52f, "amoand.w a0, a2, (a1)"),  // amoand.w a0, a2, (a1)
    (0x40c5a52f, "amoor.w a0, a2, (a1)"),   // amoor.w a0, a2, (a1)
    (0x80c5a52f, "amomin.w a0, a2, (a1)"),  // amomin.w a0, a2, (a1)
    (0xa0c5a52f, "amomax.w a0, a2, (a1)"),  // amomax.w a0, a2, (a1)
    (0xc0c5a52f, "amominu.w a0, a2, (a1)"), // amominu.w a0, a2, (a1)
    (0xe0c5a52f, "amomaxu.w a0, a2, (a1)"), // amomaxu.w a0, a2, (a1)
    (0x1605b52f, "lr.d a0, (a1)"),          // lr.d.aqrl a0, (a1)
    (0x1ac5b52f, "sc.d a0, a2, (a1)"),      // sc.d.rl a0, a2, (a1)
    (0x0cc5b52f, "amoswap.d a0, a2, (a1)"), // amoswap.d.aq a0, a2, (a1)
    (0x00c5b52f, "amoadd.d a0, a2, (a1)"),  // amoadd.d a0, a2, (a1)
    (0x20c5b52f, "amoxor.d a0, a2, (a1)"),  // amoxor.d a0, a2, (a1)
    (0x60c5b52f, "amoand.d a0, a2, (a1)"),  // amoand.d a0, a2, (a1)
    (0x40c5b52f, "amoor.d a0, a2, (a1)"),   // amoor.d a0, a2, (a1)
    (0x80c5b52f, "amomin.d a0, a2, (a1)"),  // amomin.d a0, a2, (a1)
    (0xa0c5b52f, "amomax.d a0, a2, (a1)"),  // amomax.d a0, a2, (a1)
    (0xc0c5b52f, "amominu.d a0, a2, (a1)"), // amominu.d a0, a2, (a1)
    (0xe0c5b52f, "amomaxu.d a0, a2, (a1)"), // amomaxu.d a0, a2, (a1)
    (0x00452087, "flw f1, 4(a0)"),          // flw f1, 4(a0)
    (0xff813f87, "fld f31, -8(sp)"),        // fld f31, -8(sp)
    (0x00252627, "fsw f2, 12(a0)"),         // fsw f2, 12(a0)
    (0x7e353c27, "fsd f3, 2040(a0)"),       // fsd f3, 2040(a0)
    (0x1fe8, "addi a0, sp, 1020"),          // c.addi4spn a0, sp, 1020
    (0x0044, "addi s1, sp, 4"),             // c.addi4spn s1, sp, 4
    (0x3fe0, "fld f8, 248(a5)"),            // c.fld f8, 248(a5)
    (0x5de8, "lw a0, 124(a1)"),             // c.lw a0, 124(a1)
    (0x40c0, "lw s0, 4(s1)"),               // c.lw s0, 4(s1)
    (0x7de8, "ld a0, 248(a1)"),             // c.ld a0, 248(a1)
    (0xa504, "fsd f9, 8(a0)"),              // c.fsd f9, 8(a0)
    (0xc2b0, "sw a2, 64(a3)"),              // c.sw a2, 64(a3)
    (0xe3d8, "sd a4, 128(a5)"),             // c.sd a4, 128(a5)
    (0x0001, "addi zero, zero, 0"),         // c.nop
    (0x1501, "addi a0, a0, -32"),           // c.addi a0, -32
    (0x0ffd, "addi t6, t6, 31"),            // c.addi t6, 31
    (0x357d, "addiw a0, a0, -1"),           // c.addiw a0, -1
    (0x5781, "addi a5, zero, -32"),         // c.li a5, -32
    (0x40fd, "addi ra, zero, 31"),          // c.li ra, 31
    (0x7101, "addi sp, sp, -512"),          // c.addi16sp sp, -512
    (0x617d, "addi sp, sp, 496"),           // c.addi16sp sp, 496
    (0x6141, "addi sp, sp, 16"),            // c.addi16sp sp, 16
    (0x7501, "lui a0, 0xfffe0"),            // c.lui a0, 0xfffe0
    (0x62fd, "lui t0, 0x1f"),               // c.lui t0, 31
    (0x917d, "srli a0, a0, 63"),            // c.srli a0, 63
    (0x8485, "srai s1, s1, 1"),             // c.srai s1, 1
    (0x9a7d, "andi a2, a2, -1"),            // c.andi a2, -1
    (0x8d0d, "sub a0, a0, a1"),             // c.sub a0, a1
    (0x8c25, "xor s0, s0, s1"),             // c.xor s0, s1
    (0x8f5d, "or a4, a4, a5"),              // c.or a4, a5
    (0x8e75, "and a2, a2, a3"),             // c.and a2, a3
    (0x9d0d, "subw a0, a0, a1"),            // c.subw a0, a1
    (0x9d2d, "addw a0, a0, a1"),            // c.addw a0, a1
    (0xaffd, "jal zero, pc+2046"),          // c.j .+2046
    (0xb001, "jal zero, pc-2048"),          // c.j .-2048
    (0xa46d, "jal zero, pc+682"),           // c.j .+0x2aa
    (0xcd7d, "beq a0, zero, pc+254"),       // c.beqz a0, .+254
    (0xf081, "bne s1, zero, pc-256"),       // c.bnez s1, .-256
    (0xebb9, "bne a5, zero, pc+86"),        // c.bnez a5, .+0x56
    (0x157e, "slli a0, a0, 63"),            // c.slli a0, 63
    (0x30fe, "fld f1, 504(sp)"),            // c.fldsp f1, 504(sp)
    (0x50fe, "lw ra, 252(sp)"),             // c.lwsp ra, 252(sp)
    (0x72fe, "ld t0, 504(sp)"),             // c.ldsp t0, 504(sp)
    (0x6422, "ld s0, 8(sp)"),               // c.ldsp s0, 8(sp)
    (0x8082, "jalr zero, 0(ra)"),           // c.jr ra
    (0x852e, "add a0, zero, a1"),           // c.mv a0, a1
    (0x9002, "ebreak"),                     // c.ebreak
    (0x9282, "jalr ra, 0(t0)"),             // c.jalr t0
    (0x952e, "add a0, a0, a1"),             // c.add a0, a1
    (0xbf86, "fsd f1, 504(sp)"),            // c.fsdsp f1, 504(sp)
    (0xdfaa, "sw a0, 252(sp)"),             // c.swsp a0, 252(sp)
    (0xffae, "sd a1, 504(sp)"),             // c.sdsp a1, 504(sp)
    // F, D, Zfa, Zfhmin
    (0xffe51087, "flh f1, -2(a0)"),              // flh f1, -2(a0)
    (0x00259327, "fsh f2, 6(a1)"),               // fsh f2, 6(a1)
    (0x003170d3, "fadd.s f1, f2, f3"),           // fadd.s f1, f2, f3
    (0x0bdf1fd3, "fsub.d f31, f30, f29"),        // fsub.d f31, f30, f29, rtz
    (0x1062f253, "fmul.s f4, f5, f6"),           // fmul.s f4, f5, f6, dyn
    (0x1a9473d3, "fdiv.d f7, f8, f9"),           // fdiv.d f7, f8, f9
    (0x5805f553, "fsqrt.s f10, f11"),            // fsqrt.s f10, f11
    (0x5a05b553, "fsqrt.d f10, f11"),            // fsqrt.d f10, f11, rup
    (0x203100d3, "fsgnj.s f1, f2, f3"),          // fsgnj.s f1, f2, f3
    (0x223110d3, "fsgnjn.d f1, f2, f3"),         // fsgnjn.d f1, f2, f3
    (0x203120d3, "fsgnjx.s f1, f2, f3"),         // fsgnjx.s f1, f2, f3
    (0x2a3100d3, "fmin.d f1, f2, f3"),           // fmin.d f1, f2, f3
    (0x283110d3, "fmax.s f1, f2, f3"),           // fmax.s f1, f2, f3
    (0x283120d3, "fminm.s f1, f2, f3"),          // fminm.s f1, f2, f3
    (0x2a3130d3, "fmaxm.d f1, f2, f3"),          // fmaxm.d f1, f2, f3
    (0xa020a553, "feq.s a0, f1, f2"),            // feq.s a0, f1, f2
    (0xa24195d3, "flt.d a1, f3, f4"),            // flt.d a1, f3, f4
    (0xa0628653, "fle.s a2, f5, f6"),            // fle.s a2, f5, f6
    (0xa283d6d3, "fltq.d a3, f7, f8"),           // fltq.d a3, f7, f8
    (0xa0a4c753, "fleq.s a4, f9, f10"),          // fleq.s a4, f9, f10
    (0xe20612d3, "fclass.d t0, f12"),            // fclass.d t0, f12
    (0x203170c3, "fmadd.s f1, f2, f3, f4"),      // fmadd.s f1, f2, f3, f4
    (0x427302c7, "fmsub.d f5, f6, f7, f8"),      // fmsub.d f5, f6, f7, f8, rne
    (0x60b574cb, "fnmsub.s f9, f10, f11, f12"),  // fnmsub.s f9, f10, f11, f12
    (0xfbeefe4f, "fnmadd.d f28, f29, f30, f31"), // fnmadd.d f28, f29, f30, f31
    (0xf01800d3, "fli.s f1, 16"),                // fli.s f1, 1.0
    (0xf2108153, "fli.d f2, 1"),                 // fli.d f2, min
    (0xf21f01d3, "fli.d f3, 30"),                // fli.d f3, inf
    (0x404170d3, "fround.s f1, f2"),             // fround.s f1, f2
    (0x425241d3, "froundnx.d f3, f4"),           // froundnx.d f3, f4, rmm
    (0xc0009553, "fcvt.w.s a0, f1"),             // fcvt.w.s a0, f1, rtz
    (0xc210f553, "fcvt.wu.d a0, f1"),            // fcvt.wu.d a0, f1
    (0xc020f553, "fcvt.l.s a0, f1"),             // fcvt.l.s a0, f1
    (0xc230f553, "fcvt.lu.d a0, f1"),            // fcvt.lu.d a0, f1
    (0xd00570d3, "fcvt.s.w f1, a0"),             // fcvt.s.w f1, a0
    (0xd21500d3, "fcvt.d.wu f1, a0"),            // fcvt.d.wu f1, a0
    (0xd02570d3, "fcvt.s.l f1, a0"),             // fcvt.s.l f1, a0
    (0xd23570d3, "fcvt.d.lu f1, a0"),            // fcvt.d.lu f1, a0
    (0xe0008553, "fmv.x.w a0, f1"),              // fmv.x.w a0, f1
    (0xf00500d3, "fmv.w.x f1, a0"),              // fmv.w.x f1, a0
    (0xe2008553, "fmv.x.d a0, f1"),              // fmv.x.d a0, f1
    (0xf20500d3, "fmv.d.x f1, a0"),              // fmv.d.x f1, a0
    (0xe4008553, "fmv.x.h a0, f1"),              // fmv.x.h a0, f1
    (0xf40500d3, "fmv.h.x f1, a0"),              // fmv.h.x f1, a0
    (0x401170d3, "fcvt.s.d f1, f2"),             // fcvt.s.d f1, f2
    (0x420100d3, "fcvt.d.s f1, f2"),             // fcvt.d.s f1, f2
    (0x402100d3, "fcvt.s.h f1, f2"),             // fcvt.s.h f1, f2
    (0x440170d3, "fcvt.h.s f1, f2"),             // fcvt.h.s f1, f2
    (0x422100d3, "fcvt.d.h f1, f2"),             // fcvt.d.h f1, f2
    (0x441170d3, "fcvt.h.d f1, f2"),             // fcvt.h.d f1, f2
    (0xc2809553, "fcvtmod.w.d a0, f1"),          // fcvtmod.w.d a0, f1, rtz
    // Zba, Zbb, Zbs, Zicond, Zimop, Zcmop, Zawrs, Zicbom, Zicboz, Zicbop, Svinval, Zcb
    (0x20c5a533, "sh1add a0, a1, a2"),    // sh1add a0, a1, a2
    (0x20c5c533, "sh2add a0, a1, a2"),    // sh2add a0, a1, a2
    (0x20c5e533, "sh3add a0, a1, a2"),    // sh3add a0, a1, a2
    (0x08c5853b, "add.uw a0, a1, a2"),    // add.uw a0, a1, a2
    (0x20c5a53b, "sh1add.uw a0, a1, a2"), // sh1add.uw a0, a1, a2
    (0x20c5c53b, "sh2add.uw a0, a1, a2"), // sh2add.uw a0, a1, a2
    (0x20c5e53b, "sh3add.uw a0, a1, a2"), // sh3add.uw a0, a1, a2
    (0x0bf5951b, "slli.uw a0, a1, 63"),   // slli.uw a0, a1, 63
    (0x40c5f533, "andn a0, a1, a2"),      // andn a0, a1, a2
    (0x40c5e533, "orn a0, a1, a2"),       // orn a0, a1, a2
    (0x40c5c533, "xnor a0, a1, a2"),      // xnor a0, a1, a2
    (0x60059513, "clz a0, a1"),           // clz a0, a1
    (0x6005951b, "clzw a0, a1"),          // clzw a0, a1
    (0x60159513, "ctz a0, a1"),           // ctz a0, a1
    (0x6015951b, "ctzw a0, a1"),          // ctzw a0, a1
    (0x60259513, "cpop a0, a1"),          // cpop a0, a1
    (0x6025951b, "cpopw a0, a1"),         // cpopw a0, a1
    (0x0ac5e533, "max a0, a1, a2"),       // max a0, a1, a2
    (0x0ac5f533, "maxu a0, a1, a2"),      // maxu a0, a1, a2
    (0x0ac5c533, "min a0, a1, a2"),       // min a0, a1, a2
    (0x0ac5d533, "minu a0, a1, a2"),      // minu a0, a1, a2
    (0x60459513, "sext.b a0, a1"),        // sext.b a0, a1
    (0x60559513, "sext.h a0, a1"),        // sext.h a0, a1
    (0x0805c53b, "zext.h a0, a1"),        // zext.h a0, a1
    (0x60c59533, "rol a0, a1, a2"),       // rol a0, a1, a2
    (0x60c5953b, "rolw a0, a1, a2"),      // rolw a0, a1, a2
    (0x60c5d533, "ror a0, a1, a2"),       // ror a0, a1, a2
    (0x60c5d53b, "rorw a0, a1, a2"),      // rorw a0, a1, a2
    (0x63f5d513, "rori a0, a1, 63"),      // rori a0, a1, 63
    (0x61f5d51b, "roriw a0, a1, 31"),     // roriw a0, a1, 31
    (0x2875d513, "orc.b a0, a1"),         // orc.b a0, a1
    (0x6b85d513, "rev8 a0, a1"),          // rev8 a0, a1
    (0x48c59533, "bclr a0, a1, a2"),      // bclr a0, a1, a2
    (0x4bf59513, "bclri a0, a1, 63"),     // bclri a0, a1, 63
    (0x48c5d533, "bext a0, a1, a2"),      // bext a0, a1, a2
    (0x4a15d513, "bexti a0, a1, 33"),     // bexti a0, a1, 33
    (0x68c59533, "binv a0, a1, a2"),      // binv a0, a1, a2
    (0x68159513, "binvi a0, a1, 1"),      // binvi a0, a1, 1
    (0x28c59533, "bset a0, a1, a2"),      // bset a0, a1, a2
    (0x2a859513, "bseti a0, a1, 40"),     // bseti a0, a1, 40
    (0x0ec5d533, "czero.eqz a0, a1, a2"), // czero.eqz a0, a1, a2
    (0x0ec5f533, "czero.nez a0, a1, a2"), // czero.nez a0, a1, a2
    (0x81c5c573, "mop.r a0, a1"),         // mop.r.0 a0, a1
    (0xcdf342f3, "mop.r t0, t1"),         // mop.r.31 t0, t1
    (0x82c5c573, "mop.rr a0, a1, a2"),    // mop.rr.0 a0, a1, a2
    (0xcef746f3, "mop.rr a3, a4, a5"),    // mop.rr.7 a3, a4, a5
    (0x00d00073, "wrs.nto"),              // wrs.nto
    (0x01d00073, "wrs.sto"),              // wrs.sto
    (0x0015200f, "cbo.clean (a0)"),       // cbo.clean (a0)
    (0x0025a00f, "cbo.flush (a1)"),       // cbo.flush (a1)
    (0x0006200f, "cbo.inval (a2)"),       // cbo.inval (a2)
    (0x0046a00f, "cbo.zero (a3)"),        // cbo.zero (a3)
    (0x04156013, "ori zero, a0, 65"),     // prefetch.r 64(a0)
    (0x16b50073, "sinval.vma a0, a1"),    // sinval.vma a0, a1
    (0x18000073, "sfence.w.inval"),       // sfence.w.inval
    (0x18100073, "sfence.inval.ir"),      // sfence.inval.ir
    (0x81e8, "lbu a0, 3(a1)"),            // c.lbu a0, 3(a1)
    (0x86b0, "lhu a2, 2(a3)"),            // c.lhu a2, 2(a3)
    (0x87d8, "lh a4, 0(a5)"),             // c.lh a4, 0(a5)
    (0x89c8, "sb a0, 1(a1)"),             // c.sb a0, 1(a1)
    (0x8ca0, "sh s0, 2(s1)"),             // c.sh s0, 2(s1)
    (0x9d61, "andi a0, a0, 255"),         // c.zext.b a0
    (0x9de5, "sext.b a1, a1"),            // c.sext.b a1
    (0x9e69, "zext.h a2, a2"),            // c.zext.h a2
    (0x9eed, "sext.h a3, a3"),            // c.sext.h a3
    (0x9f71, "add.uw a4, a4, zero"),      // c.zext.w a4
    (0x9ff5, "xori a5, a5, -1"),          // c.not a5
    (0x9c45, "mul s0, s0, s1"),           // c.mul s0, s1
    (0x6081, "mop.r zero, zero"),         // c.mop.1
    (0x6781, "mop.r zero, zero"),         // c.mop.15
];

#[test]
fn decodes_known_encodings() {
    let mut failures = Vec::new();
    for &(raw, expected) in VECTORS {
        let insn = decode(raw);
        let got = insn.to_string();
        let len = if raw & 3 == 3 { 4 } else { 2 };
        if got != expected || insn.len != len {
            failures.push(format!(
                "{raw:#010x}: expected `{expected}` (len {len}), got `{got}` (len {})",
                insn.len
            ));
        }
    }
    assert!(failures.is_empty(), "decode mismatches:\n{}", failures.join("\n"));
}

#[test]
fn rejects_reserved_encodings() {
    let reserved: &[u32] = &[
        0x0000,                                  // all-zero parcel
        0xffff_ffff,                             // all-ones
        0x6101,                                  // c.addi16sp with imm=0
        0x6501,                                  // c.lui with imm=0
        0x2001,                                  // c.addiw with rd=0
        0x4002,                                  // c.lwsp with rd=0
        0x6002,                                  // c.ldsp with rd=0
        0x8002,                                  // c.jr with rs1=0
        0x9c00 | (0b11 << 5) | (0b110 << 2) | 1, // Zcb unary space, reserved selector
        0x8c40,      // quadrant 0, funct3=100, funct3'=011 with bit 6 set (reserved)
        0xfe00_0033, // OP with funct7=0x7f
        0x1050_00f3, // wfi with rd != 0
    ];
    for &raw in reserved {
        assert_eq!(decode(raw).op, Op::Illegal, "{raw:#x} should be illegal, got {}", decode(raw));
    }
    // lr.w with rs2 != 0 is reserved.
    assert_eq!(decode(0x1005_a52f | (1 << 20)).op, Op::Illegal);
    // slli with shamt[6] set (bit 26) is reserved on RV64.
    assert_eq!(decode(0x0405_9513).op, Op::Illegal);
}

#[test]
fn compressed_length() {
    assert_eq!(crate::insn_len(0x4501), 2);
    assert_eq!(crate::insn_len(0x0513), 4);
}
