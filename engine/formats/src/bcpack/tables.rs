use super::bc6::{Bc6Ep, Bc6Ix};
use super::bc7::{Bc7Ep, Bc7Ix};
use super::plain::Plain;
use super::{BUCKETS, DICTIONARY, Error, MODELS};

/// Bytes of the table file (see [`Tables::parse`]).
pub const TABLES_SIZE: usize = MODELS * BUCKETS * 2
    + 256
    + 64
    + 256
    + 64
    + 64
    + 512 * 2
    + DICTIONARY * (6 + 16 + 8 + 32 + 2 + 16 + 4 + 32 + 25 + 33 + 50 + 66);

/// The format's constant tables.
pub struct Tables {
    pub(super) freq: [[u32; BUCKETS]; MODELS],
    /// Each bucket's first slot; 65536 past the model's last, and one more
    /// 65536 at the end so a bucket always has a next start.
    pub(super) start: [[u32; BUCKETS + 1]; MODELS],
    /// Per model, the bucket of the first slot of each 256-slot window: the
    /// search for a slot's bucket starts there and seldom moves on.
    pub(super) bucket: Box<[[u8; 256]; MODELS]>,
    /// ceil(2^32 / freq): exact division for the offsets inside a bucket.
    pub(super) recip: [[u64; BUCKETS]; MODELS],
    pub(super) rank1: [u8; 256],
    pub(super) rank3a: [u8; 64],
    pub(super) rank3c: [u8; 256],
    pub(super) rank4: [u8; 64],
    pub(super) rank5: [u8; 64],
    pub(super) ctrl7: [u16; 512],
    pub(super) bc1: Vec<Plain>,
    pub(super) bc3: Vec<Plain>,
    pub(super) bc4: Vec<Plain>,
    pub(super) bc5: Vec<Plain>,
    pub(super) bc6: Vec<(Bc6Ep, Bc6Ix)>,
    pub(super) bc7: Vec<(Bc7Ep, Bc7Ix)>,
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let (head, rest) = self.0.split_at(N);
        self.0 = rest;
        head.try_into().unwrap_or([0; N])
    }
    fn u8(&mut self) -> u8 {
        self.bytes::<1>()[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.bytes())
    }
}

impl Tables {
    /// The tables Oathkeep ships with.
    pub fn builtin() -> Result<Tables, Error> {
        Tables::parse(include_bytes!("../bcpack-tables.bin"))
    }

    /// Reads the table file: 128 x 9 u16 model frequencies (per bucket, the
    /// frequency of each of its symbols), the rank tables (BC1 256 B, BC3
    /// alpha 64 B and colour 256 B, BC4 64 B, BC5 64 B), 512 u16 BC7 control
    /// words, then 255 dictionary entries per format in format order: BC1
    /// 6 + 16 B, BC3 8 + 32 B, BC4 2 + 16 B, BC5 4 + 32 B, BC6H (u8 mode,
    /// 12 u16, u8 mode, 16 i16), BC7 (u16 control, 24 u16, 2 u8, 2 B,
    /// 16 + 15 i16). Little-endian, no padding. Every model must tile 65536.
    pub fn parse(bytes: &[u8]) -> Result<Tables, Error> {
        if bytes.len() != TABLES_SIZE {
            return Err(Error("table file has the wrong size"));
        }
        let mut r = Reader(bytes);
        let mut freq = [[0u32; BUCKETS]; MODELS];
        let mut start = [[0u32; BUCKETS + 1]; MODELS];
        let mut bucket = Box::new([[0u8; 256]; MODELS]);
        let mut recip = [[0u64; BUCKETS]; MODELS];
        for m in 0..MODELS {
            for f in &mut freq[m] {
                *f = u32::from(r.u16());
            }
            // Model 16 g + shape has g + 2 buckets; bucket b > 0 holds 2^(b-1) symbols.
            let used = m / 16 + 2;
            let mut at = 0u32;
            for b in 0..used {
                if freq[m][b] == 0 {
                    return Err(Error("a model has an empty bucket"));
                }
                start[m][b] = at;
                recip[m][b] = (1u64 << 32).div_ceil(u64::from(freq[m][b]));
                at += freq[m][b] << b.saturating_sub(1);
            }
            if at != 1 << 16 {
                return Err(Error("a model does not tile 65536"));
            }
            start[m][used..].fill(1 << 16);
            for (window, b) in bucket[m].iter_mut().enumerate() {
                let first = (window as u32) << 8;
                *b = start[m][1..].iter().filter(|&&s| s <= first).count() as u8;
            }
        }
        let rank1 = r.bytes();
        let rank3a = r.bytes();
        let rank3c = r.bytes();
        let rank4 = r.bytes();
        let rank5 = r.bytes();
        let mut ctrl7 = [0u16; 512];
        for c in &mut ctrl7 {
            *c = r.u16();
        }
        let mut plain = |ep: usize, ix: usize| {
            (0..DICTIONARY)
                .map(|_| {
                    let mut p = Plain::default();
                    for v in &mut p.ep[..ep] {
                        *v = r.u8();
                    }
                    for v in &mut p.ix[..ix] {
                        *v = r.u8();
                    }
                    p
                })
                .collect::<Vec<_>>()
        };
        let bc1 = plain(6, 16);
        let bc3 = plain(8, 32);
        let bc4 = plain(2, 16);
        let bc5 = plain(4, 32);
        let bc6 = (0..DICTIONARY)
            .map(|_| {
                // The endpoint entry's mode byte plays no part in decoding.
                r.u8();
                let mut ep = Bc6Ep::default();
                for v in ep.e.as_flattened_mut() {
                    *v = r.u16();
                }
                let mut ix = Bc6Ix {
                    mode: r.u8(),
                    ..Default::default()
                };
                for v in &mut ix.ix {
                    *v = r.u16() as i16;
                }
                (ep, ix)
            })
            .collect();
        let bc7 = (0..DICTIONARY)
            .map(|_| {
                let mut ep = Bc7Ep {
                    ctrl: r.u16(),
                    ..Default::default()
                };
                for v in ep.rows.as_flattened_mut() {
                    *v = r.u16();
                }
                let mut ix = Bc7Ix {
                    bits: [r.u8(), r.u8()],
                    ..Default::default()
                };
                // Unused: the widths again in all entries but the first.
                r.u16();
                // Only 15 slots for set 1, which no entry uses (0 bits).
                for v in &mut ix.ix.as_flattened_mut()[..31] {
                    *v = r.u16() as i16;
                }
                (ep, ix)
            })
            .collect();
        Ok(Tables {
            freq,
            start,
            bucket,
            recip,
            rank1,
            rank3a,
            rank3c,
            rank4,
            rank5,
            ctrl7,
            bc1,
            bc3,
            bc4,
            bc5,
            bc6,
            bc7,
        })
    }
}
