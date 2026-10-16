//! Minimal ELF64 little-endian reader: just enough to load RISC-V images and look up symbols.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Truncated,
    NotElf,
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "ELF file is truncated"),
            Error::NotElf => write!(f, "not an ELF file"),
            Error::Unsupported(what) => write!(f, "unsupported ELF file: {what}"),
        }
    }
}

impl std::error::Error for Error {}

const EM_RISCV: u16 = 243;
const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;

/// A loadable segment.
#[derive(Debug, Clone, Copy)]
pub struct Segment<'a> {
    /// Physical load address.
    pub paddr: u64,
    /// Bytes present in the file; the remaining `mem_size - data.len()` bytes are zero.
    pub data: &'a [u8],
    pub mem_size: u64,
}

pub struct Elf<'a> {
    bytes: &'a [u8],
    pub entry: u64,
    phoff: usize,
    phnum: usize,
    phentsize: usize,
    shoff: usize,
    shnum: usize,
    shentsize: usize,
}

fn slice(bytes: &[u8], off: usize, len: usize) -> Result<&[u8], Error> {
    bytes.get(off..off.checked_add(len).ok_or(Error::Truncated)?).ok_or(Error::Truncated)
}

fn u16_at(b: &[u8], off: usize) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(slice(b, off, 2)?.try_into().unwrap()))
}

fn u32_at(b: &[u8], off: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(slice(b, off, 4)?.try_into().unwrap()))
}

fn u64_at(b: &[u8], off: usize) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(slice(b, off, 8)?.try_into().unwrap()))
}

impl<'a> Elf<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if slice(bytes, 0, 4)? != b"\x7fELF" {
            return Err(Error::NotElf);
        }
        if bytes[4] != 2 {
            return Err(Error::Unsupported("not ELF64"));
        }
        if bytes[5] != 1 {
            return Err(Error::Unsupported("not little-endian"));
        }
        if u16_at(bytes, 18)? != EM_RISCV {
            return Err(Error::Unsupported("not a RISC-V binary"));
        }
        Ok(Elf {
            bytes,
            entry: u64_at(bytes, 24)?,
            phoff: u64_at(bytes, 32)? as usize,
            shoff: u64_at(bytes, 40)? as usize,
            phentsize: u16_at(bytes, 54)? as usize,
            phnum: u16_at(bytes, 56)? as usize,
            shentsize: u16_at(bytes, 58)? as usize,
            shnum: u16_at(bytes, 60)? as usize,
        })
    }

    /// The `PT_LOAD` segments.
    pub fn segments(&self) -> Result<Vec<Segment<'a>>, Error> {
        let mut out = Vec::new();
        for i in 0..self.phnum {
            let ph = self.phoff + i * self.phentsize;
            if u32_at(self.bytes, ph)? != PT_LOAD {
                continue;
            }
            let offset = u64_at(self.bytes, ph + 8)? as usize;
            let paddr = u64_at(self.bytes, ph + 24)?;
            let file_size = u64_at(self.bytes, ph + 32)? as usize;
            let mem_size = u64_at(self.bytes, ph + 40)?;
            out.push(Segment { paddr, data: slice(self.bytes, offset, file_size)?, mem_size });
        }
        Ok(out)
    }

    /// Looks up the value of a symbol in `.symtab`.
    pub fn symbol(&self, name: &str) -> Result<Option<u64>, Error> {
        for i in 0..self.shnum {
            let sh = self.shoff + i * self.shentsize;
            if u32_at(self.bytes, sh + 4)? != SHT_SYMTAB {
                continue;
            }
            let sym_off = u64_at(self.bytes, sh + 24)? as usize;
            let sym_size = u64_at(self.bytes, sh + 32)? as usize;
            let entsize = (u64_at(self.bytes, sh + 56)? as usize).max(24);
            // sh_link points at the associated string table.
            let strtab = self.shoff + u32_at(self.bytes, sh + 40)? as usize * self.shentsize;
            let str_off = u64_at(self.bytes, strtab + 24)? as usize;
            let str_size = u64_at(self.bytes, strtab + 32)? as usize;
            let strings = slice(self.bytes, str_off, str_size)?;
            for s in (0..sym_size / entsize).map(|n| sym_off + n * entsize) {
                let name_off = u32_at(self.bytes, s)? as usize;
                let sym_name = strings.get(name_off..).ok_or(Error::Truncated)?;
                let end = sym_name.iter().position(|&b| b == 0).ok_or(Error::Truncated)?;
                if &sym_name[..end] == name.as_bytes() {
                    return Ok(Some(u64_at(self.bytes, s + 8)?));
                }
            }
        }
        Ok(None)
    }
}
