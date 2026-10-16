//! CSR numbers and field definitions.

#![allow(dead_code)]

// Unprivileged floating point.
pub const FFLAGS: u16 = 0x001;
pub const FRM: u16 = 0x002;
pub const FCSR: u16 = 0x003;

// Unprivileged vector.
pub const VSTART: u16 = 0x008;
pub const VXSAT: u16 = 0x009;
pub const VXRM: u16 = 0x00a;
pub const VCSR: u16 = 0x00f;
pub const VL: u16 = 0xc20;
pub const VTYPE: u16 = 0xc21;
pub const VLENB: u16 = 0xc22;

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
pub const STIMECMP: u16 = 0x14d;
pub const SSTATEEN0: u16 = 0x10c;
pub const SSTATEEN3: u16 = 0x10f;
pub const SCOUNTOVF: u16 = 0xda0;

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
pub const MSTATEEN0: u16 = 0x30c;
pub const MSTATEEN1: u16 = 0x30d;
pub const MSTATEEN3: u16 = 0x30f;
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

    /// Bits of `mstatus` that M-mode software can write.
    pub const M_WRITABLE: u64 =
        SIE | MIE | SPIE | MPIE | SPP | VS | MPP | FS | MPRV | SUM | MXR | TVM | TW | TSR;
    /// Bits of `mstatus` visible through `sstatus`.
    pub const S_VISIBLE: u64 =
        SIE | SPIE | UBE | SPP | VS | FS | XS | SUM | MXR | (UXL_SXL_64 & (3 << 32)) | SD;
    /// Bits of `mstatus` writable through `sstatus`.
    pub const S_WRITABLE: u64 = SIE | SPIE | SPP | VS | FS | SUM | MXR;
}

/// `menvcfg` / `senvcfg` fields.
pub mod envcfg {
    pub const FIOM: u64 = 1 << 0;
    /// Low bit of the 2-bit CBIE field: set for both "flush" (01) and "invalidate" (11).
    pub const CBIE_FLUSH: u64 = 1 << 4;
    pub const CBIE: u64 = 3 << 4;
    pub const CBCFE: u64 = 1 << 6;
    pub const CBZE: u64 = 1 << 7;
    /// Pointer masking mode for the next lower privilege (Smnpm/Ssnpm).
    pub const PMM: u64 = 3 << 32;
    pub const PMM_SHIFT: u32 = 32;
    pub const PBMTE: u64 = 1 << 62;
    pub const STCE: u64 = 1 << 63;
}

/// `mstateenN` fields.
pub mod stateen {
    /// Access to `sstateenN` from S-mode.
    pub const SE0: u64 = 1 << 63;
    /// Access to `senvcfg` from S-mode (`mstateen0` only).
    pub const ENVCFG: u64 = 1 << 62;
}

/// `mcounteren` bit for the `time` CSR (also gates `stimecmp`).
pub const COUNTEREN_TM: u64 = 1 << 1;

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
