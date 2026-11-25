mod tables;

use std::sync::OnceLock;

use tables::*;

pub const PACKET_BYTES: usize = 2048;
const PACKET_BITS: usize = PACKET_BYTES * 8;
const HEADER_BITS: usize = 32;
pub const FRAME_SAMPLES: usize = 512;
pub const SUBFRAME_SAMPLES: usize = 128;
const LENGTH_BITS: u32 = 15;
const MIN_SUBFRAME: usize = SUBFRAME_SAMPLES;
const BLOCK_SIZES: usize = 3;
const MAX_BANDS: usize = 29;
const MAX_SUBFRAMES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "XMA: {}", self.0)
    }
}

impl std::error::Error for Error {}

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

#[derive(Debug, Clone)]
pub struct Frame {
    pub data: Vec<u8>,
    pub bits: usize,
    pub start: usize,
    pub next: Option<usize>,
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn bit(&mut self) -> Option<u32> {
        if self.pos.is_multiple_of(PACKET_BITS) {
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

    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0, |v, _| Some(v << 1 | self.bit()?))
    }
}

pub fn read_frame(data: &[u8], start: usize) -> Option<Frame> {
    let mut cursor = Cursor { data, pos: start };
    let bits = cursor.bits(LENGTH_BITS)? as usize;
    if bits <= LENGTH_BITS as usize || bits == 0x7fff {
        return None;
    }
    let mut out = vec![0u8; bits.div_ceil(8)];
    let mut cursor = Cursor { data, pos: start };
    for i in 0..bits {
        out[i / 8] |= (cursor.bit()? as u8) << (7 - i % 8);
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

fn next_frame(data: &[u8], start: usize, end: usize) -> Option<usize> {
    let index = start / PACKET_BITS;
    let header = packet(data, index)?;
    let mut at = index * PACKET_BITS + HEADER_BITS + header.first_frame_bit as usize;
    let mut number = 0;
    while at < start {
        let mut cursor = Cursor { data, pos: at };
        let bits = cursor.bits(LENGTH_BITS)? as usize;
        if bits <= LENGTH_BITS as usize {
            return None;
        }
        at += bits;
        number += 1;
    }
    let last = number + 1 >= header.frames;
    let mut end = end;
    if end.is_multiple_of(PACKET_BITS) || (last && end / PACKET_BITS == index) {
        let next =
            (end - 1) / PACKET_BITS + 1 + packet(data, (end - 1) / PACKET_BITS)?.skip as usize;
        let header = packet(data, next)?;
        end = next * PACKET_BITS + HEADER_BITS + header.first_frame_bit as usize;
    }
    (end / 8 < data.len()).then_some(end)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl Bits<'_> {
    fn peek32(&self) -> u32 {
        let mut v = 0u64;
        let byte = self.pos / 8;
        for i in 0..5 {
            v = v << 8 | *self.data.get(byte + i).unwrap_or(&0) as u64;
        }
        (v << (24 + self.pos % 8) >> 32) as u32
    }

    fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek32() >> (32 - n);
        self.pos += n as usize;
        v
    }

    fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    fn signed(&mut self, n: u32) -> i32 {
        ((self.read(n) << (32 - n)) as i32) >> (32 - n)
    }

    fn consumed(&self) -> usize {
        self.pos
    }

    fn overread(&self) -> bool {
        self.pos > self.end
    }
}

struct Vlc {
    codes: Vec<(u32, u8, i32)>,
}

impl Vlc {
    fn new(entries: impl Iterator<Item = (i32, u8)>) -> Vlc {
        let mut code = 0u64;
        let mut codes = Vec::new();
        for (symbol, len) in entries {
            codes.push((code as u32, len, symbol));
            code += 1 << (32 - len);
        }
        Vlc { codes }
    }

    fn read(&self, bits: &mut Bits) -> i32 {
        let v = bits.peek32();
        let i = self.codes.partition_point(|c| c.0 <= v) - 1;
        let (_, len, symbol) = self.codes[i];
        bits.pos += len as usize;
        symbol
    }
}

struct Codes {
    scale: Vlc,
    scale_rl: Vlc,
    coef: [Vlc; 2],
    vec4: Vlc,
    vec2: Vlc,
    vec1: Vlc,
}

fn codes() -> &'static Codes {
    static CODES: OnceLock<Codes> = OnceLock::new();
    CODES.get_or_init(|| {
        let pairs = |t: &'static [(u16, u8)], offset: i32| {
            Vlc::new(t.iter().map(move |&(s, l)| (s as i32 + offset, l)))
        };
        Codes {
            scale: pairs(&SCALE, -60),
            scale_rl: pairs(&SCALE_RL, 0),
            coef: [
                Vlc::new(
                    COEF0_SYMS
                        .iter()
                        .zip(COEF0_LENS)
                        .map(|(&s, l)| (s as i32, l)),
                ),
                pairs(&COEF1, 0),
            ],
            vec4: Vlc::new(
                VEC4_SYMS
                    .iter()
                    .zip(VEC4_LENS)
                    .map(|(&s, l)| (s as i32 - 1, l)),
            ),
            vec2: pairs(&VEC2, -1),
            vec1: pairs(&VEC1, 0),
        }
    })
}

fn large_value(bits: &mut Bits) -> u32 {
    let mut n = 8;
    if bits.bit() {
        n += 8;
        if bits.bit() {
            n += 8;
            if bits.bit() {
                n += 7;
            }
        }
    }
    bits.read(n)
}

fn log2(v: usize) -> u32 {
    usize::BITS - 1 - v.max(1).leading_zeros()
}

fn imdct_half(input: &[f32], out: &mut [f32], scale: f32) {
    let n = input.len();
    let m = n / 2;
    let mut re = vec![0f64; m];
    let mut im = vec![0f64; m];
    for k in 0..m {
        let (a, b) = (input[2 * k] as f64, input[n - 1 - 2 * k] as f64);
        let (s, c) = (-std::f64::consts::PI * k as f64 / n as f64).sin_cos();
        (re[k], im[k]) = (a * c - b * s, a * s + b * c);
    }
    fft(&mut re, &mut im);
    for k in 0..m {
        let (s, c) = (-std::f64::consts::PI * (4 * k + 1) as f64 / (4 * n) as f64).sin_cos();
        let (wr, wi) = (re[k] * c - im[k] * s, re[k] * s + im[k] * c);
        out[n - 1 - 2 * k] = (wr as f32) * scale;
        out[2 * k] = -(wi as f32) * scale;
    }
}

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let (ws, wc) = (-2.0 * std::f64::consts::PI / len as f64).sin_cos();
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                (re[b], im[b]) = (re[a] - tr, im[a] - ti);
                (re[a], im[a]) = (re[a] + tr, im[a] + ti);
                (cr, ci) = (cr * wc - ci * ws, cr * ws + ci * wc);
            }
        }
        len <<= 1;
    }
}

fn sine_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i as f64 + 0.5) * std::f64::consts::PI / (2.0 * n as f64)).sin() as f32)
        .collect()
}

#[derive(Clone)]
struct Channel {
    prev_block_len: usize,
    transmit_coefs: bool,
    num_subframes: usize,
    subframe_len: [usize; MAX_SUBFRAMES],
    cur_subframe: usize,
    decoded_samples: usize,
    quant_step: i32,
    reuse_sf: bool,
    scale_factor_step: i32,
    max_scale_factor: i32,
    saved_scale_factors: [[i32; MAX_BANDS]; 2],
    scale_factor_idx: usize,
    scale_factors: usize,
    table_idx: usize,
    num_vec_coeffs: usize,
    coeffs: [f32; FRAME_SAMPLES],
    out: [f32; FRAME_SAMPLES + FRAME_SAMPLES / 2],
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

#[derive(Clone)]
pub struct Decoder {
    channels: usize,
    num_sfb: [usize; BLOCK_SIZES],
    sfb_offsets: [[usize; MAX_BANDS + 1]; BLOCK_SIZES],
    sf_offsets: [[[usize; MAX_BANDS]; BLOCK_SIZES]; BLOCK_SIZES],
    ch: [Channel; 2],
    subframe_len: usize,
    channels_for_cur_subframe: usize,
    channel_indexes: [usize; 2],
    num_bands: usize,
    table_idx: usize,
    esc_len: u32,
    transmit_num_vec_coeffs: bool,
    transform: bool,
    transform_band: [bool; MAX_BANDS],
    parsed_all_subframes: bool,
}

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

    pub fn reset(&mut self) {
        self.ch = [Channel::new(), Channel::new()];
    }

    pub fn decode(&mut self, data: &[u8], bits: usize, out: &mut Vec<i16>) -> Result<(), Error> {
        let mut b = Bits {
            data,
            pos: 0,
            end: bits,
        };
        let len = b.read(LENGTH_BITS) as usize;
        let result = self.frame(&mut b, len);
        if result.is_err() {
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
        if self.channels > 1 && b.bit() && b.bit() {
            b.read((self.channels * self.channels * 4) as u32);
        }
        b.read(8);
        if b.bit() {
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
        if b.overread() || b.consumed() > len {
            return Err(Error("frame overread"));
        }
        Ok(())
    }

    fn subframe_length(&self, b: &mut Bits, offset: usize) -> Result<usize, Error> {
        if offset == FRAME_SAMPLES - MIN_SUBFRAME {
            return Ok(MIN_SUBFRAME);
        }
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

    fn coefficients(&mut self, b: &mut Bits, c: usize) -> Result<(), Error> {
        let codes = codes();
        let table = b.bit() as usize;
        let (run, level): (&[u16], &[f32]) = if table == 1 {
            (&COEF1_RUN, &COEF1_LEVEL)
        } else {
            (&COEF0_RUN, &COEF0_LEVEL)
        };
        let subframe_len = self.subframe_len;
        let ch = &mut self.ch[c];
        let mut rl_mode = false;
        let mut cur = 0;
        let mut num_zeros = 0;
        while (self.transmit_num_vec_coeffs || !rl_mode) && cur + 3 < ch.num_vec_coeffs {
            let mut vals = [0u32; 4];
            let idx = codes.vec4.read(b);
            if idx < 0 {
                for i in (0..4).step_by(2) {
                    let idx = codes.vec2.read(b);
                    if idx < 0 {
                        for v in &mut vals[i..i + 2] {
                            let mut x = codes.vec1.read(b) as u32;
                            if x as usize == VEC1.len() - 1 {
                                x += large_value(b);
                            }
                            *v = x;
                        }
                    } else {
                        vals[i] = (idx >> 4) as u32;
                        vals[i + 1] = (idx & 0xf) as u32;
                    }
                }
            } else {
                vals = [
                    (idx >> 12) as u32,
                    ((idx >> 8) & 0xf) as u32,
                    ((idx >> 4) & 0xf) as u32,
                    (idx & 0xf) as u32,
                ];
            }
            for v in vals {
                if v != 0 {
                    let negative = !b.bit();
                    ch.coeffs[cur] = if negative { -(v as f32) } else { v as f32 };
                    num_zeros = 0;
                } else {
                    ch.coeffs[cur] = 0.0;
                    num_zeros += 1;
                    rl_mode |= num_zeros > subframe_len >> 8;
                }
                cur += 1;
            }
            if b.overread() {
                return Err(Error("vector coefficients"));
            }
        }
        if cur < subframe_len {
            ch.coeffs[cur..subframe_len].fill(0.0);
            let mask = subframe_len - 1;
            let mut offset = cur;
            while offset < subframe_len {
                let code = codes.coef[table].read(b);
                if code > 1 {
                    offset += run[code as usize] as usize;
                    let negative = !b.bit();
                    let v = level[code as usize];
                    ch.coeffs[offset & mask] = if negative { -v } else { v };
                } else if code == 1 {
                    break;
                } else {
                    let value = large_value(b) as i32;
                    if b.bit() {
                        if b.bit() {
                            if b.bit() {
                                break;
                            }
                            offset += b.read(self.esc_len) as usize + 4;
                        } else {
                            offset += b.read(2) as usize + 1;
                        }
                    }
                    let negative = !b.bit();
                    ch.coeffs[offset & mask] = if negative { -value } else { value } as f32;
                }
                if b.overread() {
                    return Err(Error("run level coefficients"));
                }
                offset += 1;
            }
        }
        Ok(())
    }

    fn scale_factors(&mut self, b: &mut Bits) -> Result<(), Error> {
        let codes = codes();
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes[i];
            let sf_offsets = self.sf_offsets[self.table_idx][self.ch[c].table_idx];
            let ch = &mut self.ch[c];
            let target = 1 - ch.scale_factor_idx;
            ch.scale_factors = target;
            if ch.reuse_sf {
                let from = ch.saved_scale_factors[ch.scale_factor_idx];
                for (sf, &offset) in ch.saved_scale_factors[target]
                    .iter_mut()
                    .zip(&sf_offsets)
                    .take(self.num_bands)
                {
                    *sf = from[offset];
                }
            }
            if ch.cur_subframe == 0 || b.bit() {
                if !ch.reuse_sf {
                    ch.scale_factor_step = b.read(2) as i32 + 1;
                    let mut val = 45 / ch.scale_factor_step;
                    for band in 0..self.num_bands {
                        val += codes.scale.read(b);
                        ch.saved_scale_factors[target][band] = val;
                    }
                } else {
                    let mut band = 0;
                    while band < self.num_bands {
                        let idx = codes.scale_rl.read(b);
                        let (skip, val, negative) = match idx {
                            0 => {
                                let code = b.read(14);
                                ((code & 0x3f) >> 1, (code >> 6) as i32, code & 1 == 0)
                            }
                            1 => break,
                            _ => (
                                SCALE_RL_RUN[idx as usize] as u32,
                                SCALE_RL_LEVEL[idx as usize] as i32,
                                !b.bit(),
                            ),
                        };
                        band += skip as usize;
                        if band >= self.num_bands {
                            return Err(Error("scale factor run"));
                        }
                        ch.saved_scale_factors[target][band] += if negative { -val } else { val };
                        band += 1;
                    }
                }
                ch.scale_factor_idx = target;
                ch.table_idx = self.table_idx;
                ch.reuse_sf = true;
            }
            let sf = &ch.saved_scale_factors[ch.scale_factors][..self.num_bands];
            ch.max_scale_factor = sf.iter().copied().max().unwrap_or(0);
        }
        Ok(())
    }

    fn subframe(&mut self, b: &mut Bits) -> Result<(), Error> {
        let mut offset = FRAME_SAMPLES;
        let mut subframe_len = FRAME_SAMPLES;
        let mut total_samples = FRAME_SAMPLES * self.channels;
        for c in 0..self.channels {
            let ch = &self.ch[c];
            if offset > ch.decoded_samples {
                offset = ch.decoded_samples;
                subframe_len = ch.subframe_len[ch.cur_subframe.min(MAX_SUBFRAMES - 1)];
            }
        }
        self.channels_for_cur_subframe = 0;
        for c in 0..self.channels {
            let ch = &mut self.ch[c];
            total_samples -= ch.decoded_samples;
            if offset == ch.decoded_samples
                && ch.cur_subframe < ch.num_subframes
                && subframe_len == ch.subframe_len[ch.cur_subframe]
            {
                total_samples -= subframe_len;
                ch.decoded_samples += subframe_len;
                self.channel_indexes[self.channels_for_cur_subframe] = c;
                self.channels_for_cur_subframe += 1;
            }
        }
        if self.channels_for_cur_subframe == 0 {
            return Err(Error("no channel in subframe"));
        }
        if total_samples == 0 {
            self.parsed_all_subframes = true;
        }
        self.table_idx = log2(FRAME_SAMPLES / subframe_len) as usize;
        self.num_bands = self.num_sfb[self.table_idx];
        self.subframe_len = subframe_len;
        self.esc_len = log2(subframe_len - 1) + 1;

        if b.bit() {
            let mut fill = b.read(2) as usize;
            if fill == 0 {
                let len = b.read(4);
                fill = b.read(len) as usize + 1;
            }
            b.pos += fill;
        }
        if b.bit() {
            return Err(Error("reserved bit"));
        }
        self.channel_transform(b)?;

        let mut transmit_coeffs = false;
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes[i];
            self.ch[c].transmit_coefs = b.bit();
            transmit_coeffs |= self.ch[c].transmit_coefs;
        }
        if transmit_coeffs {
            self.transmit_num_vec_coeffs = b.bit();
            if self.transmit_num_vec_coeffs {
                let num_bits = log2(subframe_len.div_ceil(4)) + 1;
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes[i];
                    let n = (b.read(num_bits) << 2) as usize;
                    if n > subframe_len {
                        return Err(Error("vector coefficient count"));
                    }
                    self.ch[c].num_vec_coeffs = n;
                }
            } else {
                for i in 0..self.channels_for_cur_subframe {
                    self.ch[self.channel_indexes[i]].num_vec_coeffs = subframe_len;
                }
            }
            let mut quant_step = 90;
            let mut step = b.signed(6);
            quant_step += step;
            if step == -32 || step == 31 {
                let negative = step == -32;
                let mut quant = 0;
                while b.pos + 5 < b.end {
                    step = b.read(5) as i32;
                    if step != 31 {
                        break;
                    }
                    quant += 31;
                }
                quant_step += if negative {
                    -(quant + step)
                } else {
                    quant + step
                };
            }
            if self.channels_for_cur_subframe == 1 {
                self.ch[self.channel_indexes[0]].quant_step = quant_step;
            } else {
                let modifier_len = b.read(3);
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes[i];
                    self.ch[c].quant_step = quant_step;
                    if b.bit() {
                        self.ch[c].quant_step += if modifier_len > 0 {
                            b.read(modifier_len) as i32 + 1
                        } else {
                            1
                        };
                    }
                }
            }
            self.scale_factors(b)?;
        }

        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes[i];
            if self.ch[c].transmit_coefs && b.pos < b.end {
                self.coefficients(b, c)?;
            } else {
                self.ch[c].coeffs[..subframe_len].fill(0.0);
            }
        }

        let at = offset + FRAME_SAMPLES / 2;
        if transmit_coeffs {
            self.inverse_channel_transform();
            let scale = 2.0 / subframe_len as f32 / 32768.0;
            let sfb = self.sfb_offsets[self.table_idx];
            let mut tmp = [0f32; FRAME_SAMPLES];
            for i in 0..self.channels_for_cur_subframe {
                let ch = &mut self.ch[self.channel_indexes[i]];
                let sf = ch.saved_scale_factors[ch.scale_factors];
                for band in 0..self.num_bands {
                    let end = sfb[band + 1].min(subframe_len);
                    let exp =
                        ch.quant_step - (ch.max_scale_factor - sf[band]) * ch.scale_factor_step;
                    let quant = 10f32.powf(exp as f32 / 20.0);
                    for (t, &c) in tmp[sfb[band]..end]
                        .iter_mut()
                        .zip(&ch.coeffs[sfb[band]..end])
                    {
                        *t = c * quant;
                    }
                }
                imdct_half(
                    &tmp[..subframe_len],
                    &mut ch.out[at..at + subframe_len],
                    scale,
                );
            }
        } else {
            for i in 0..self.channels_for_cur_subframe {
                self.ch[self.channel_indexes[i]].out[at..at + subframe_len].fill(0.0);
            }
        }

        for i in 0..self.channels_for_cur_subframe {
            let ch = &mut self.ch[self.channel_indexes[i]];
            let mut winlen = ch.prev_block_len;
            let mut start = at - winlen / 2;
            if subframe_len < winlen {
                start += (winlen - subframe_len) / 2;
                winlen = subframe_len;
            }
            window_overlap(
                &mut ch.out[start..start + winlen],
                &windows()[log2(winlen) as usize - 7],
            );
            ch.prev_block_len = subframe_len;
        }

        for i in 0..self.channels_for_cur_subframe {
            let ch = &mut self.ch[self.channel_indexes[i]];
            if ch.cur_subframe >= ch.num_subframes {
                return Err(Error("subframe overrun"));
            }
            ch.cur_subframe += 1;
        }
        if b.overread() {
            return Err(Error("subframe overread"));
        }
        Ok(())
    }

    fn inverse_channel_transform(&mut self) {
        if !self.transform || self.channels_for_cur_subframe != 2 {
            return;
        }
        let sfb = self.sfb_offsets[self.table_idx];
        let [a, b] = &mut self.ch;
        for band in 0..self.num_bands {
            let range = sfb[band]..sfb[band + 1].min(self.subframe_len);
            if self.transform_band[band] {
                for y in range {
                    let (l, r) = (a.coeffs[y], b.coeffs[y]);
                    (a.coeffs[y], b.coeffs[y]) = (l - r, l + r);
                }
            } else {
                for y in range {
                    a.coeffs[y] *= 181.0 / 128.0;
                    b.coeffs[y] *= 181.0 / 128.0;
                }
            }
        }
    }
}

fn windows() -> &'static [Vec<f32>; 3] {
    static WINDOWS: OnceLock<[Vec<f32>; 3]> = OnceLock::new();
    WINDOWS.get_or_init(|| [sine_window(128), sine_window(256), sine_window(512)])
}

fn window_overlap(block: &mut [f32], window: &[f32]) {
    let len = block.len() / 2;
    for k in 0..len {
        let (i, j) = (k, 2 * len - 1 - k);
        let (s0, s1) = (block[i], block[2 * len - 1 - k]);
        let (wi, wj) = (window[i], window[j]);
        block[i] = s0 * wj - s1 * wi;
        block[j] = s0 * wi + s1 * wj;
    }
}

fn to_i16(v: f32) -> i16 {
    (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_bits(data: &mut [u8], pos: usize, n: u32, value: u32) {
        for i in 0..n as usize {
            if value >> (n as usize - 1 - i) & 1 == 1 {
                data[(pos + i) / 8] |= 1 << (7 - (pos + i) % 8);
            }
        }
    }

    #[test]
    fn a_zero_length_frame_before_the_start_ends_the_search() {
        let mut data = vec![0u8; PACKET_BYTES];
        put_bits(&mut data, 0, 32, 2 << 26);
        let start = HEADER_BITS + 100;
        put_bits(&mut data, start, LENGTH_BITS, 200);
        let frame = read_frame(&data, start).expect("the frame at `start` is well formed");
        assert_eq!(frame.bits, 200);
        assert_eq!(frame.next, None);
    }

    fn imdct_reference(input: &[f32]) -> Vec<f32> {
        let n = input.len();
        (0..n)
            .map(|j| {
                let t = (j + n / 2) as f64 + 0.5 + n as f64 / 2.0;
                -(input
                    .iter()
                    .enumerate()
                    .map(|(k, &x)| {
                        x as f64 * (std::f64::consts::PI / n as f64 * t * (k as f64 + 0.5)).cos()
                    })
                    .sum::<f64>() as f32)
            })
            .collect()
    }

    #[test]
    fn fast_imdct_matches_the_direct_sum() {
        for n in [128usize, 256, 512] {
            let input: Vec<f32> = (0..n).map(|i| ((i * 7919) % 97) as f32 - 48.0).collect();
            let mut fast = vec![0f32; n];
            imdct_half(&input, &mut fast, 1.0);
            let reference = imdct_reference(&input);
            for (a, b) in fast.iter().zip(&reference) {
                assert!((a - b).abs() < 1e-2 * (1.0 + b.abs()), "n {n}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn imdct_and_window_reconstruct_a_signal() {
        let n = 256;
        let signal: Vec<f32> = (0..n * 6).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        let window = |i: usize| ((i as f64 + 0.5) * std::f64::consts::PI / (2.0 * n as f64)).sin();
        let mut out = vec![0f32; n * 7];
        for block in 0..5 {
            let base = block * n;
            let coeffs: Vec<f32> = (0..n)
                .map(|k| {
                    (0..2 * n)
                        .map(|i| {
                            let t = i as f64 + 0.5 + n as f64 / 2.0;
                            -signal[base + i] as f64
                                * window(i)
                                * (std::f64::consts::PI / n as f64 * t * (k as f64 + 0.5)).cos()
                        })
                        .sum::<f64>() as f32
                })
                .collect();
            let at = base + n / 2;
            imdct_half(&coeffs, &mut out[at..at + n], 2.0 / n as f32);
            if block > 0 {
                window_overlap(&mut out[at - n / 2..at + n / 2], &sine_window(n));
            }
        }
        for i in n..5 * n {
            assert!(
                (out[i] - signal[i]).abs() < 1e-3,
                "{i}: {} vs {}",
                out[i],
                signal[i]
            );
        }
    }

    #[test]
    fn packet_rejects_out_of_range_indices_and_partial_headers() {
        assert!(packet(&[0; 4], usize::MAX / PACKET_BYTES + 1).is_none());
        assert!(packet(&[0; 3], 0).is_none());
    }

    #[test]
    fn packet_headers_and_frame_walk() {
        let mut data = vec![0xffu8; 2 * PACKET_BYTES];
        let header = |frames: u32, first: u32| (frames << 26) | (first << 11);
        let bits = [(0, header(2, 0), 32), (PACKET_BITS, header(1, 48), 32)];
        let put = |data: &mut Vec<u8>, at: usize, value: u32, width: u32| {
            for i in 0..width as usize {
                let bit = (value >> (width as usize - 1 - i)) & 1;
                let p = at + i;
                data[p / 8] = (data[p / 8] & !(1 << (7 - p % 8))) | ((bit as u8) << (7 - p % 8));
            }
        };
        for &(at, v, w) in &bits {
            put(&mut data, at, v, w);
        }
        put(&mut data, 32, 100, 15);
        put(&mut data, 132, 16300, 15);
        let c_start = PACKET_BITS + 32 + 48;
        put(&mut data, c_start, 60, 15);
        let a = read_frame(&data, 32).unwrap();
        assert_eq!((a.bits, a.next), (100, Some(132)));
        let b = read_frame(&data, 132).unwrap();
        assert_eq!((b.bits, b.next), (16300, Some(c_start)));
        assert_eq!(b.data.len(), 16300usize.div_ceil(8));
        let c = read_frame(&data, c_start).unwrap();
        assert_eq!(c.bits, 60);
        assert_eq!(c.next, None);
    }

    #[test]
    #[ignore = "needs streams outside the repository"]
    fn walks_saved_streams() {
        let dir = std::env::var("OATHKEEP_XMA_STREAMS").unwrap();
        let mut bad = 0;
        let mut streams = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "xma") {
                continue;
            }
            streams += 1;
            let data = std::fs::read(&path).unwrap();
            let counted: u32 = (0..data.len() / PACKET_BYTES)
                .map(|i| packet(&data, i).unwrap().frames)
                .sum();
            let ctx = std::fs::read(path.with_extension("ctx")).unwrap();
            let word = u32::from_le_bytes(ctx[4..8].try_into().unwrap());
            let rate = [24_000, 32_000, 44_100, 48_000][(word >> 27 & 3) as usize];
            let mut decoder = Decoder::new(1 + (word >> 29 & 1) as usize, rate);
            let (mut walked, mut errors, mut peak, mut pcm) = (0, 0, 0i16, Vec::new());
            let mut at = Some(HEADER_BITS + packet(&data, 0).unwrap().first_frame_bit as usize);
            while let Some(frame) = at.and_then(|bit| read_frame(&data, bit)) {
                pcm.clear();
                errors += decoder.decode(&frame.data, frame.bits, &mut pcm).is_err() as u32;
                peak = pcm.iter().fold(peak, |p, s| p.max(s.saturating_abs()));
                walked += 1;
                at = frame.next;
            }
            println!(
                "{}: {} packets, {counted} frames counted, {walked} walked, {errors} bad, peak {peak}",
                path.display(),
                data.len() / PACKET_BYTES
            );
            bad += u32::from(walked != counted || errors > 0);
        }
        assert!(streams > 0, "OATHKEEP_XMA_STREAMS contains no .xma files");
        assert_eq!(bad, 0);
    }

    #[test]
    #[ignore = "needs reference files outside the repository"]
    fn matches_a_reference_decoder() {
        let dir = std::path::PathBuf::from(std::env::var("OATHKEEP_XMA_REFERENCE").unwrap());
        for (name, channels) in [("mono", 1), ("stereo", 2)] {
            let data = std::fs::read(dir.join(format!("{name}.xmadata"))).unwrap();
            let reference: Vec<i16> = std::fs::read(dir.join(format!("{name}.ref")))
                .unwrap()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| i16::from_le_bytes(b))
                .collect();
            let mut decoder = Decoder::new(channels, 48_000);
            let mut pcm = Vec::new();
            let mut at = Some(HEADER_BITS + packet(&data, 0).unwrap().first_frame_bit as usize);
            let mut errors = 0;
            while let Some(frame) = at.and_then(|bit| read_frame(&data, bit)) {
                errors += decoder.decode(&frame.data, frame.bits, &mut pcm).is_err() as usize;
                at = frame.next;
            }
            let best = (0..2048 / channels)
                .map(|d| {
                    let n = reference.len().min(pcm.len() - d * channels);
                    let e: f64 = (0..n)
                        .map(|i| (pcm[d * channels + i] as f64 - reference[i] as f64).powi(2))
                        .sum();
                    (e, d, n)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap();
            let signal: f64 = reference.iter().map(|&s| (s as f64).powi(2)).sum();
            let snr = 10.0 * (signal / best.0.max(1.0)).log10();
            println!(
                "{name}: {} samples, {errors} bad frames, delay {} samples, SNR {snr:.1} dB",
                pcm.len() / channels,
                best.1
            );
            assert!(errors == 0 && snr > 40.0);
        }
    }
}
