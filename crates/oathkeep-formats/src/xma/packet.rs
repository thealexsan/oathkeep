use super::{HEADER_BITS, LENGTH_BITS, PACKET_BITS, PACKET_BYTES};

// Packets.

/// A packet's header fields.
#[derive(Debug, Clone, Copy)]
pub struct Packet {
    pub frames: u32,
    pub first_frame_bit: u32,
    pub skip: u32,
}

pub fn packet(data: &[u8], index: usize) -> Option<Packet> {
    let start = index.checked_mul(PACKET_BYTES)?;
    let b = data.get(start..)?.get(..4)?;
    let h = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Some(Packet {
        frames: h >> 26,
        first_frame_bit: (h >> 11) & 0x7fff,
        skip: h & 0xff,
    })
}

/// One frame's bits gathered out of its packets.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The frame from its length field on, MSB first.
    pub data: Vec<u8>,
    pub bits: usize,
    /// Bit offset in the buffer where it starts, and where the next frame
    /// starts (`None` when the buffer has no further frame).
    pub start: usize,
    pub next: Option<usize>,
}

/// Bit positions in a buffer of packets, skipping each packet's header and
/// the packets of other streams.
pub(super) struct Cursor<'a> {
    pub(super) data: &'a [u8],
    pub(super) pos: usize,
}

impl Cursor<'_> {
    fn bit(&mut self) -> Option<u32> {
        if self.pos.is_multiple_of(PACKET_BITS) {
            // Leaving a packet: jump over the other streams' packets.
            let previous = self.pos / PACKET_BITS;
            if previous > 0 {
                let skip = packet(self.data, previous - 1)?.skip as usize;
                self.pos = (previous + skip) * PACKET_BITS;
            }
            self.pos += HEADER_BITS;
        }
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    pub(super) fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0, |v, _| Some(v << 1 | self.bit()?))
    }

    /// A payload byte, MSB first. At a packet edge the individual bits
    /// may be separated by a header and other streams' packets.
    pub(super) fn byte(&mut self) -> Option<u8> {
        let in_packet = self.pos % PACKET_BITS;
        if in_packet == 0 || in_packet > PACKET_BITS - 8 {
            return self.bits(8).map(|v| v as u8);
        }
        let at = self.pos / 8;
        let shift = self.pos % 8;
        let first = *self.data.get(at)?;
        let byte = if shift == 0 {
            first
        } else {
            (first << shift) | (self.data.get(at + 1)? >> (8 - shift))
        };
        self.pos += 8;
        Some(byte)
    }
}

/// Reads the frame starting at bit `start` (a frame start inside a packet's
/// payload) of `data`, a buffer of whole packets of one stream.
pub fn read_frame(data: &[u8], start: usize) -> Option<Frame> {
    let mut cursor = Cursor { data, pos: start };
    let bits = cursor.bits(LENGTH_BITS)? as usize;
    if bits <= LENGTH_BITS as usize || bits == 0x7fff {
        return None;
    }
    let mut out = Vec::with_capacity(bits.div_ceil(8));
    let mut cursor = Cursor { data, pos: start };
    for _ in 0..bits / 8 {
        out.push(cursor.byte()?);
    }
    let tail = (bits % 8) as u32;
    if tail != 0 {
        out.push((cursor.bits(tail)? << (8 - tail)) as u8);
    }
    let end = cursor.pos;
    let next = next_frame(data, start, end);
    Some(Frame {
        data: out,
        bits,
        start,
        next,
    })
}

/// Where the frame after the one at `start`..`end` begins: right after it,
/// unless it was the last frame to begin in its packet and the rest of that
/// packet is padding.
pub(super) fn next_frame(data: &[u8], start: usize, end: usize) -> Option<usize> {
    let index = start / PACKET_BITS;
    let header = packet(data, index)?;
    let mut at = index * PACKET_BITS + HEADER_BITS + header.first_frame_bit as usize;
    let mut number = 0;
    while at < start {
        let mut cursor = Cursor { data, pos: at };
        let bits = cursor.bits(LENGTH_BITS)? as usize;
        // The same rule as `read_frame`: a length that does not cover its own
        // field is not a frame. A zero length would never move `at`, so a
        // damaged packet would loop here for good.
        if bits <= LENGTH_BITS as usize {
            return None;
        }
        at += bits;
        number += 1;
    }
    let last = number + 1 >= header.frames;
    let mut end = end;
    // A frame that ends in a packet where no frame begins (the tail of a frame
    // that crossed packets, then padding) is the last one before the next
    // packet that has frames.
    let ends_in_packet_without_frames =
        end / PACKET_BITS != index && !end.is_multiple_of(PACKET_BITS) && packet(data, end / PACKET_BITS)?.frames == 0;
    if end.is_multiple_of(PACKET_BITS) || (last && end / PACKET_BITS == index) || ends_in_packet_without_frames {
        // The next frame begins in the next packet of this stream that has one;
        // a packet with none (a frame spans all of it) holds no start.
        let mut next = (end - 1) / PACKET_BITS + 1 + packet(data, (end - 1) / PACKET_BITS)?.skip as usize;
        let mut header = packet(data, next)?;
        while header.frames == 0 {
            next += 1 + header.skip as usize;
            header = packet(data, next)?;
        }
        end = next * PACKET_BITS + HEADER_BITS + header.first_frame_bit as usize;
    }
    (end / 8 < data.len()).then_some(end)
}
