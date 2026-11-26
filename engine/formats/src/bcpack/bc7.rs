use super::plain::{add_index, nibbles};
use super::rans::{pick, references};
use super::{Error, HISTORY, Tables};

#[derive(Clone, Copy, Default)]
pub(super) struct Bc7Ep {
    pub(super) ctrl: u16,
    /// Per subset: R0 R1 G0 G1 B0 B1 A0 A1, left-aligned.
    pub(super) rows: [[u16; 8]; 3],
}

#[derive(Clone, Copy, Default)]
pub(super) struct Bc7Ix {
    pub(super) bits: [u8; 2],
    pub(super) ix: [[i16; 16]; 2],
}

/// Little-endian bit writer for one 128-bit block.
#[derive(Default)]
pub(super) struct Bits {
    pub(super) v: u128,
    pub(super) at: u32,
}

impl Bits {
    pub(super) fn put(&mut self, value: u64, n: u32) {
        if n > 0 && self.at < 128 {
            self.v |= (u128::from(value) & ((1u128 << n) - 1)) << self.at;
        }
        self.at += n;
    }
}

// BC7.

/// Public BC7 anchor indices: second subset of two; second and third of three.
pub(super) const ANCHOR2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8,
    2, 2, 8, 8, 2, 2, 15, 15, 6, 8, 2, 8, 15, 15, 2, 8, 2, 2, 2, 15, 15, 6, 6, 2, 6, 8, 15, 15, 2,
    2, 15, 15, 15, 15, 15, 2, 2, 15,
];
const ANCHOR3A: [u8; 64] = [
    3, 3, 15, 15, 8, 3, 15, 15, 8, 8, 6, 6, 6, 5, 3, 3, 3, 3, 8, 15, 3, 3, 6, 10, 5, 8, 8, 6, 8, 5,
    15, 15, 8, 15, 3, 5, 6, 10, 8, 15, 15, 3, 15, 5, 15, 15, 15, 15, 3, 15, 5, 5, 5, 8, 5, 10, 5,
    10, 8, 13, 15, 12, 3, 3,
];
const ANCHOR3B: [u8; 64] = [
    15, 8, 8, 3, 15, 15, 3, 8, 15, 15, 15, 15, 15, 15, 15, 8, 15, 8, 15, 3, 15, 8, 15, 8, 3, 15, 6,
    10, 15, 15, 10, 8, 15, 3, 15, 10, 10, 8, 9, 10, 6, 15, 8, 15, 3, 6, 6, 8, 15, 3, 15, 15, 15,
    15, 15, 15, 15, 15, 15, 15, 3, 15, 15, 8,
];

/// Endpoint bits of R G B A per mode (public), before p-bits and rotation.
const BC7_BITS: [[u8; 4]; 8] = [
    [4, 4, 4, 0],
    [6, 6, 6, 0],
    [5, 5, 5, 0],
    [7, 7, 7, 0],
    [5, 5, 5, 6],
    [7, 7, 7, 8],
    [7, 7, 7, 7],
    [5, 5, 5, 5],
];
/// Index bits of the primary and secondary sets per mode.
const BC7_INDEX: [[u8; 2]; 8] = [
    [3, 0],
    [3, 0],
    [2, 0],
    [2, 0],
    [3, 2],
    [2, 2],
    [4, 0],
    [2, 0],
];

fn pbits(mode: u16) -> bool {
    matches!(mode, 0 | 1 | 3 | 6 | 7)
}

/// Channel precisions: the p-bit is red's lowest bit; rotation swaps alpha
/// with the rotated channel.
fn bc7_precision(ctrl: u16) -> [u8; 4] {
    let (mode, rot) = (ctrl & 7, usize::from(ctrl >> 3 & 3));
    let mut p = BC7_BITS[usize::from(mode)];
    if rot != 0 {
        p.swap(3, rot - 1);
    } else if pbits(mode) {
        p[0] += 1;
    }
    p
}

pub(super) fn bc7<'a>(
    records: impl Iterator<Item = &'a [u8]>,
    out: impl Iterator<Item = &'a mut [u8]>,
    t: &Tables,
) -> Result<(), Error> {
    let mut history = vec![(Bc7Ep::default(), Bc7Ix::default()); HISTORY];
    let zero = (Bc7Ep::default(), Bc7Ix::default());
    for (i, (s, block)) in records.zip(out).enumerate() {
        let (ep_ref, ix_ref) = references(s, true);
        let ctrl = t.ctrl7[usize::from(s[31] & 15) << 5 | usize::from(s[30] & 31)];
        if ctrl == 0 {
            return Err(Error("unused BC7 control code"));
        }
        let mode = usize::from(ctrl & 7);
        let subsets = usize::from((ctrl & 0xff) >> 6);
        let prec = bc7_precision(ctrl);
        let three = subsets == 3;
        let or0 = |v: u8| if three { 0 } else { v };
        let if3 = |v: u8| if three { v } else { 0 };
        let stored = [
            [s[4], s[5], s[6], s[7], s[8], s[9], or0(s[10]), or0(s[11])],
            [
                s[12],
                s[13],
                s[14],
                s[15],
                s[16],
                s[17],
                or0(s[18]),
                or0(s[19]),
            ],
            [
                s[20],
                s[21],
                if3(s[10]),
                if3(s[11]),
                if3(s[18]),
                if3(s[19]),
                0,
                0,
            ],
        ];
        let r = &pick(i, ep_ref, &history, &t.bc7, &zero).0;
        let mut ep = Bc7Ep {
            ctrl,
            ..Default::default()
        };
        // Rows the reference lacks build on its first row.
        let common = subsets.min(usize::from((r.ctrl & 0xff) >> 6));
        let columns = if prec[3] > 0 { 8 } else { 6 };
        for c in 0..columns {
            let p = prec[c / 2];
            if p == 0 {
                continue;
            }
            let mask = 0xffffu16 << (16 - p);
            for (row, st) in stored.iter().enumerate().take(subsets) {
                let v = st[c];
                let residual =
                    (u16::from(v >> 1) ^ (v & 1).wrapping_neg() as i8 as u16) << (16 - p);
                let base = r.rows[if row < common { row } else { 0 }][c] & mask;
                ep.rows[row][c] = base.wrapping_add(residual);
            }
        }
        let bits = BC7_INDEX[mode];
        let rx = &pick(i, ix_ref, &history, &t.bc7, &zero).1;
        let mut ix = Bc7Ix {
            bits,
            ..Default::default()
        };
        for (set, offsets) in [[22, 24, 26, 28], [12, 14, 16, 20]].into_iter().enumerate() {
            if bits[set] == 0 {
                continue;
            }
            for (k, o) in offsets.into_iter().enumerate() {
                for (b, &byte) in s[o..o + 2].iter().enumerate() {
                    let pair = nibbles(byte, k < 3);
                    for (h, &v) in pair.iter().enumerate() {
                        let p = 4 * k + 2 * b + h;
                        ix.ix[set][p] = add_index(v, bits[set], rx.ix[set][p], rx.bits[set]);
                    }
                }
            }
        }
        history[i % HISTORY] = (ep, ix);
        bc7_pack(&ep, &ix, prec, block);
    }
    Ok(())
}

fn bc7_pack(ep: &Bc7Ep, ix: &Bc7Ix, prec: [u8; 4], out: &mut [u8]) {
    let ctrl = ep.ctrl;
    let mode = ctrl & 7;
    let subsets = usize::from((ctrl & 0xff) >> 6);
    let part = usize::from(ctrl >> 8 & 63);
    let rot = usize::from(ctrl >> 3 & 3);
    let mut w = Bits::default();
    w.put(1 << mode, u32::from(mode) + 1);
    match mode {
        0 => w.put(part as u64, 4),
        1 | 2 | 3 | 7 => w.put(part as u64, 6),
        4 => {
            w.put(rot as u64, 2);
            w.put(u64::from(ctrl >> 5 & 1), 1);
        }
        5 => w.put(rot as u64, 2),
        _ => {}
    }
    let ends = 2 * subsets;
    let field = |row: usize, col: usize, p: u8| u64::from(ep.rows[row][col] >> (16 - p));
    let mut order = [0, 1, 2, 3];
    if rot != 0 {
        order.swap(3, rot - 1);
    }
    for ch in order {
        let p = prec[ch];
        if p == 0 {
            continue;
        }
        let drop = u32::from(ch == 0 && pbits(mode));
        for e in 0..ends {
            w.put(field(e / 2, 2 * ch + e % 2, p) >> drop, u32::from(p) - drop);
        }
    }
    match mode {
        1 => (0..subsets).for_each(|row| w.put(field(row, 0, prec[0]) & 1, 1)),
        m if pbits(m) => (0..ends).for_each(|e| w.put(field(e / 2, e % 2, prec[0]) & 1, 1)),
        _ => {}
    }
    let anchors: &[u8] = match subsets {
        2 => &[0, ANCHOR2[part]],
        3 => &[0, ANCHOR3A[part], ANCHOR3B[part]],
        _ => &[0],
    };
    let mut indices = |set: usize| {
        let bits = ix.bits[set];
        for (p, &v) in ix.ix[set].iter().enumerate() {
            let n = u32::from(bits) - u32::from(anchors.contains(&(p as u8)));
            w.put(u64::from(v as u16), n);
        }
    };
    match mode {
        4 => {
            indices(1);
            indices(0);
        }
        5 => {
            indices(0);
            indices(1);
        }
        _ => indices(0),
    }
    out.copy_from_slice(&w.v.to_le_bytes());
}
