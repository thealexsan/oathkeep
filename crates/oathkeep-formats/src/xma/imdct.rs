use std::sync::OnceLock;

use super::FRAME_SAMPLES;
use super::bits::log2;

/// Half-length inverse MDCT as FFmpeg defines it (`av_tx` MDCT, inverse):
/// `n` coefficients to the middle `n` samples of the 2n-sample window,
/// negated and scaled by `scale`. Computed as a DCT-IV through an
/// n/2-point complex FFT.
///
/// The rotations come from [`Rotations`], built once per subframe size, so
/// a subframe costs no sines or cosines and no allocation.
pub(super) fn imdct_half(input: &[f32], out: &mut [f32], scale: f32) {
    let n = input.len();
    let m = n / 2;
    let built;
    let rotations = match rotations().get(log2(n).wrapping_sub(7) as usize) {
        Some(r) if r.pre_re.len() == m => r,
        _ => {
            built = Rotations::new(n);
            &built
        }
    };
    let mut stack = [0f64; FRAME_SAMPLES];
    let mut heap = Vec::new();
    let (re, im) = if m <= stack.len() / 2 {
        stack.split_at_mut(m)
    } else {
        heap.resize(2 * m, 0f64);
        heap.split_at_mut(m)
    };
    let im = &mut im[..m];
    // Pre-rotation, each point stored at its bit-reversed place, which is
    // where the FFT's input permutation would move it.
    let pre = rotations
        .pre_re
        .iter()
        .zip(&rotations.pre_im)
        .zip(&rotations.reversed);
    for (k, ((&c, &s), &to)) in pre.enumerate() {
        let (a, b) = (input[2 * k] as f64, input[n - 1 - 2 * k] as f64);
        re[to as usize] = a * c - b * s;
        im[to as usize] = a * s + b * c;
    }
    rotations.butterflies(re, im);
    let post = rotations.post_re.iter().zip(&rotations.post_im);
    for (k, ((&re, &im), (&c, &s))) in re.iter().zip(im.iter()).zip(post).enumerate() {
        let (wr, wi) = (re * c - im * s, re * s + im * c);
        // DCT-IV: c[2k] = Re, c[n-1-2k] = -Im; output j is c[n-1-j].
        out[n - 1 - 2 * k] = (wr as f32) * scale;
        out[2 * k] = -(wi as f32) * scale;
    }
}

/// Everything [`imdct_half`] needs for one size `n` that does not depend on
/// the coefficients: the pre-rotation `exp(-iπk/n)` and post-rotation
/// `exp(-iπ(4k+1)/4n)` of each of the n/2 FFT points, the FFT's bit-reversed
/// order and its butterfly rotations. Each value is computed with the same
/// expression (for the butterflies the same running product) the transform
/// used to work out on every call, so it gives the same bits.
pub(super) struct Rotations {
    pub(super) pre_re: Vec<f64>,
    pub(super) pre_im: Vec<f64>,
    pub(super) post_re: Vec<f64>,
    pub(super) post_im: Vec<f64>,
    /// Where the bit-reversal permutation puts each point.
    pub(super) reversed: Vec<u16>,
    /// The butterfly rotations of every stage, the stage of length `len`
    /// starting at `len / 2 - 1`.
    pub(super) twiddle_re: Vec<f64>,
    pub(super) twiddle_im: Vec<f64>,
}

impl Rotations {
    pub(super) fn new(n: usize) -> Rotations {
        let m = n / 2;
        let (pre_re, pre_im) = (0..m)
            .map(|k| {
                let (s, c) = (-std::f64::consts::PI * k as f64 / n as f64).sin_cos();
                (c, s)
            })
            .unzip();
        let (post_re, post_im) = (0..m)
            .map(|k| {
                let (s, c) =
                    (-std::f64::consts::PI * (4 * k + 1) as f64 / (4 * n) as f64).sin_cos();
                (c, s)
            })
            .unzip();
        // The swaps of the in-place bit-reversal, applied to the identity.
        let mut reversed: Vec<u16> = (0..m as u16).collect();
        let mut j = 0;
        for i in 1..m {
            let mut bit = m >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                reversed.swap(i, j);
            }
        }
        let mut twiddle_re = Vec::with_capacity(m.saturating_sub(1));
        let mut twiddle_im = Vec::with_capacity(m.saturating_sub(1));
        let mut len = 2;
        while len <= m {
            let (ws, wc) = (-2.0 * std::f64::consts::PI / len as f64).sin_cos();
            let (mut cr, mut ci) = (1.0, 0.0);
            for _ in 0..len / 2 {
                twiddle_re.push(cr);
                twiddle_im.push(ci);
                (cr, ci) = (cr * wc - ci * ws, cr * ws + ci * wc);
            }
            len <<= 1;
        }
        Rotations {
            pre_re,
            pre_im,
            post_re,
            post_im,
            reversed,
            twiddle_re,
            twiddle_im,
        }
    }

    /// The butterfly stages of an in-place radix-2 forward DFT
    /// (`exp(-2πi nk/N)`) on points already in bit-reversed order.
    fn butterflies(&self, re: &mut [f64], im: &mut [f64]) {
        let m = re.len().min(im.len());
        let mut len = 2;
        while len <= m {
            let half = len / 2;
            let wr = &self.twiddle_re[half - 1..][..half];
            let wi = &self.twiddle_im[half - 1..][..half];
            let blocks = re[..m]
                .chunks_exact_mut(len)
                .zip(im[..m].chunks_exact_mut(len));
            for (re, im) in blocks {
                let (are, bre) = re.split_at_mut(half);
                let (aim, bim) = im.split_at_mut(half);
                butterfly_block(are, aim, bre, bim, wr, wi);
            }
            len <<= 1;
        }
    }
}

/// One block of butterflies: `a + w·b` into `a`, `a − w·b` into `b`.
#[inline(always)]
pub(super) fn butterfly_block(
    are: &mut [f64],
    aim: &mut [f64],
    bre: &mut [f64],
    bim: &mut [f64],
    wr: &[f64],
    wi: &[f64],
) {
    let lanes = are
        .iter_mut()
        .zip(aim)
        .zip(bre.iter_mut().zip(bim))
        .zip(wr.iter().zip(wi));
    for (((ar, ai), (br, bi)), (&cr, &ci)) in lanes {
        let (tr, ti) = (*br * cr - *bi * ci, *br * ci + *bi * cr);
        (*br, *bi) = (*ar - tr, *ai - ti);
        (*ar, *ai) = (*ar + tr, *ai + ti);
    }
}

/// [`Rotations`] for subframes of 128, 256 and 512 samples.
pub(super) fn rotations() -> &'static [Rotations; 3] {
    static ROTATIONS: OnceLock<[Rotations; 3]> = OnceLock::new();
    ROTATIONS.get_or_init(|| {
        [
            Rotations::new(128),
            Rotations::new(256),
            Rotations::new(512),
        ]
    })
}

/// `ff_sine_windows`: the rising half of a sine window of length `n`.
pub(super) fn sine_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i as f64 + 0.5) * std::f64::consts::PI / (2.0 * n as f64)).sin() as f32)
        .collect()
}

/// Sine windows for blocks of 128, 256 and 512 samples.
pub(super) fn windows() -> &'static [Vec<f32>; 3] {
    static WINDOWS: OnceLock<[Vec<f32>; 3]> = OnceLock::new();
    WINDOWS.get_or_init(|| [sine_window(128), sine_window(256), sine_window(512)])
}

/// FFmpeg's `vector_fmul_window` in place: the first half of `block` holds
/// the previous block's tail, the second half the current block's head.
pub(super) fn window_overlap(block: &mut [f32], window: &[f32]) {
    let len = block.len() / 2;
    for k in 0..len {
        let (i, j) = (k, 2 * len - 1 - k);
        let (s0, s1) = (block[i], block[2 * len - 1 - k]);
        let (wi, wj) = (window[i], window[j]);
        block[i] = s0 * wj - s1 * wi;
        block[j] = s0 * wi + s1 * wj;
    }
}

pub(super) fn to_i16(v: f32) -> i16 {
    (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}
