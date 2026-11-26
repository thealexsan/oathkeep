use super::rans::{pick, references, zz};
use super::{Error, HISTORY, Tables};

/// Endpoints and indices of BC1, BC3, BC4 and BC5: small unsigned values,
/// summed per element modulo the element's width.
#[derive(Clone, Copy, Default)]
pub(super) struct Plain {
    pub(super) ep: [u8; 8],
    pub(super) ix: [u8; 32],
}

/// Element widths of the plain formats' endpoints and indices.
pub(super) struct Widths {
    ep: [u8; 8],
    ix: [u8; 32],
    paired: bool,
}

const fn widths(ep: [u8; 8], ix3: usize, ix2: usize, paired: bool) -> Widths {
    let mut ix = [0; 32];
    let mut k = 0;
    while k < ix3 + ix2 {
        ix[k] = if k < ix3 { 3 } else { 2 };
        k += 1;
    }
    Widths { ep, ix, paired }
}

pub(super) const BC1: Widths = widths([5, 6, 5, 5, 6, 5, 0, 0], 0, 16, false);
pub(super) const BC3: Widths = widths([5, 6, 5, 5, 6, 5, 8, 8], 16, 16, true);
pub(super) const BC4: Widths = widths([8, 8, 0, 0, 0, 0, 0, 0], 16, 0, false);
pub(super) const BC5: Widths = widths([8, 8, 8, 8, 0, 0, 0, 0], 32, 0, true);

pub(super) fn plain<'a>(
    records: impl Iterator<Item = &'a [u8]>,
    out: impl Iterator<Item = &'a mut [u8]>,
    dictionary: &[Plain],
    w: &Widths,
    residual: impl Fn(&[u8]) -> Plain,
    pack: fn(&Plain, &mut [u8]),
) -> Result<(), Error> {
    let mut history = vec![Plain::default(); HISTORY];
    let zero = Plain::default();
    for (i, (s, block)) in records.zip(out).enumerate() {
        let (ep_ref, ix_ref) = references(s, w.paired);
        let ep_ref = &pick(i, ep_ref, &history, dictionary, &zero).ep;
        let ix_ref = &pick(i, ix_ref, &history, dictionary, &zero).ix;
        let mut v = residual(s);
        for ((v, r), bits) in v.ep.iter_mut().zip(ep_ref).zip(w.ep) {
            *v = v.wrapping_add(*r) & ((1u16 << bits) - 1) as u8;
        }
        for ((v, r), bits) in v.ix.iter_mut().zip(ix_ref).zip(w.ix) {
            *v = v.wrapping_add(*r) & ((1u8 << bits) - 1);
        }
        history[i % HISTORY] = v;
        pack(&v, block);
    }
    Ok(())
}

/// 2-bit index residuals: one symbol per 2x2 quad.
pub(super) fn quads(rank: &[u8; 256], s: &[u8], ix: &mut [u8]) {
    for (q, &sym) in s.iter().enumerate() {
        let v = rank[usize::from(sym)];
        let base = (q >> 1) * 8 + (q & 1) * 2;
        for (k, p) in [0, 1, 4, 5].into_iter().enumerate() {
            ix[base + p] = v >> (2 * k) & 3;
        }
    }
}

/// 3-bit index residuals: one symbol per two pixels in raster order.
pub(super) fn pairs(rank: &[u8; 64], s: &[u8], ix: &mut [u8]) {
    for (k, &sym) in s.iter().enumerate() {
        let v = rank[usize::from(sym & 63)];
        ix[2 * k] = v & 7;
        ix[2 * k + 1] = v >> 4 & 7;
    }
}

pub(super) fn bc1_residual(t: &Tables, s: &[u8]) -> Plain {
    let mut v = Plain::default();
    for (k, bits) in [5, 6, 5, 5, 6, 5].into_iter().enumerate() {
        v.ep[k] = zz(s[2 + k], bits);
    }
    quads(&t.rank1, &s[8..12], &mut v.ix);
    v
}

pub(super) fn bc3_residual(t: &Tables, s: &[u8]) -> Plain {
    // Colour lanes carry each channel's two endpoints together.
    let mut v = Plain {
        ep: [
            zz(s[14], 5),
            zz(s[16], 6),
            zz(s[18], 5),
            zz(s[15], 5),
            zz(s[17], 6),
            zz(s[19], 5),
            zz(s[4], 8),
            zz(s[5], 8),
        ],
        ..Default::default()
    };
    pairs(&t.rank3a, &s[6..14], &mut v.ix[..16]);
    quads(&t.rank3c, &s[20..24], &mut v.ix[16..]);
    v
}

pub(super) fn bc4_residual(t: &Tables, s: &[u8]) -> Plain {
    let mut v = Plain::default();
    v.ep[..2].copy_from_slice(&[zz(s[2], 8), zz(s[3], 8)]);
    pairs(&t.rank4, &s[4..12], &mut v.ix[..16]);
    v
}

pub(super) fn bc5_residual(t: &Tables, s: &[u8]) -> Plain {
    let mut v = Plain::default();
    for k in 0..4 {
        v.ep[k] = zz(s[4 + k], 8);
    }
    pairs(&t.rank5, &s[8..24], &mut v.ix);
    v
}

/// Indices along the endpoint line -> BC1 and BC4 codes.
const BC1_CODE: [u32; 4] = [0, 2, 3, 1];
pub(super) const BC4_CODE: [u64; 8] = [0, 2, 3, 4, 5, 6, 7, 1];

pub(super) fn bc1_color(ep: &[u8], ix: &[u8], out: &mut [u8]) {
    let c0 = u16::from(ep[0]) << 11 | u16::from(ep[1]) << 5 | u16::from(ep[2]);
    let c1 = u16::from(ep[3]) << 11 | u16::from(ep[4]) << 5 | u16::from(ep[5]);
    let bits = (0..16).fold(0, |acc, p| {
        acc | BC1_CODE[usize::from(ix[p] & 3)] << (2 * p)
    });
    out[..2].copy_from_slice(&c0.to_le_bytes());
    out[2..4].copy_from_slice(&c1.to_le_bytes());
    out[4..8].copy_from_slice(&bits.to_le_bytes());
}

/// BC4 half: endpoints are kept 128 below their stored value.
pub(super) fn bc4_alpha(e0: u8, e1: u8, ix: &[u8], out: &mut [u8]) {
    let bits = (0..16).fold(0u64, |acc, p| {
        acc | BC4_CODE[usize::from(ix[p] & 7)] << (16 + 3 * p)
    });
    let v = u64::from(e0 ^ 0x80) | u64::from(e1 ^ 0x80) << 8 | bits;
    out[..8].copy_from_slice(&v.to_le_bytes());
}

pub(super) fn bc1_pack(v: &Plain, out: &mut [u8]) {
    bc1_color(&v.ep, &v.ix, out);
}

pub(super) fn bc3_pack(v: &Plain, out: &mut [u8]) {
    bc4_alpha(v.ep[6], v.ep[7], &v.ix[..16], &mut out[..8]);
    bc1_color(&v.ep, &v.ix[16..], &mut out[8..]);
}

pub(super) fn bc4_pack(v: &Plain, out: &mut [u8]) {
    bc4_alpha(v.ep[0], v.ep[1], &v.ix, out);
}

pub(super) fn bc5_pack(v: &Plain, out: &mut [u8]) {
    bc4_alpha(v.ep[0], v.ep[1], &v.ix[..16], &mut out[..8]);
    bc4_alpha(v.ep[2], v.ep[3], &v.ix[16..], &mut out[8..]);
}

/// A byte's even and odd bits as two zigzag-coded 4-bit residuals.
pub(super) fn nibbles(b: u8, odd_first: bool) -> [i16; 2] {
    let half = |v: u8| (0..4).fold(0u8, |acc, k| acc | (v >> (2 * k) & 1) << k);
    let z = |u: u8| i16::from(u >> 1) ^ -i16::from(u & 1);
    let (even, odd) = (z(half(b)), z(half(b >> 1)));
    if odd_first { [odd, even] } else { [even, odd] }
}

/// Index residual plus reference, both left-aligned to 16 bits at their own
/// widths, back to `bits` bits (signed).
pub(super) fn add_index(cur: i16, bits: u8, refv: i16, ref_bits: u8) -> i16 {
    let up = |v: i16, b: u8| {
        if b == 0 || b > 16 {
            0
        } else {
            (v as u16) << (16 - b)
        }
    };
    let sum = up(cur, bits).wrapping_add(up(refv, ref_bits)) as i16;
    if bits == 0 { 0 } else { sum >> (16 - bits) }
}
