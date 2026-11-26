//! A DXIL module and its LLVM 3.7 bitcode, written with unabbreviated
//! records only (the format DXIL fixes; codes from LLVM's `LLVMBitCodes.h`).
//!
//! Values are symbolic while the module is built ([`V`]) and numbered when
//! it is written: functions, then constants (all at module level), then the
//! instructions of `main` in block order.

pub mod census;
pub mod opt;

mod builder;
mod encode;
mod types;
mod writer;
#[cfg(test)]
mod tests;

pub(crate) use types::*;
pub(in crate::emit::bitcode) use encode::successors;
