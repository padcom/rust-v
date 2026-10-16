//! A RISC-V hart: architectural state, CSRs, traps, MMU and the reference interpreter.

mod block;
pub mod bus;
pub mod code;
pub mod csr;
pub mod exec;
mod fpu;
pub mod hart;
pub mod jit;
pub mod mem;
pub mod mmu;
pub mod trap;
mod vector;

pub use bus::{Bus, Device};
pub use exec::Exit;
pub use hart::{Hart, HartConfig, HartShared, HostClock, Priv, TimeSource};
pub use vector::MAX_VLENB;

#[cfg(test)]
mod tests;
