//! XMA2, the console's hardware audio codec: mono or stereo WMA Pro frames of
//! 512 samples per channel, packed into 2048-byte packets.
//!
//! A packet starts with a 32-bit header (MSB first): the number of frames
//! that begin in it (6 bits), the number of bits that finish the previous
//! frame before the first new one (15), metadata (3) and how many packets of
//! other streams follow before this stream's next packet (8). Every frame
//! starts with its length in bits (15) and ends with a "more frames" bit.
//!
//! The frame decoder follows FFmpeg's `wmaprodec.c` (LGPL-2.1-or-later, used
//! under GPL-3.0-or-later) restricted to what XMA needs: at most two
//! channels, four subframe sizes (512 down to 128), no LFE channel.


mod bits;
mod decoder;
mod imdct;
mod packet;
mod tables;
#[cfg(test)]
mod tests;

pub use decoder::Decoder;
pub use packet::{Frame, Packet, packet, read_frame};

pub const PACKET_BYTES: usize = 2048;
pub(super) const PACKET_BITS: usize = PACKET_BYTES * 8;
pub(super) const HEADER_BITS: usize = 32;
/// Samples per channel in one frame.
pub const FRAME_SAMPLES: usize = 512;
/// Samples per channel in one subframe, the unit of skips and loop ends.
pub const SUBFRAME_SAMPLES: usize = 128;
pub(super) const LENGTH_BITS: u32 = 15;
/// 512 >> 2: the smallest subframe.
pub(super) const MIN_SUBFRAME: usize = SUBFRAME_SAMPLES;
pub(super) const BLOCK_SIZES: usize = 3;
pub(super) const MAX_BANDS: usize = 29;
pub(super) const MAX_SUBFRAMES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "XMA: {}", self.0)
    }
}

impl std::error::Error for Error {}
