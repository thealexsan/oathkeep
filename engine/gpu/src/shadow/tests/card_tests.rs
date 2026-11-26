use super::{CardSpan, PIECE, card_hashed_of, card_piece_of, card_span_of, joined, joined_piece_runs};

/// Joined runs hold every piece of the runs given, once, and no other.
#[test]
fn piece_runs_join_without_losing_or_adding_pieces() {
    let run = |first: u32, pieces: u32| CardSpan {
        from: first * 65536,
        to: (first + pieces) * 65536,
        first,
        pieces,
        skew: 7,
    };
    let runs = vec![run(40, 1), run(3, 2), run(4, 1), run(5, 3), run(40, 1), run(9, 1), run(41, 2), run(100, 1)];
    let mut wanted: Vec<u32> = runs.iter().flat_map(|r| r.first..r.first + r.pieces).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let joined = joined_piece_runs(runs);
    let got: Vec<u32> = joined.iter().flat_map(|r| r.first..r.first + r.pieces).collect();
    assert_eq!(got, wanted);
    assert_eq!(joined.iter().map(|r| (r.first, r.pieces)).collect::<Vec<_>>(), [(3, 5), (9, 1), (40, 3), (100, 1)]);
    assert!(joined.iter().all(|r| r.skew == 7));
    assert!(joined_piece_runs(Vec::new()).is_empty());
}

/// The offsets the compare's shader takes for a piece (`CARD_COMPARE`
/// in host.rs), before it widens them to 16 bytes.
fn shader_piece(span: &CardSpan, i: u32) -> (u32, u32) {
    let piece = span.first + i;
    let lo = piece * 65536;
    let lo = if lo > span.skew { lo - span.skew } else { 0 };
    let hi = (piece + 1) * 65536 - span.skew;
    (lo.max(span.from), hi.min(span.to))
}

/// The pieces of a span cover it without gaps, and the shader's
/// arithmetic names the same bytes as ours.
#[test]
fn the_shader_and_the_merge_cut_the_same_pieces() {
    for start in [0usize, 4096, PIECE - 16, PIECE, 3 * PIECE + 0x1f00, 0xd_b790_df00] {
        let len = 5 * PIECE + 4096;
        for (from, to) in [(0, len), (0, 1), (16, PIECE), (PIECE - 4, PIECE + 4), (2 * PIECE, len), (12345, 4 * PIECE + 7)] {
            let span = card_span_of(start, from, to);
            let mut at = from;
            for i in 0..span.pieces {
                let (lo, hi) = card_piece_of(start, len, &span, i);
                assert_eq!(lo, at, "{start:#x} {from} {to} piece {i}");
                assert!(hi > lo && hi - lo <= PIECE);
                assert_eq!(shader_piece(&span, i), (lo as u32, hi as u32), "{start:#x} {from} {to} piece {i}");
                at = hi;
            }
            assert_eq!(at, to, "{start:#x} {from} {to}");
        }
    }
}

/// Changed ranges are joined where they overlap or touch, and only there.
/// What is fetched of a piece to hash it is what the shader hashes.
#[test]
fn a_piece_is_hashed_here_as_on_the_card() {
    for start in [0usize, 0x10_0000, 0xd_5edb_c780, 0x12_3456_7008] {
        for size in [1usize << 16, 5 << 16, 0x20_0000] {
            let span = card_span_of(start, 0, size);
            for i in 0..span.pieces {
                // As `CARD_COMPARE` has it.
                let (piece, skew, size) = (span.first + i, span.skew, size as u32);
                let lo = (piece * 65536).saturating_sub(skew);
                let hi = ((piece + 1) * 65536 - skew).min(size);
                let shader = (lo & !15, (hi + 15) & !15);
                let here = card_hashed_of(start, size as usize, piece as usize);
                assert_eq!((here.0 as u32, here.1 as u32), shader, "start {start:#x} size {size:#x} piece {piece}");
            }
        }
    }
}

#[test]
fn ranges_join_where_they_touch() {
    assert_eq!(joined(vec![(8, 12), (0, 4), (4, 6), (10, 20), (30, 31)]), [(0, 6), (8, 20), (30, 31)]);
    assert!(joined(Vec::new()).is_empty());
}

#[test]
fn card_tier_reserves_top_headroom_for_large_buffers_under_tight_caps() {
    use super::card_tier_most;
    const MIB: u64 = 1 << 20;
    let cap = 640 * MIB;
    assert_eq!(card_tier_most(64 << 10, cap, false), 320 * MIB);
    assert_eq!(card_tier_most(512 << 10, cap, false), 320 * MIB);
    assert_eq!(card_tier_most(MIB, cap, false), 560 * MIB);
    assert_eq!(card_tier_most(4 * MIB, cap, false), 560 * MIB);
    assert_eq!(card_tier_most(8 * MIB, cap, false), cap);
    assert_eq!(card_tier_most(24 * MIB, cap, false), cap);
    // `card_all` or >1 GiB caps use the full limit.
    assert_eq!(card_tier_most(64 << 10, cap, true), cap);
    assert_eq!(card_tier_most(64 << 10, 2048 * MIB, false), 2048 * MIB);
    assert_eq!(card_tier_most(64 << 10, 64 * MIB, false), 64 * MIB);
}

#[test]
fn avx2_diff_and_merge_match_scalar_across_boundaries() {
    use super::{
        SNAP_CHUNK, back_piece, card_merge_bytes, card_merge_bytes_scalar, changed_runs,
        changed_runs_scalar, merge_piece, merge_piece_scalar, split_copy_chunks,
    };
    let mut seed = 0x9e37_79b9u32;
    let mut rng = || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        seed >> 8
    };
    for len in [0usize, 1, 7, 31, 32, 33, 63, 64, 65, 255, 256, 257, 511, 512, 513, 1055] {
        let old: Vec<u8> = (0..len).map(|_| rng() as u8).collect();
        let mut now = old.clone();
        let mut title = old.clone();
        // Perturb across 32-byte and 256-byte boundaries, plus overlapping CPU/GPU writes.
        for &edge in &[0usize, 31, 32, 63, 64, 255, 256, 511, 512] {
            if edge < len {
                now[edge] ^= 0x5a;
            }
            if edge + 1 < len {
                title[edge + 1] ^= 0x33;
            }
        }
        for _ in 0..16 {
            if len > 0 {
                let p = rng() as usize % len;
                if rng() & 1 == 0 {
                    now[p] = rng() as u8;
                } else {
                    title[p] = rng() as u8;
                }
            }
        }
        assert_eq!(changed_runs(&now, &old), changed_runs_scalar(&now, &old), "changed_runs len={len}");

        let (mut h1, mut t1) = (old.clone(), title.clone());
        let (mut h2, mut t2) = (old.clone(), title.clone());
        assert_eq!(
            merge_piece(&now, &mut h1, &mut t1),
            merge_piece_scalar(&now, &mut h2, &mut t2, 0),
            "merge_piece bool len={len}"
        );
        assert_eq!((&h1, &t1), (&h2, &t2), "merge_piece bytes len={len}");

        let (mut o1, mut ct1) = (old.clone(), title.clone());
        let (mut o2, mut ct2) = (old.clone(), title.clone());
        assert_eq!(
            card_merge_bytes(&now, &mut o1, &mut ct1),
            card_merge_bytes_scalar(&now, &mut o2, &mut ct2, 0),
            "card_merge_bytes bool len={len}"
        );
        assert_eq!((&o1, &ct1), (&o2, &ct2), "card_merge_bytes bytes len={len}");

        if len > 0 {
            let mut host_a = now.clone();
            let (mut s_title, mut s_mask, mut s_runs) = (Vec::new(), Vec::new(), Vec::new());
            let got = back_piece(&mut host_a, &old, &mut s_title, &mut s_mask, &mut s_runs, |first, into| {
                into.copy_from_slice(&title[first..first + into.len()]);
                true
            });
            // Verify byte-by-byte reference semantics of `back_piece`.
            if let Some((first, span_n)) = got {
                let mut exp_title = title[first..first + span_n].to_vec();
                let mut exp_mask = vec![0u8; span_n];
                for k in 0..span_n {
                    let h = now[first + k];
                    let o = old[first + k];
                    let t = exp_title[k];
                    if h != o && t == o {
                        exp_mask[k] = h ^ t;
                        exp_title[k] = h;
                    }
                }
                assert_eq!(s_title, exp_title, "back_piece title len={len}");
                assert_eq!(&host_a[first..first + span_n], &exp_title[..], "back_piece host len={len}");
                assert_eq!(s_mask, exp_mask, "back_piece mask len={len}");
                assert_eq!(s_runs, changed_runs_scalar(&s_mask, &vec![0u8; span_n]), "back_piece runs len={len}");
            }
        }
    }
    let chunks = split_copy_chunks(vec![(0x1000, 0x2000, 2 * SNAP_CHUNK + 17), (0x9000, 0xa000, 64)], SNAP_CHUNK);
    assert_eq!(
        chunks,
        vec![
            (0x1000, 0x2000, SNAP_CHUNK),
            (0x1000 + SNAP_CHUNK, 0x2000 + SNAP_CHUNK, SNAP_CHUNK),
            (0x1000 + 2 * SNAP_CHUNK, 0x2000 + 2 * SNAP_CHUNK, 17),
            (0x9000, 0xa000, 64),
        ]
    );
    let mut raw = vec![
        (0x1800, 0x2800, 0x800),
        (0x1000, 0x2000, 0x1000),
        (0x1400, 0x2400, 0x600),
        (0x5000, 0x7000, 0),
        (0x8000, 0x9000, 64),
        (0x8040, 0xa040, 64), // different (src - dst) delta -> must NOT coalesce
    ];
    super::coalesce_copy_ranges(&mut raw);
    assert_eq!(raw, vec![(0x1000, 0x2000, 0x1000), (0x8000, 0x9000, 64), (0x8040, 0xa040, 64)]);
    let batches = super::pack_copy_batches(&chunks, super::SNAP_BATCH);
    let flat: Vec<usize> = batches.into_iter().flatten().collect();
    assert_eq!(flat, (0..chunks.len()).collect::<Vec<_>>());
}
