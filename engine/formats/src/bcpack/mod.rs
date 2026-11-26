//! BCPack: lossless packing of BC1-BC7 block streams, as the console's
//! DirectStorage hardware unpacks them. Decode only; BC2 is left out (no
//! title data uses it).
//!
//! A packed stream has no header. It is 12 or 16 interleaved rANS *lanes*;
//! each lane is a run of fragments (header byte, length, payload) coded with
//! one of 128 static models, and a fixed schedule decides which lane takes
//! the next input byte. The lanes' symbols form one record per block: two
//! back references (to one of the last 256 blocks, or to a per-format
//! dictionary that stands in for blocks -1 to -255) and the residuals of the
//! endpoints and indices against them. The models, the index rank tables,
//! the BC7 control codes and the dictionaries are constants of the format,
//! kept in `bcpack-tables.bin` beside this file ([`Tables::builtin`]).


mod bc6;
mod bc7;
mod plain;
mod rans;
mod tables;
#[cfg(test)]
mod tests;

pub use tables::{TABLES_SIZE, Tables};
use plain::{bc1_pack, bc3_pack, bc4_pack, bc5_pack};
use rans::lanes;

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BCPack: {}", self.0)
    }
}

impl std::error::Error for Error {}

pub(super) const MODELS: usize = 128;
pub(super) const BUCKETS: usize = 9;
pub(super) const HISTORY: usize = 256;
pub(super) const DICTIONARY: usize = 255;
/// rANS: decode while the state is above this, read while below.
pub(super) const LOW: u32 = 1 << 24;
/// Input bytes per schedule round.
pub(super) const ROUND_BYTES: usize = 8;


/// Unpacks `src` (BC number `bc`, 1 and 3-7) into `dst`, whose length gives
/// the block count. The stream carries no size: bytes after the last one the
/// lanes need are ignored, a stream that runs out is an error.
pub fn decode(bc: u8, src: &[u8], dst: &mut [u8], tables: &Tables) -> Result<(), Error> {
    let block = match bc {
        1 | 4 => 8,
        3 | 5 | 6 | 7 => 16,
        _ => return Err(Error("unsupported BC format")),
    };
    if dst.is_empty() || !dst.len().is_multiple_of(block) {
        return Err(Error("destination is not a whole number of blocks"));
    }
    let blocks = dst.len() / block;
    let (widths, per): (&[u8], usize) = match bc {
        1 => (&[7, 7, 4, 5, 4, 4, 5, 4, 7, 7, 7, 7], 1),
        3 => (&[3, 3, 7, 5, 5, 5, 5, 4, 5, 4, 7, 7], 2),
        4 => (&[7, 7, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5], 1),
        5 => (&[3, 3, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5], 2),
        6 => (&[3, 3, 7, 7, 7, 4, 4, 4, 5, 5, 5, 7, 7, 7, 7, 4], 2),
        _ => (&[3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4], 2),
    };
    let stride = widths.len() * per;
    let mut records = vec![0u8; blocks * stride];
    let mut next = src.iter().copied();
    lanes(tables, widths, per, blocks, |_| next.next(), &mut records)?;
    let records = records.chunks_exact(stride);
    let out = dst.chunks_exact_mut(block);
    let t = tables;
    match bc {
        1 => plain::plain(records, out, &t.bc1, &plain::BC1, |s| plain::bc1_residual(t, s), bc1_pack),
        3 => plain::plain(records, out, &t.bc3, &plain::BC3, |s| plain::bc3_residual(t, s), bc3_pack),
        4 => plain::plain(records, out, &t.bc4, &plain::BC4, |s| plain::bc4_residual(t, s), bc4_pack),
        5 => plain::plain(records, out, &t.bc5, &plain::BC5, |s| plain::bc5_residual(t, s), bc5_pack),
        6 => bc6::bc6(records, out, &t.bc6),
        _ => bc7::bc7(records, out, t),
    }
}
