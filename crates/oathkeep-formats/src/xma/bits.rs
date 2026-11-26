use std::sync::OnceLock;

use super::tables::*;

// Bitstream reading inside a frame.

pub(super) struct Bits<'a> {
    pub(super) data: &'a [u8],
    pub(super) pos: usize,
    pub(super) end: usize,
}

impl Bits<'_> {
    pub(super) fn peek32(&self) -> u32 {
        let mut v = 0u64;
        let byte = self.pos / 8;
        for i in 0..5 {
            v = v << 8 | *self.data.get(byte + i).unwrap_or(&0) as u64;
        }
        (v << (24 + self.pos % 8) >> 32) as u32
    }

    pub(super) fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek32() >> (32 - n);
        self.pos += n as usize;
        v
    }

    pub(super) fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    pub(super) fn signed(&mut self, n: u32) -> i32 {
        ((self.read(n) << (32 - n)) as i32) >> (32 - n)
    }

    pub(super) fn consumed(&self) -> usize {
        self.pos
    }

    pub(super) fn overread(&self) -> bool {
        self.pos > self.end
    }
}

/// A prefix code built like FFmpeg's `ff_vlc_init_from_lengths`: codes are
/// handed out in table order, each the next free one of its length.
pub(super) struct Vlc {
    /// Left-aligned code, length, symbol; sorted by code.
    pub(super) codes: Vec<(u32, u8, i32)>,
}

impl Vlc {
    pub(super) fn new(entries: impl Iterator<Item = (i32, u8)>) -> Vlc {
        let mut code = 0u64;
        let mut codes = Vec::new();
        for (symbol, len) in entries {
            codes.push((code as u32, len, symbol));
            code += 1 << (32 - len);
        }
        Vlc { codes }
    }

    pub(super) fn read(&self, bits: &mut Bits) -> i32 {
        let v = bits.peek32();
        let i = self.codes.partition_point(|c| c.0 <= v) - 1;
        let (_, len, symbol) = self.codes[i];
        bits.pos += len as usize;
        symbol
    }
}

pub(super) struct Codes {
    pub(super) scale: Vlc,
    pub(super) scale_rl: Vlc,
    pub(super) coef: [Vlc; 2],
    pub(super) vec4: Vlc,
    pub(super) vec2: Vlc,
    pub(super) vec1: Vlc,
}

pub(super) fn codes() -> &'static Codes {
    static CODES: OnceLock<Codes> = OnceLock::new();
    CODES.get_or_init(|| {
        let pairs = |t: &'static [(u16, u8)], offset: i32| {
            Vlc::new(t.iter().map(move |&(s, l)| (s as i32 + offset, l)))
        };
        Codes {
            scale: pairs(&SCALE, -60),
            scale_rl: pairs(&SCALE_RL, 0),
            coef: [
                Vlc::new(COEF0_SYMS.iter().zip(COEF0_LENS).map(|(&s, l)| (s as i32, l))),
                pairs(&COEF1, 0),
            ],
            vec4: Vlc::new(VEC4_SYMS.iter().zip(VEC4_LENS).map(|(&s, l)| (s as i32 - 1, l))),
            vec2: pairs(&VEC2, -1),
            vec1: pairs(&VEC1, 0),
        }
    })
}

/// `ff_wma_get_large_val`.
pub(super) fn large_value(bits: &mut Bits) -> u32 {
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

pub(super) fn log2(v: usize) -> u32 {
    usize::BITS - 1 - v.max(1).leading_zeros()
}

// The inverse transform.
