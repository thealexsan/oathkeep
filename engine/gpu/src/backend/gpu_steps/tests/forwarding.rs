use super::*;

/// Reference backwards resolver used only to verify exact equivalence of
/// [`Forwarding::build`]'s single-pass `ActivePlaced` interval map.
fn reference_placed_segments(does: &[Does], at: usize) -> Vec<(usize, usize, usize)> {
    let (r, _) = reads_writes(&does[at]).unwrap();
    let total = r.1.saturating_sub(r.0);
    if total == 0 {
        return vec![(0, r.0, 0)];
    }
    let mut segs: Vec<(usize, usize, usize, bool)> = vec![(0, r.0, total, false)];
    for k in (0..at).rev() {
        if segs.iter().all(|s| s.3) {
            break;
        }
        match does[k] {
            Does::Copy { dst, len, .. } if len > 0 => {
                let c = (dst, dst + len);
                if !segs.iter().any(|&(_, a, n, locked)| !locked && meet((a, a + n), c)) {
                    continue;
                }
                let mut next = Vec::with_capacity(segs.len() + 2);
                for (off, a, n, locked) in segs {
                    let b = a + n;
                    if locked || !meet((a, b), c) {
                        next.push((off, a, n, locked));
                        continue;
                    }
                    if a < c.0 {
                        next.push((off, a, c.0 - a, false));
                    }
                    let (lo, hi) = (a.max(c.0), b.min(c.1));
                    next.push((off + (lo - a), lo, hi - lo, true));
                    if b > c.1 {
                        next.push((off + (c.1 - a), c.1, b - c.1, false));
                    }
                }
                segs = next;
            }
            Does::Placed { dst, src, len, .. } if len > 0 => {
                let p = (dst, dst + len);
                if !segs.iter().any(|&(_, a, n, locked)| !locked && meet((a, a + n), p)) {
                    continue;
                }
                let mut next = Vec::with_capacity(segs.len() + 2);
                for (off, a, n, locked) in segs {
                    let b = a + n;
                    if locked || !meet((a, b), p) {
                        next.push((off, a, n, locked));
                        continue;
                    }
                    if a < p.0 {
                        next.push((off, a, p.0 - a, false));
                    }
                    let (lo, hi) = (a.max(p.0), b.min(p.1));
                    next.push((off + (lo - a), src + (lo - dst), hi - lo, false));
                    if b > p.1 {
                        next.push((off + (p.1 - a), p.1, b - p.1, false));
                    }
                }
                segs = next;
            }
            _ => {}
        }
    }
    let mut out: Vec<(usize, usize, usize)> = Vec::with_capacity(segs.len());
    for (off, a, n, _) in segs {
        if let Some(last) = out.last_mut()
            && last.0 + last.2 == off
            && last.1 + last.2 == a
        {
            last.2 += n;
        } else {
            out.push((off, a, n));
        }
    }
    out
}

#[test]
fn single_placed_forwarding_remaps_same_list_reads() {
    let p = placed_item(0x9_9273_8280, 0x9_6071_0400, 0xbd0);
    let f = fill(0x9_9273_8280, 0xf0, true);
    let does = [p, f];
    let (chosen, fwd) = plan_forwarded(&does).unwrap();
    assert_eq!(chosen, vec![(1, 0)]);
    assert_eq!(segs_of(&fwd, &does, 1), vec![(0, 0x9_6071_0400, 0xf0)]);
    // Fully covered single-interval forwarding does not touch `seg_pool`.
    assert!(fwd.seg_pool.is_empty());

    // Partial left/right straddling around a single placed copy.
    let straddle = [placed_item(2000, 5000, 32), fill(1992, 48, true)];
    let (chosen, fwd) = plan_forwarded(&straddle).unwrap();
    assert_eq!(chosen, vec![(1, 0)]);
    assert_eq!(segs_of(&fwd, &straddle, 1), vec![(0, 1992, 8), (8, 5000, 32), (40, 2032, 8)]);

    // Taken copy ahead of placed copy elevates forwarded fill to level 1.
    let dep = [copy(3000, 7000, 64, true), placed_item(2000, 3000, 64), fill(2000, 64, false)];
    let (chosen, fwd) = plan_forwarded(&dep).unwrap();
    assert_eq!(chosen, vec![(0, 0), (2, 1)]);
    assert_eq!(segs_of(&fwd, &dep, 2), vec![(0, 3000, 64)]);
}

#[test]
fn chained_placed_copies_forward_transitively_and_coalesce() {
    // Multi-hop chain with partial overlap on both ends.
    let chain = [
        placed_item(6000, 8000, 64),
        placed_item(4000, 6008, 48),
        placed_item(2000, 4016, 32),
        fill(2008, 48, true),
    ];
    let (chosen, fwd) = plan_forwarded(&chain).unwrap();
    assert_eq!(chosen, vec![(3, 0)]);
    assert_eq!(segs_of(&fwd, &chain, 3), vec![(0, 8032, 24), (24, 2032, 24)]);
    assert_eq!(segs_of(&fwd, &chain, 3), reference_placed_segments(&chain, 3));

    // Adjacent collinear placed copies coalesce back into a single piece.
    let coalesced = [
        placed_item(2000, 5000, 32),
        placed_item(2032, 5032, 32),
        fill(2016, 32, true),
    ];
    let (chosen, fwd) = plan_forwarded(&coalesced).unwrap();
    assert_eq!(chosen, vec![(2, 0)]);
    assert_eq!(segs_of(&fwd, &coalesced, 2), vec![(0, 5016, 32)]);
    assert!(fwd.seg_pool.is_empty());

    // Round-trip placed copy `A -> B` then `B -> A` restores identity cleanly.
    let round_trip = [
        placed_item(3000, 2000, 64),
        placed_item(2000, 3000, 64),
        fill(2000, 64, true),
    ];
    let (_, fwd) = plan_forwarded(&round_trip).unwrap();
    assert_eq!(segs_of(&fwd, &round_trip, 2), vec![(0, 2000, 64)]);
}

#[test]
fn copy_overwrite_splits_and_invalidates_active_placed_intervals() {
    // A Copy overwriting the middle `[4016, 4048)` of an active Placed
    // `[4000, 4064) -> 9000` splits `active` into left `[4000, 4016) -> 9000`
    // and right `[4048, 4064) -> 9048`, while locking `[4016, 4048)` to the
    // Copy's destination.
    let does = [
        placed_item(4000, 9000, 64),
        copy(4016, 7000, 32, false),
        placed_item(2000, 4000, 64),
        fill(2000, 64, true),
    ];
    let fwd = Forwarding::build(&does, true, true);
    assert_eq!(
        segs_of(&fwd, &does, 3),
        vec![(0, 9000, 16), (16, 4016, 32), (48, 9048, 16)]
    );
    assert_eq!(segs_of(&fwd, &does, 3), reference_placed_segments(&does, 3));
    // Because the Copy at index 1 wrote `[4016, 4048)` behind `placed_item(4000, 9000, 64)`
    // and is pulled in by `fill(2000, 64, true)`, `plan_forwarded` refuses with `BEHIND_PLACED`.
    assert_eq!(plan_forwarded(&does).err(), Some(BEHIND_PLACED));

    // When the Copy is ahead of `placed_item(2000, 4000, 64)` with no earlier
    // placed copy on 4000, `plan_forwarded` pulls the Copy onto level 0 and
    // schedules the forwarded Fill on level 1.
    let valid = [
        copy(4016, 7000, 32, false),
        placed_item(2000, 4000, 64),
        fill(2000, 64, true),
    ];
    let (chosen, fwd) = plan_forwarded(&valid).unwrap();
    assert_eq!(chosen, vec![(0, 0), (2, 1)]);
    assert_eq!(segs_of(&fwd, &valid, 2), vec![(0, 4000, 64)]);
}

#[test]
fn self_overlap_and_conflicting_writes_are_rejected() {
    // Placed copy overlapping its own source (`dst != src && meet(dst, src)`).
    assert_eq!(plan_forwarded(&[placed_item(2000, 2032, 64), fill(2000, 64, true)]).err(), Some(BEHIND_PLACED));
    assert_eq!(plan_forwarded(&[placed_item(2032, 2000, 64), fill(2032, 64, true)]).err(), Some(BEHIND_PLACED));

    // Placed copy whose CPU scratch source waits on flights.
    let waiting = Does::Placed {
        dst: 2000,
        src: 3000,
        len: 64,
        cpu_src: true,
        waits: true,
    };
    assert_eq!(plan_forwarded(&[waiting, fill(2000, 64, true)]).err(), Some(BEHIND_PLACED));

    // Copy whose forwarded read maps onto its own destination (`a copy onto itself`).
    let forwarded_self_copy = [placed_item(2000, 5000, 64), copy(5016, 2000, 64, true)];
    assert_eq!(plan_forwarded(&forwarded_self_copy).err(), Some("a copy onto itself"));

    // Writes behind a placed copy onto its dst or src refuse.
    assert_eq!(
        plan_forwarded(&[placed_item(2000, 3000, 64), copy(2000, 7000, 8, true)]).err(),
        Some(BEHIND_PLACED)
    );
    assert_eq!(
        plan_forwarded(&[placed_item(2000, 3000, 64), copy(3000, 7000, 8, true)]).err(),
        Some(BEHIND_PLACED)
    );

    // CPU-sourced placed copy behind a taken copy onto its source refuses.
    let cpu_placed = Does::Placed {
        dst: 2000,
        src: 3000,
        len: 64,
        cpu_src: true,
        waits: false,
    };
    assert_eq!(
        plan_forwarded(&[copy(3000, 7000, 8, true), cpu_placed, fill(2000, 64, true)]).err(),
        Some(BEHIND_PLACED)
    );

    // Cross-list cut query skips same-list conflicts and finds the earliest
    // splittable cross-list cut in one pass.
    let cross = [
        placed_item(2000, 3000, 64),
        fill(2000, 16, true),
        fill(2016, 16, true),
    ];
    let steps = [(0, 100, 0), (0, 100, 1), (1, 200, 0)];
    let run_starts = [0, 2, 3];
    let splittable = [false, true];
    let mut behind = None;
    let mut query = CutQuery {
        steps: &steps,
        run_starts: &run_starts,
        splittable: &splittable,
        min_cut: None,
    };
    assert_eq!(
        plan_inner(&cross, 2, true, false, &mut behind, Some(&mut query)).err(),
        Some(BEHIND_PLACED)
    );
    assert_eq!(query.min_cut, Some(1));
}

#[test]
fn forwarded_placed_levels_preserve_exact_sequential_semantics() {
    let mut seed = 54321u32;
    let mut random = move |n: u32| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        (seed >> 16) % n
    };
    let (mut done, mut forwarded) = (0, 0);
    for _ in 0..4000 {
        let does: Vec<Does> = (0..2 + random(8))
            .map(|_| {
                let (at, other, len) = (random(60) as usize, random(60) as usize, 1 + random(8) as usize);
                let asked = random(4) == 0;
                match random(10) {
                    0..3 => copy(at, other, len, asked),
                    3..5 => placed_item(at, other, len),
                    5..8 => fill(at, len, asked),
                    8 => stuck(at, len),
                    _ => Does::Read {
                        from: at,
                        to: at + len,
                        asked: false,
                    },
                }
            })
            .collect();
        let Ok((chosen, fwd)) = plan_forwarded(&does) else {
            continue;
        };
        done += 1;
        // Verify both ActivePlaced equivalence and LevelSegTree equivalence!
        let order: Vec<usize> = chosen.iter().map(|&(i, _)| i).collect();
        let copies_n = order.iter().filter(|&&i| matches!(does[i], Does::Copy { .. })).count();
        assert_eq!(
            assign_levels_segtree(&does, &fwd, &order, copies_n),
            assign_levels_direct(&does, &fwd, &order)
        );
        for (idx, d) in does.iter().enumerate() {
            if matches!(d, Does::Copy { .. } | Does::Fill { to: true, .. }) {
                assert_eq!(segs_of(&fwd, &does, idx), reference_placed_segments(&does, idx));
            }
        }
        let start: Vec<u32> = (0..80).collect();
        let mut memory = start.clone();
        let mut wanted = Vec::new();
        for d in &does {
            let (from, to) = match *d {
                Does::Read { from, to, .. } => (from, to),
                Does::Placed { src, len, .. } => (src, src + len),
                _ => reads_writes(d).unwrap().0,
            };
            wanted.push(memory[from..to].to_vec());
            match *d {
                Does::Copy { dst, src, len, .. } | Does::Placed { dst, src, len, .. } => {
                    memory.copy_within(src..src + len, dst);
                }
                _ => {}
            }
        }
        let is_taken = |i: usize| chosen.binary_search_by_key(&i, |&(idx, _)| idx).is_ok();
        let untaken_races_placed = does.iter().enumerate().any(|(p, d)| {
            let Does::Placed { dst, src, len, .. } = *d else {
                return false;
            };
            does[p + 1..].iter().enumerate().any(|(o, later)| {
                !is_taken(p + 1 + o)
                    && matches!(*later, Does::Copy { dst: cd, len: cl, .. }
                        if meet((cd, cd + cl), (dst, dst + len)) || meet((cd, cd + cl), (src, src + len)))
            })
        });
        if untaken_races_placed {
            continue;
        }
        let mut cpu = start.clone();
        let mut card = start.clone();
        let mut cpu_stale: Vec<(usize, usize)> = Vec::new();
        for (i, d) in does.iter().enumerate() {
            if is_taken(i) {
                if let Some((_, w)) = reads_writes(d)
                    && w.0 < w.1
                {
                    insert_range(&mut cpu_stale, w.0, w.1);
                }
                continue;
            }
            match *d {
                Does::Placed { dst, src, len, .. } => {
                    if touched(&cpu_stale, (src, src + len)) {
                        insert_range(&mut cpu_stale, dst, dst + len);
                    } else if !touched(&cpu_stale, (dst, dst + len)) {
                        cpu.copy_within(src..src + len, dst);
                    }
                }
                Does::Copy { dst, src, len, .. } => {
                    assert_eq!(cpu[src..src + len], wanted[i][..]);
                    cpu.copy_within(src..src + len, dst);
                    card[dst..dst + len].copy_from_slice(&wanted[i]);
                }
                _ => {}
            }
        }
        for level in 0..=chosen.iter().map(|c| c.1).max().unwrap_or(0) {
            let of: Vec<usize> = chosen.iter().filter(|c| c.1 == level).map(|c| c.0).collect();
            let before = card.clone();
            for i in of {
                let segs = segs_of(&fwd, &does, i);
                let ((raw_from, raw_to), (dst, dst_end)) = reads_writes(&does[i]).unwrap();
                if segs.len() > 1 || segs[0].1 != raw_from {
                    forwarded += 1;
                }
                let mut assembled = vec![0u32; raw_to - raw_from];
                for (off, a, n) in segs {
                    assembled[off..off + n].copy_from_slice(&before[a..a + n]);
                }
                assert_eq!(assembled, wanted[i], "step {i} of {does:?}: {chosen:?}");
                if dst < dst_end {
                    card[dst..dst_end].copy_from_slice(&assembled);
                }
            }
        }
        for d in &does {
            if let Does::Placed { dst, src, len, .. } = *d {
                card.copy_within(src..src + len, dst);
            }
        }
        assert_eq!(card, memory, "{does:?}: {chosen:?}");
    }
    assert!(done > 500 && forwarded > 50, "{done} {forwarded}");
}

#[test]
fn stress_ten_thousand_steps_plans_fast_and_accurately() {
    const N: usize = 10_000;
    let mut does = Vec::with_capacity(N);
    // Realistic heavy submission: chained copies, placed ring copies, and
    // thousands of constant-buffer fills (both overlapping placed copies and
    // disjoint from them) exercising O(1) bounding-box skips, O(log P)
    // active-interval lookups, and O(N log N) segment-tree level assignment.
    for c in 0..64usize {
        let src = 0x1000_0000 + c * 128;
        let dst = 0x1000_0000 + (c + 1) * 128;
        does.push(copy(dst, src, 128, c == 0));
    }
    for p in 0..64usize {
        let src = 0x1000_0000 + (p + 1) * 128;
        let dst = 0x2000_0000 + p * 256;
        does.push(placed_item(dst, src, 128));
    }
    while does.len() < N {
        let idx = does.len();
        if idx % 5 == 0 {
            // Overlaps an active placed copy -> forwarded to copy chain!
            let p = (idx / 5) % 64;
            does.push(fill(0x2000_0000 + p * 256 + 16, 64, true));
        } else {
            // Disjoint ring fill -> O(1) bounding-box skip!
            does.push(fill(0x3000_0000 + idx * 64, 64, idx % 2 == 0));
        }
    }
    // Warm once, then measure.
    let _ = plan_forwarded(&does).unwrap();
    let started = std::time::Instant::now();
    let (chosen, fwd) = plan_forwarded(&does).unwrap();
    let elapsed = started.elapsed();
    assert!(chosen.len() > 5_000);
    // Chain of 64 dependent copies produces levels 0..=63, and fills reading
    // placed copy 63 (sourced from copy 63) land on level 64.
    let max_level = chosen.iter().map(|&(_, lvl)| lvl).max().unwrap_or(0);
    assert_eq!(max_level, 64);
    let first_fill = (128..N).find(|&idx| idx % 5 == 0).unwrap();
    let expected_p = (first_fill / 5) % 64;
    assert_eq!(
        segs_of(&fwd, &does, first_fill),
        vec![(0, 0x1000_0000 + (expected_p + 1) * 128 + 16, 64)]
    );
    eprintln!("stress 10,000 steps planned {} card ops in {:?}", chosen.len(), elapsed);
    let limit_ms = if cfg!(debug_assertions) { 75 } else { 5 };
    assert!(
        elapsed.as_millis() <= limit_ms,
        "10,000-step plan took {:?} (limit {limit_ms} ms)",
        elapsed
    );
}

#[test]
fn placed_order_defaults_on_unless_zero() {
    // Without OATHKEEP_PLACED_ORDER=0, placed_order() is enabled by default
    // so fills ahead of a placed copy are not refused.
    assert!(placed_order());
}
