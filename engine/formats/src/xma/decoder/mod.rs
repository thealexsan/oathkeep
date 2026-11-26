mod subframe;

use super::bits::{Bits, log2};
use super::imdct::to_i16;
use super::tables::*;
use super::{BLOCK_SIZES, Error, FRAME_SAMPLES, LENGTH_BITS, MAX_BANDS, MAX_SUBFRAMES, MIN_SUBFRAME};

// The frame decoder.

#[derive(Clone)]
pub(super) struct Channel {
    pub(super) prev_block_len: usize,
    pub(super) transmit_coefs: bool,
    pub(super) num_subframes: usize,
    pub(super) subframe_len: [usize; MAX_SUBFRAMES],
    pub(super) cur_subframe: usize,
    pub(super) decoded_samples: usize,
    pub(super) quant_step: i32,
    pub(super) reuse_sf: bool,
    pub(super) scale_factor_step: i32,
    pub(super) max_scale_factor: i32,
    pub(super) saved_scale_factors: [[i32; MAX_BANDS]; 2],
    pub(super) scale_factor_idx: usize,
    /// Which of the two saved sets is in use for this subframe.
    pub(super) scale_factors: usize,
    pub(super) table_idx: usize,
    pub(super) num_vec_coeffs: usize,
    pub(super) coeffs: [f32; FRAME_SAMPLES],
    /// The frame's output, then half a frame of overlap for the next one.
    pub(super) out: [f32; FRAME_SAMPLES + FRAME_SAMPLES / 2],
}

impl Channel {
    fn new() -> Self {
        Channel {
            prev_block_len: FRAME_SAMPLES,
            transmit_coefs: false,
            num_subframes: 0,
            subframe_len: [0; MAX_SUBFRAMES],
            cur_subframe: 0,
            decoded_samples: 0,
            quant_step: 0,
            reuse_sf: false,
            scale_factor_step: 1,
            max_scale_factor: 0,
            saved_scale_factors: [[0; MAX_BANDS]; 2],
            scale_factor_idx: 0,
            scale_factors: 0,
            table_idx: 0,
            num_vec_coeffs: 0,
            coeffs: [0.0; FRAME_SAMPLES],
            out: [0.0; FRAME_SAMPLES + FRAME_SAMPLES / 2],
        }
    }
}

/// Decodes the frames of one mono or stereo stream.
#[derive(Clone)]
pub struct Decoder {
    pub(super) channels: usize,
    pub(super) num_sfb: [usize; BLOCK_SIZES],
    pub(super) sfb_offsets: [[usize; MAX_BANDS + 1]; BLOCK_SIZES],
    pub(super) sf_offsets: [[[usize; MAX_BANDS]; BLOCK_SIZES]; BLOCK_SIZES],
    pub(super) ch: [Channel; 2],
    // Per subframe.
    pub(super) subframe_len: usize,
    pub(super) channels_for_cur_subframe: usize,
    pub(super) channel_indexes: [usize; 2],
    pub(super) num_bands: usize,
    pub(super) table_idx: usize,
    pub(super) esc_len: u32,
    pub(super) transmit_num_vec_coeffs: bool,
    pub(super) transform: bool,
    pub(super) transform_band: [bool; MAX_BANDS],
    pub(super) parsed_all_subframes: bool,
}

/// What the decoder needs from the stream format: the channel count and
/// the sample rate, which sets the scale factor band layout.
impl Decoder {
    pub fn new(channels: usize, sample_rate: u32) -> Decoder {
        let channels = channels.clamp(1, 2);
        let rate = match sample_rate {
            r if r > 44_100 => 48_000,
            r if r > 32_000 => 44_100,
            r if r > 24_000 => 32_000,
            _ => 24_000,
        };
        let mut sfb_offsets = [[0usize; MAX_BANDS + 1]; BLOCK_SIZES];
        let mut num_sfb = [0usize; BLOCK_SIZES];
        for i in 0..BLOCK_SIZES {
            let subframe_len = FRAME_SAMPLES >> i;
            let offsets = &mut sfb_offsets[i];
            let mut band = 1;
            for &freq in CRITICAL_FREQ.iter().take(MAX_BANDS - 1) {
                if offsets[band - 1] >= subframe_len {
                    break;
                }
                let offset = ((subframe_len * 2 * freq as usize) / rate + 2) & !3;
                if offset > offsets[band - 1] {
                    offsets[band] = offset;
                    band += 1;
                }
                if offset >= subframe_len {
                    break;
                }
            }
            offsets[band - 1] = subframe_len;
            num_sfb[i] = band - 1;
        }
        let mut sf_offsets = [[[0usize; MAX_BANDS]; BLOCK_SIZES]; BLOCK_SIZES];
        for i in 0..BLOCK_SIZES {
            for b in 0..num_sfb[i] {
                let offset = ((sfb_offsets[i][b] + sfb_offsets[i][b + 1] - 1) << i) >> 1;
                for x in 0..BLOCK_SIZES {
                    let mut v = 0;
                    while v + 1 < MAX_BANDS && (sfb_offsets[x][v + 1] << x) < offset {
                        v += 1;
                    }
                    sf_offsets[i][x][b] = v;
                }
            }
        }
        Decoder {
            channels,
            num_sfb,
            sfb_offsets,
            sf_offsets,
            ch: [Channel::new(), Channel::new()],
            subframe_len: FRAME_SAMPLES,
            channels_for_cur_subframe: 0,
            channel_indexes: [0, 1],
            num_bands: 0,
            table_idx: 0,
            esc_len: 0,
            transmit_num_vec_coeffs: false,
            transform: false,
            transform_band: [false; MAX_BANDS],
            parsed_all_subframes: false,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Forgets the overlap with earlier frames (a new stream or a seek).
    pub fn reset(&mut self) {
        self.ch = [Channel::new(), Channel::new()];
    }

    /// Decodes one frame (`frame.data`, `frame.bits`) and appends
    /// `FRAME_SAMPLES` interleaved samples per channel to `out`.
    pub fn decode(&mut self, data: &[u8], bits: usize, out: &mut Vec<i16>) -> Result<(), Error> {
        let mut b = Bits {
            data,
            pos: 0,
            // Past the data there are only zeros to read. A count beyond it
            // would never let a stereo tile header overread, and its loop
            // takes no subframe on zero bits, so it would spin for good.
            end: bits.min(data.len().saturating_mul(8)),
        };
        let len = b.read(LENGTH_BITS) as usize;
        let result = self.frame(&mut b, len);
        if result.is_err() {
            // Whatever the frame held is lost; keep the stream's timing.
            for c in 0..self.channels {
                self.ch[c].out = [0.0; FRAME_SAMPLES + FRAME_SAMPLES / 2];
            }
        }
        for i in 0..FRAME_SAMPLES {
            for c in 0..self.channels {
                out.push(to_i16(self.ch[c].out[i]));
            }
        }
        for c in 0..self.channels {
            self.ch[c].out.copy_within(FRAME_SAMPLES.., 0);
        }
        result
    }

    /// The overlap left after the last frame, then silence: half a frame of
    /// samples per channel.
    pub fn flush(&mut self, out: &mut Vec<i16>) {
        for i in 0..FRAME_SAMPLES / 2 {
            for c in 0..self.channels {
                out.push(to_i16(self.ch[c].out[i]));
            }
        }
        self.reset();
    }

    fn frame(&mut self, b: &mut Bits, len: usize) -> Result<(), Error> {
        self.tile_header(b)?;
        // Post-processing transform.
        if self.channels > 1 && b.bit() && b.bit() {
            b.read((self.channels * self.channels * 4) as u32);
        }
        // Dynamic range compression gain, unused.
        b.read(8);
        if b.bit() {
            // Trim at start and end, which only WMA Pro files apply.
            if b.bit() {
                b.read(log2(FRAME_SAMPLES * 2));
            }
            if b.bit() {
                b.read(log2(FRAME_SAMPLES * 2));
            }
        }
        self.parsed_all_subframes = false;
        for c in 0..self.channels {
            let ch = &mut self.ch[c];
            ch.decoded_samples = 0;
            ch.cur_subframe = 0;
            ch.reuse_sf = false;
        }
        while !self.parsed_all_subframes {
            self.subframe(b)?;
        }
        // FFmpeg flags `len != consumed + 2` but keeps the samples; so do we.
        if b.overread() || b.consumed() > len {
            return Err(Error("frame overread"));
        }
        Ok(())
    }

    fn subframe_length(&self, b: &mut Bits, offset: usize) -> Result<usize, Error> {
        if offset == FRAME_SAMPLES - MIN_SUBFRAME {
            return Ok(MIN_SUBFRAME);
        }
        // Four subframe sizes: a 1-bit "not maximal", then 1 bit of shift.
        let shift = if b.bit() { 1 + b.read(1) } else { 0 };
        let len = FRAME_SAMPLES >> shift;
        if len < MIN_SUBFRAME {
            return Err(Error("subframe length"));
        }
        Ok(len)
    }

    fn tile_header(&mut self, b: &mut Bits) -> Result<(), Error> {
        let mut num_samples = [0usize; 2];
        let mut contains = [false; 2];
        let mut channels_for_cur_subframe = self.channels;
        let mut min_channel_len = 0;
        for c in 0..self.channels {
            self.ch[c].num_subframes = 0;
        }
        let fixed = b.bit();
        loop {
            for c in 0..self.channels {
                contains[c] = num_samples[c] == min_channel_len
                    && (fixed
                        || channels_for_cur_subframe == 1
                        || min_channel_len == FRAME_SAMPLES - MIN_SUBFRAME
                        || b.bit());
            }
            let subframe_len = self.subframe_length(b, min_channel_len)?;
            min_channel_len += subframe_len;
            for c in 0..self.channels {
                let ch = &mut self.ch[c];
                if contains[c] {
                    if ch.num_subframes >= MAX_SUBFRAMES {
                        return Err(Error("too many subframes"));
                    }
                    ch.subframe_len[ch.num_subframes] = subframe_len;
                    num_samples[c] += subframe_len;
                    ch.num_subframes += 1;
                    if num_samples[c] > FRAME_SAMPLES {
                        return Err(Error("channel longer than a frame"));
                    }
                } else if num_samples[c] <= min_channel_len {
                    if num_samples[c] < min_channel_len {
                        channels_for_cur_subframe = 0;
                        min_channel_len = num_samples[c];
                    }
                    channels_for_cur_subframe += 1;
                }
            }
            if min_channel_len >= FRAME_SAMPLES || b.overread() {
                break;
            }
        }
        if b.overread() {
            return Err(Error("tile header"));
        }
        Ok(())
    }

    fn channel_transform(&mut self, b: &mut Bits) -> Result<(), Error> {
        self.transform = false;
        if self.channels == 1 {
            return Ok(());
        }
        if b.bit() {
            return Err(Error("channel transform bit"));
        }
        // One group holding every channel of the subframe.
        if self.channels_for_cur_subframe == 2 {
            if b.bit() {
                if b.bit() {
                    return Err(Error("channel transform type"));
                }
            } else {
                self.transform = true;
            }
        }
        if self.transform {
            if !b.bit() {
                for i in 0..self.num_bands {
                    self.transform_band[i] = b.bit();
                }
            } else {
                self.transform_band = [true; MAX_BANDS];
            }
        }
        Ok(())
    }
}
