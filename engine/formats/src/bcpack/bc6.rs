use super::bc7::{ANCHOR2, Bits};
use super::plain::{add_index, nibbles};
use super::rans::{pick, references};
use super::{Error, HISTORY};

#[derive(Clone, Copy, Default)]
pub(super) struct Bc6Ep {
    /// Endpoint 0 R G B, then endpoints 1-3, left-aligned and biased.
    pub(super) e: [[u16; 3]; 4],
}

#[derive(Clone, Copy, Default)]
pub(super) struct Bc6Ix {
    pub(super) mode: u8,
    pub(super) ix: [i16; 16],
}

// BC6H.

/// Bits of endpoint 0, then of the other endpoints' R, G, B, per mode 1-14.
const BC6_BITS: [[u8; 4]; 15] = [
    [0; 4],
    [10, 5, 5, 5],
    [7, 6, 6, 6],
    [11, 5, 4, 4],
    [11, 4, 5, 4],
    [11, 4, 4, 5],
    [9, 5, 5, 5],
    [8, 6, 5, 5],
    [8, 5, 6, 5],
    [8, 5, 5, 6],
    [6, 6, 6, 6],
    [10, 10, 10, 10],
    [11, 9, 9, 9],
    [12, 8, 8, 8],
    [16, 4, 4, 4],
];
/// Mode code and its width, modes 1-14.
const BC6_CODE: [(u64, u32); 15] = [
    (0, 0),
    (0, 2),
    (1, 2),
    (2, 5),
    (6, 5),
    (10, 5),
    (14, 5),
    (18, 5),
    (22, 5),
    (26, 5),
    (30, 5),
    (3, 5),
    (7, 5),
    (11, 5),
    (15, 5),
];

/// A run of the public BC6H layout: `len` bits of channel `ch` (0-2; 3 is
/// the partition) of endpoint `ep`, from bit `first` up.
#[derive(Clone, Copy)]
struct Run {
    ch: u8,
    ep: u8,
    first: u8,
    len: u8,
}

const fn r(ch: u8, ep: u8, first: u8, len: u8) -> Run {
    Run { ch, ep, first, len }
}

const R: u8 = 0;
const G: u8 = 1;
const B: u8 = 2;
const D: Run = r(3, 0, 0, 5);

/// The header after the mode bits, per mode 1-14. Modes 13 and 14 write the
/// high base bits in ascending order (what the console's packer does).
#[rustfmt::skip]
const BC6_LAYOUT: [&[Run]; 15] = [
    &[],
    &[
        r(G, 2, 4, 1), r(B, 2, 4, 1), r(B, 3, 4, 1), r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10),
        r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4), r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4),
        r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5),
        r(B, 3, 3, 1), D,
    ],
    &[
        r(G, 2, 5, 1), r(G, 3, 4, 1), r(G, 3, 5, 1), r(R, 0, 0, 7), r(B, 3, 0, 1), r(B, 3, 1, 1),
        r(B, 2, 4, 1), r(G, 0, 0, 7), r(B, 2, 5, 1), r(B, 3, 2, 1), r(G, 2, 4, 1), r(B, 0, 0, 7),
        r(B, 3, 3, 1), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 6),
        r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 6), r(R, 3, 0, 6), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 5), r(R, 0, 10, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 4), r(G, 0, 10, 1), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 4), r(B, 0, 10, 1),
        r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 1), r(G, 3, 4, 1),
        r(G, 2, 0, 4), r(G, 1, 0, 5), r(G, 0, 10, 1), r(G, 3, 0, 4), r(B, 1, 0, 4), r(B, 0, 10, 1),
        r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 4), r(B, 3, 0, 1), r(B, 3, 2, 1), r(R, 3, 0, 4),
        r(G, 2, 4, 1), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 1), r(B, 2, 4, 1),
        r(G, 2, 0, 4), r(G, 1, 0, 4), r(G, 0, 10, 1), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 5),
        r(B, 0, 10, 1), r(B, 2, 0, 4), r(R, 2, 0, 4), r(B, 3, 1, 1), r(B, 3, 2, 1), r(R, 3, 0, 4),
        r(B, 3, 4, 1), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 9), r(B, 2, 4, 1), r(G, 0, 0, 9), r(G, 2, 4, 1), r(B, 0, 0, 9), r(B, 3, 4, 1),
        r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4), r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4),
        r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5),
        r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 8), r(G, 3, 4, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(B, 3, 2, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(B, 3, 3, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 5),
        r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 6),
        r(R, 3, 0, 6), D,
    ],
    &[
        r(R, 0, 0, 8), r(B, 3, 0, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(G, 2, 5, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(G, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 6), r(G, 3, 0, 4), r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5),
        r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 8), r(B, 3, 1, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(B, 2, 5, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 5),
        r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 6), r(G, 3, 4, 1), r(B, 3, 0, 1), r(B, 3, 1, 1), r(B, 2, 4, 1), r(G, 0, 0, 6),
        r(G, 2, 5, 1), r(B, 2, 5, 1), r(B, 3, 2, 1), r(G, 2, 4, 1), r(B, 0, 0, 6), r(G, 3, 5, 1),
        r(B, 3, 3, 1), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 6),
        r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 6), r(R, 3, 0, 6), D,
    ],
    &[r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 10), r(G, 1, 0, 10), r(B, 1, 0, 10)],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 9), r(R, 0, 10, 1), r(G, 1, 0, 9),
        r(G, 0, 10, 1), r(B, 1, 0, 9), r(B, 0, 10, 1),
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 8), r(R, 0, 10, 2), r(G, 1, 0, 8),
        r(G, 0, 10, 2), r(B, 1, 0, 8), r(B, 0, 10, 2),
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 6), r(G, 1, 0, 4),
        r(G, 0, 10, 6), r(B, 1, 0, 4), r(B, 0, 10, 6),
    ],
];

fn bc6_index_bits(mode: u8) -> u8 {
    match mode {
        1..=10 => 3,
        11..=14 => 4,
        _ => 0,
    }
}

pub(super) fn bc6<'a>(
    records: impl Iterator<Item = &'a [u8]>,
    out: impl Iterator<Item = &'a mut [u8]>,
    dictionary: &[(Bc6Ep, Bc6Ix)],
) -> Result<(), Error> {
    let mut history = vec![(Bc6Ep::default(), Bc6Ix::default()); HISTORY];
    let zero = (
        Bc6Ep::default(),
        Bc6Ix {
            mode: 11,
            ..Default::default()
        },
    );
    let z = |v: u16| (v >> 1) ^ (v & 1).wrapping_neg();
    let wide = |a: u8, b: u8| {
        (0..8).fold(0u16, |acc, k| {
            acc | u16::from(a >> k & 1) << (2 * k) | u16::from(b >> k & 1) << (2 * k + 1)
        })
    };
    for (i, (s, block)) in records.zip(out).enumerate() {
        let (ep_ref, ix_ref) = references(s, true);
        let mode = s[30].wrapping_add(1);
        if !(1..=14).contains(&mode) {
            return Err(Error("BC6H mode out of range"));
        }
        let two = mode < 11;
        let shift = BC6_BITS[usize::from(mode)].map(|b| 16 - u32::from(b));
        let e0 = [wide(s[4], s[5]), wide(s[6], s[7]), wide(s[8], s[9])].map(z);
        let e1 = [wide(s[10], s[11]), wide(s[12], s[13]), wide(s[14], s[15])].map(z);
        let (e2, e3) = if two {
            (
                [s[16], s[18], s[20]].map(|v| z(v.into())),
                [s[17], s[19], s[21]].map(|v| z(v.into())),
            )
        } else {
            (e0, e1)
        };
        let r = &pick(i, ep_ref, &history, dictionary, &zero).0;
        let mut ep = Bc6Ep::default();
        // Endpoint 0 at the base precision, the others per channel; the
        // reference is cut by the mask after the sum.
        let mask = |sh: u32| 0xffffu16 << sh;
        for ((e, &r), res) in ep.e[0].iter_mut().zip(&r.e[0]).zip(e0) {
            *e = r.wrapping_add(res << shift[0]) & mask(shift[0]);
        }
        for (k, res) in [e1, e2, e3]
            .iter()
            .enumerate()
            .take(if two { 3 } else { 1 })
        {
            for c in 0..3 {
                ep.e[k + 1][c] =
                    r.e[k + 1][c].wrapping_add(res[c] << shift[1 + c]) & mask(shift[1 + c]);
            }
        }
        if !two {
            for c in 0..3 {
                ep.e[2][c] = (ep.e[0][c] >> shift[0]) << shift[1];
            }
            ep.e[3] = ep.e[1];
        }
        let rx = &pick(i, ix_ref, &history, dictionary, &zero).1;
        let bits = bc6_index_bits(mode);
        let mut ix = Bc6Ix {
            mode,
            ..Default::default()
        };
        for k in 0..8 {
            for (h, v) in nibbles(s[22 + k], k < 6).into_iter().enumerate() {
                let p = 2 * k + h;
                ix.ix[p] = add_index(v, bits, rx.ix[p], bc6_index_bits(rx.mode));
            }
        }
        history[i % HISTORY] = (ep, ix);
        // Fields: the working value has its top bit flipped.
        let field = |k: usize, c: usize| {
            let sh = if k == 0 { shift[0] } else { shift[1 + c] };
            u64::from((ep.e[k][c] ^ 0x8000) >> sh)
        };
        let mut w = Bits::default();
        let (code, n) = BC6_CODE[usize::from(mode)];
        w.put(code, n);
        let part = s[31] & 31;
        for run in BC6_LAYOUT[usize::from(mode)] {
            if run.ch == 3 {
                w.put(part.into(), 5);
            } else {
                w.put(
                    field(run.ep.into(), run.ch.into()) >> run.first,
                    run.len.into(),
                );
            }
        }
        let anchor = if two { ANCHOR2[usize::from(part)] } else { 0 };
        for (p, &v) in ix.ix.iter().enumerate() {
            let short = p == 0 || p == usize::from(anchor);
            let v = (v as u16).wrapping_add(1 << (bits - 1));
            w.put(v.into(), u32::from(bits) - u32::from(short));
        }
        block.copy_from_slice(&w.v.to_le_bytes());
    }
    Ok(())
}
