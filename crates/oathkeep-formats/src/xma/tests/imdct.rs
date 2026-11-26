use super::super::imdct::{Rotations, imdct_half, rotations, sine_window, window_overlap};

fn imdct_reference(input: &[f32]) -> Vec<f32> {
    let n = input.len();
    (0..n)
        .map(|j| {
            let t = (j + n / 2) as f64 + 0.5 + n as f64 / 2.0;
            -(input
                .iter()
                .enumerate()
                .map(|(k, &x)| x as f64 * (std::f64::consts::PI / n as f64 * t * (k as f64 + 0.5)).cos())
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

/// The transform as it was before [`Rotations`]: every rotation worked
/// out again on each call, the FFT on two fresh buffers.
fn imdct_half_untabled(input: &[f32], out: &mut [f32], scale: f32) {
    let n = input.len();
    let m = n / 2;
    let mut re = vec![0f64; m];
    let mut im = vec![0f64; m];
    for k in 0..m {
        let (a, b) = (input[2 * k] as f64, input[n - 1 - 2 * k] as f64);
        let (s, c) = (-std::f64::consts::PI * k as f64 / n as f64).sin_cos();
        (re[k], im[k]) = (a * c - b * s, a * s + b * c);
    }
    let mut j = 0;
    for i in 1..m {
        let mut bit = m >> 1;
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
    while len <= m {
        let (ws, wc) = (-2.0 * std::f64::consts::PI / len as f64).sin_cos();
        for start in (0..m).step_by(len) {
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
    for k in 0..m {
        let (s, c) = (-std::f64::consts::PI * (4 * k + 1) as f64 / (4 * n) as f64).sin_cos();
        let (wr, wi) = (re[k] * c - im[k] * s, re[k] * s + im[k] * c);
        out[n - 1 - 2 * k] = (wr as f32) * scale;
        out[2 * k] = -(wi as f32) * scale;
    }
}

/// Pseudo-random coefficients spread over many magnitudes, as dequantised
/// subframes are.
fn noise(n: usize, seed: u64) -> Vec<f32> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mantissa = (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
            mantissa * 2f32.powi((state % 24) as i32 - 4)
        })
        .collect()
}

/// The tables change nothing: every subframe size (and sizes the decoder
/// never uses, which build their tables on the spot) gives the bits the
/// untabled transform gives.
#[test]
fn tabled_imdct_gives_the_same_bits() {
    for n in [2usize, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048] {
        for seed in 0..16 {
            let input = noise(n, seed * 7919 + n as u64);
            let scale = 2.0 / n as f32 / 32768.0;
            let mut tabled = vec![0f32; n];
            let mut untabled = vec![0f32; n];
            imdct_half(&input, &mut tabled, scale);
            imdct_half_untabled(&input, &mut untabled, scale);
            let tabled: Vec<u32> = tabled.iter().map(|v| v.to_bits()).collect();
            let untabled: Vec<u32> = untabled.iter().map(|v| v.to_bits()).collect();
            assert_eq!(tabled, untabled, "n {n}, seed {seed}");
        }
    }
    let mut out = [0f32; 0];
    imdct_half(&[], &mut out, 1.0);
}

/// The cached tables are the ones the sizes ask for.
#[test]
fn cached_rotations_match_their_sizes() {
    for (r, n) in rotations().iter().zip([128usize, 256, 512]) {
        let fresh = Rotations::new(n);
        assert_eq!((&r.pre_re, &r.pre_im), (&fresh.pre_re, &fresh.pre_im));
        assert_eq!((&r.post_re, &r.post_im), (&fresh.post_re, &fresh.post_im));
        assert_eq!(r.reversed, fresh.reversed);
        assert_eq!(
            (&r.twiddle_re, &r.twiddle_im),
            (&fresh.twiddle_re, &fresh.twiddle_im)
        );
        assert_eq!(r.pre_re.len(), n / 2);
        assert_eq!(r.twiddle_re.len(), n / 2 - 1);
        // The order is a permutation that undoes itself.
        for (i, &to) in r.reversed.iter().enumerate() {
            assert_eq!(r.reversed[to as usize] as usize, i);
        }
    }
}

/// Release benchmark of one inverse transform per subframe size, tabled
/// against untabled:
/// `cargo test --release -p oathkeep-formats imdct_throughput -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn imdct_throughput() {
    for n in [128usize, 256, 512] {
        let inputs: Vec<Vec<f32>> = (0..64).map(|seed| noise(n, seed)).collect();
        let mut out = vec![0f32; n];
        let rounds = 20_000;
        let mut time = |f: fn(&[f32], &mut [f32], f32)| {
            let mut best = f64::MAX;
            for _ in 0..3 {
                let started = std::time::Instant::now();
                for i in 0..rounds {
                    f(
                        std::hint::black_box(&inputs[i % inputs.len()]),
                        &mut out,
                        1.0,
                    );
                    std::hint::black_box(&out);
                }
                best = best.min(started.elapsed().as_secs_f64() * 1e9 / rounds as f64);
            }
            best
        };
        let before = time(imdct_half_untabled);
        let after = time(imdct_half);
        println!(
            "n {n}: untabled {before:.0} ns, tabled {after:.0} ns, {:.1}x",
            before / after
        );
    }
}

/// Windowed MDCT blocks of a signal, put back together the way the
/// decoder does (half IMDCT, then window and overlap), give the signal.
#[test]
fn imdct_and_window_reconstruct_a_signal() {
    let n = 256;
    let signal: Vec<f32> = (0..n * 6).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
    // The 2n-sample sine window; its rising half is `sine_window(n)`.
    let window = |i: usize| ((i as f64 + 0.5) * std::f64::consts::PI / (2.0 * n as f64)).sin();
    let mut out = vec![0f32; n * 7];
    for block in 0..5 {
        let base = block * n;
        let coeffs: Vec<f32> = (0..n)
            .map(|k| {
                (0..2 * n)
                    .map(|i| {
                        let t = i as f64 + 0.5 + n as f64 / 2.0;
                        // FFmpeg's forward MDCT pairs with its inverse
                        // by sign.
                        -signal[base + i] as f64
                            * window(i)
                            * (std::f64::consts::PI / n as f64 * t * (k as f64 + 0.5)).cos()
                    })
                    .sum::<f64>() as f32
            })
            .collect();
        // The half output is the middle of the 2n window.
        let at = base + n / 2;
        imdct_half(&coeffs, &mut out[at..at + n], 2.0 / n as f32);
        if block > 0 {
            window_overlap(&mut out[at - n / 2..at + n / 2], &sine_window(n));
        }
    }
    for i in n..5 * n {
        assert!((out[i] - signal[i]).abs() < 1e-3, "{i}: {} vs {}", out[i], signal[i]);
    }
}
