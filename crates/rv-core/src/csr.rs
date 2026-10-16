//! CSR numbers and field definitions.

#![allow(dead_code)]

// Unprivileged floating point.
pub const FFLAGS: u16 = 0x001;
pub const FRM: u16 = 0x002;
pub const FCSR: u16 = 0x003;

// Unprivileged counters.
pub const CYCLE: u16 = 0xc00;
pub const TIME: u16 = 0xc01;
pub const INSTRET: u16 = 0xc02;
pub const HPMCOUNTER3: u16 = 0xc03;
pub const HPMCOUNTER31: u16 = 0xc1f;

// Supervisor.
pub const SSTATUS: u16 = 0x100;
pub const SIE: u16 = 0x104;
pub const STVEC: u16 = 0x105;
pub const SCOUNTEREN: u16 = 0x106;
pub const SENVCFG: u16 = 0x10a;
pub const SSCRATCH: u16 = 0x140;
pub const SEPC: u16 = 0x141;
pub const SCAUSE: u16 = 0x142;
pub const STVAL: u16 = 0x143;
pub const SIP: u16 = 0x144;
pub const SATP: u16 = 0x180;

// Machine.
pub const MVENDORID: u16 = 0xf11;
pub const MARCHID: u16 = 0xf12;
pub const MIMPID: u16 = 0xf13;
pub const MHARTID: u16 = 0xf14;
pub const MCONFIGPTR: u16 = 0xf15;
pub const MSTATUS: u16 = 0x300;
pub const MISA: u16 = 0x301;
pub const MEDELEG: u16 = 0x302;
pub const MIDELEG: u16 = 0x303;
pub const MIE: u16 = 0x304;
pub const MTVEC: u16 = 0x305;
pub const MCOUNTEREN: u16 = 0x306;
pub const MENVCFG: u16 = 0x30a;
pub const MCOUNTINHIBIT: u16 = 0x320;
pub const MHPMEVENT3: u16 = 0x323;
pub const MHPMEVENT31: u16 = 0x33f;
pub const MSCRATCH: u16 = 0x340;
pub const MEPC: u16 = 0x341;
pub const MCAUSE: u16 = 0x342;
pub const MTVAL: u16 = 0x343;
pub const MIP: u16 = 0x344;
pub const PMPCFG0: u16 = 0x3a0;
pub const PMPCFG15: u16 = 0x3af;
pub const PMPADDR0: u16 = 0x3b0;
pub const PMPADDR63: u16 = 0x3ef;
pub const MSECCFG: u16 = 0x747;
pub const TSELECT: u16 = 0x7a0;
pub const TDATA1: u16 = 0x7a1;
pub const TDATA2: u16 = 0x7a2;
pub const TDATA3: u16 = 0x7a3;
pub const TINFO: u16 = 0x7a4;
pub const MCYCLE: u16 = 0xb00;
pub const MINSTRET: u16 = 0xb02;
pub const MHPMCOUNTER3: u16 = 0xb03;
pub const MHPMCOUNTER31: u16 = 0xb1f;

/// `mstatus` / `sstatus` fields.
pub mod status {
    pub const SIE: u64 = 1 << 1;
    pub const MIE: u64 = 1 << 3;
    pub const SPIE: u64 = 1 << 5;
    pub const UBE: u64 = 1 << 6;
    pub const MPIE: u64 = 1 << 7;
    pub const SPP: u64 = 1 << 8;
    pub const VS: u64 = 3 << 9;
    pub const MPP: u64 = 3 << 11;
    pub const MPP_SHIFT: u32 = 11;
    pub const FS: u64 = 3 << 13;
    pub const XS: u64 = 3 << 15;
    pub const MPRV: u64 = 1 << 17;
    pub const SUM: u64 = 1 << 18;
    pub const MXR: u64 = 1 << 19;
    pub const TVM: u64 = 1 << 20;
    pub const TW: u64 = 1 << 21;
    pub const TSR: u64 = 1 << 22;
    /// UXL = SXL = 64-bit; read-only.
    pub const UXL_SXL_64: u64 = (2 << 32) | (2 << 34);
    pub const SD: u64 = 1 << 63;

    /// Bits of `mstatus` that M-mode software can write (VS is added when V exists).
    pub const M_WRITABLE: u64 = SIE | MIE | SPIE | MPIE | SPP | MPP | FS | MPRV | SUM | MXR | TVM | TW | TSR;
    /// Bits of `mstatus` visible through `sstatus`.
    pub const S_VISIBLE: u64 =
        SIE | SPIE | UBE | SPP | VS | FS | XS | SUM | MXR | (UXL_SXL_64 & (3 << 32)) | SD;
    /// Bits of `mstatus` writable through `sstatus`.
    pub const S_WRITABLE: u64 = SIE | SPIE | SPP | FS | SUM | MXR;
}

/// `satp` fields.
pub mod satp {
    pub const MODE_SHIFT: u32 = 60;
    pub const MODE_BARE: u64 = 0;
    pub const MODE_SV39: u64 = 8;
    pub const MODE_SV48: u64 = 9;
    pub const MODE_SV57: u64 = 10;
    pub const ASID_MASK: u64 = 0xffff << 44;
    pub const PPN_MASK: u64 = (1 << 44) - 1;
}

/// `misa` extension bits.
pub mod misa {
    pub const fn ext(letter: u8) -> u64 {
        1 << (letter - b'A')
    }
    pub const MXL_64: u64 = 2 << 62;
}
