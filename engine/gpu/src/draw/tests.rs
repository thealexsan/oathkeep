//! Unit tests for `draw` submodules.

mod heap_range_tests {
    use super::super::heap_ranges::{HEAP_ROUND, HeapRanges, SLOT_FLAG_OBJECT, SLOT_FLAG_UAV};

    /// Round by round every slot is validated or pruned in its part, O(1)
    /// removals maintain exact union spans per copy, and dropping the last slot
    /// of a copy drops its entry immediately.
    #[test]
    fn every_slot_is_looked_at_again_in_a_round() {
        const SLOTS: u32 = 1000;
        let mut ranges: HeapRanges<u64> = HeapRanges::default();
        ranges.set(7, None);
        assert_eq!(ranges.views_count(), 0);
        assert_eq!(ranges.copies_len(), 0);

        for slot in (0..SLOTS).step_by(3) {
            let copy_id = (slot % 17) as usize + 1;
            let words = [0xfeed_face, slot, 1];
            ranges.set(
                slot,
                Some((
                    copy_id,
                    copy_id as u64,
                    u64::from(slot),
                    u64::from(slot) + 16,
                    words,
                    0,
                )),
            );
        }
        assert_eq!(ranges.views_count(), SLOTS.div_ceil(3));
        assert_eq!(ranges.copies_len(), 17);

        let parts = HEAP_ROUND as u32;
        for pass in 1..=HEAP_ROUND {
            let part = (pass % HEAP_ROUND) as u32;
            let live: Vec<u32> = (0..SLOTS)
                .step_by(3)
                .filter(|slot| slot % parts == part)
                .collect();
            ranges.prune_missing_in_part(part, parts, &live);
            for &slot in &live {
                let copy_id = (slot % 17) as usize + 1;
                let cached = ranges.get(slot).expect("live slot preserved");
                assert_eq!(cached.words, [0xfeed_face, slot, 1]);
                ranges.set(
                    slot,
                    Some((
                        copy_id,
                        copy_id as u64,
                        pass,
                        pass + u64::from(slot) + 1,
                        [0xfeed_face, slot, 2],
                        SLOT_FLAG_UAV,
                    )),
                );
            }
            assert_eq!(ranges.views_count(), SLOTS.div_ceil(3));
        }
        assert!(ranges.copy_spans().all(|(_, lo, hi)| lo >= 1 && hi > lo));

        // Removing a boundary slot recomputes the shrunk span accurately, and
        // removing all slots for a copy drops the copy entry immediately.
        let mut shrink: HeapRanges<std::sync::Arc<u64>> = HeapRanges::default();
        let shared = std::sync::Arc::new(99u64);
        let key = std::sync::Arc::as_ptr(&shared) as usize;
        shrink.set(
            2,
            Some((key, shared.clone(), 100, 200, [1, 2, 3], SLOT_FLAG_OBJECT)),
        );
        shrink.set(5, Some((key, shared.clone(), 120, 160, [1, 5, 3], 0)));
        shrink.set(8, Some((key, shared.clone(), 50, 300, [1, 8, 3], 0)));
        assert_eq!(std::sync::Arc::strong_count(&shared), 2);
        assert_eq!(
            shrink.copy_spans().collect::<Vec<_>>(),
            vec![(&shared, 50, 300)]
        );
        // Drop outer-bound slot 8 -> span shrinks to [100, 200].
        shrink.set(8, None);
        assert_eq!(shrink.views_count(), 2);
        assert_eq!(
            shrink.copy_spans().collect::<Vec<_>>(),
            vec![(&shared, 100, 200)]
        );
        // Prune part 0 with empty live set -> drops slot 2 (2 % 3 == 2 stays, 2 % 2 == 0 goes).
        shrink.prune_missing_in_part(0, 2, &[]);
        assert_eq!(shrink.views_count(), 1);
        assert_eq!(
            shrink.copy_spans().collect::<Vec<_>>(),
            vec![(&shared, 120, 160)]
        );
        // Drop last slot 5 -> `Arc` reference inside `HeapRanges` is released immediately.
        shrink.set(5, None);
        assert_eq!(shrink.views_count(), 0);
        assert_eq!(shrink.copies_len(), 0);
        assert_eq!(std::sync::Arc::strong_count(&shared), 1);
    }

    /// The spans of 4,400 copies from the ranges of 72,000 slots, as a pass
    /// makes them, from the old per-slot hash rebuild vs incremental `HeapRanges`.
    /// Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn spans_of_a_pass() {
        use std::collections::HashMap;
        use std::sync::Arc;
        let copies: Vec<Arc<u64>> = (0..4400).map(Arc::new).collect();
        let (mut table, mut slots) = (HashMap::new(), HeapRanges::default());
        for slot in 0..72_000u32 {
            let copy = &copies[slot as usize * 7919 % 4400];
            let key = Arc::as_ptr(copy) as usize;
            table.insert(slot, (copy.clone(), u64::from(slot), u64::from(slot) + 64));
            slots.set(
                slot,
                Some((
                    key,
                    copy.clone(),
                    u64::from(slot),
                    u64::from(slot) + 64,
                    [1, slot, 1],
                    0,
                )),
            );
        }
        fn join<S: std::hash::BuildHasher + Default>(
            ranges: &mut dyn Iterator<Item = &(Arc<u64>, u64, u64)>,
        ) -> usize {
            let mut spans: HashMap<usize, (Arc<u64>, u64, u64), S> = HashMap::default();
            for (copy, from, to) in ranges {
                let span = spans
                    .entry(Arc::as_ptr(copy) as usize)
                    .or_insert_with(|| (copy.clone(), *from, *to));
                span.1 = span.1.min(*from);
                span.2 = span.2.max(*to);
            }
            spans.len()
        }
        let best = |f: &mut dyn FnMut() -> usize| {
            (0..30)
                .map(|_| {
                    let t = std::time::Instant::now();
                    assert_eq!(f(), 4400);
                    t.elapsed().as_secs_f64() * 1e3
                })
                .fold(f64::MAX, f64::min)
        };
        println!(
            "{:.2} ms from the table, {:.2} ms incremental by copy",
            best(&mut || join::<std::hash::RandomState>(&mut table.values())),
            best(&mut || slots.copy_spans().count()),
        );
    }
}

mod thread_tests {
    use super::super::direct::dispatch_groups;

    #[test]
    fn threads_are_counted_in_the_programs_groups() {
        assert_eq!(dispatch_groups([8847, 1, 1], [64, 1, 1]), [139, 1, 1]);
        assert_eq!(dispatch_groups([74, 1, 1], [64, 1, 1]), [2, 1, 1]);
        assert_eq!(dispatch_groups([32, 1, 1], [32, 1, 1]), [1, 1, 1]);
        assert_eq!(dispatch_groups([0, 12, 9], [64, 4, 4]), [0, 3, 3]);
    }
}

mod staging_budget_tests {
    use super::super::heap_textures::staging_budget_per_frame;

    #[test]
    fn staging_budget_scales_with_vram_budget() {
        const GIB: u64 = 1 << 30;
        const MIB: u64 = 1 << 20;
        assert_eq!(staging_budget_per_frame(None), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(4 * GIB)), 96 * MIB);
        assert_eq!(staging_budget_per_frame(Some(6 * GIB)), 96 * MIB);
        assert_eq!(staging_budget_per_frame(Some(8 * GIB)), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(12 * GIB)), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(13 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(14 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(16 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(20 * GIB)), 384 * MIB);
        assert_eq!(staging_budget_per_frame(Some(24 * GIB)), 384 * MIB);
        assert_eq!(staging_budget_per_frame(Some(32 * GIB)), 384 * MIB);
    }
}

mod record_hash_tests {
    use super::super::plan::{Few, RecordHash};

    #[test]
    fn a_few_things_are_kept_in_order_however_many() {
        for n in 0..10u32 {
            let mut few: Few<u32, 4> = Few::default();
            (0..n).for_each(|i| few.push(n - i));
            assert_eq!(few.to_vec(), (0..n).map(|i| n - i).collect::<Vec<_>>());
            few.sort_unstable();
            assert_eq!(few.to_vec(), (1..=n).collect::<Vec<_>>());
        }
    }

    #[test]
    fn records_that_differ_hash_apart() {
        let of = |records: &[[u32; 3]]| {
            let mut h = RecordHash::default();
            records.iter().for_each(|r| h.add(*r));
            h.0
        };
        let mut seen = std::collections::HashSet::new();
        // One record, one word or two of it different; runs of zeros; order.
        for a in 0..64u32 {
            for b in 0..64u32 {
                for c in [0, 1, 0x8000_0000, u32::MAX] {
                    assert!(seen.insert(of(&[[a, b, c]])));
                    assert!(seen.insert(of(&[[1, 2, 3], [a, b, c]])));
                    assert!(seen.insert(of(&[[a, b, c], [1, 2, 3], [0, 0, 0]])));
                }
            }
        }
        assert!(seen.insert(of(&[])));
        // (One record of zeros is among the above.)
        let zeros: Vec<u64> = (2..100).map(|n| of(&vec![[0; 3]; n])).collect();
        assert!(zeros.iter().all(|h| seen.insert(*h)));
    }
}

mod table_plan_and_reuse_tests {
    use std::time::Duration;

    use oathkeep_shader::native::{ResourceType, RootType, UsedResource};
    use oathkeep_shader::resource::RootIndexedView;

    use super::super::heap_textures::{
        POOL_REUSE_HEADROOM, TILE_REUSE_FLOOR, clamp_pass_bytes_when_full, heap_texture_pass_budget,
    };
    use super::super::plan::TablePlan;
    use super::super::readable::BakingGuard;

    #[test]
    fn table_plan_tracks_indexed_only_tables_and_high_slot_snapshots() {
        let indexed = [RootIndexedView {
            table_slot: 3,
            constant_slot: 0,
            constant_dword: 2,
            shift: 5,
            byte_offset: 64,
        }];
        let plan = TablePlan::new(&[], &indexed);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].slot, 3);
        assert_eq!(plan.groups[0].indexed.len(), 1);

        let selected = plan.groups[0].selected_descriptors(|slot, dword| {
            assert_eq!((slot, dword), (0, 2));
            10 // (64 + (10 << 5)) / 32 == 12
        });
        assert_eq!(selected, vec![12]);

        let start = 0x1000_0000u64;
        let count = 1_245_184u32;
        let index_of = |handle: u64| {
            let off = handle.checked_sub(start)?;
            (off % 32 == 0)
                .then(|| u32::try_from(off / 32).ok())
                .flatten()
                .filter(|&i| i < count)
        };
        let host_count = 868_927u32;
        // High transient table past `host_count` (e.g. slot 900,000) must snapshot
        // even when `entries` is empty and only `RootIndexedView` accesses it.
        let high_table = start + 900_000u64 * 32;
        assert!(plan.groups[0].needs_snapshot_by(host_count, high_table, &selected, index_of));

        // Table starting 4 slots below `host_count` whose `RootIndexedView` reaches
        // `host_count - 4 + 12 >= host_count` must also snapshot.
        let straddling_table = start + u64::from(host_count - 4) * 32;
        assert!(plan.groups[0].needs_snapshot_by(host_count, straddling_table, &selected, index_of));

        // Table well inside `[0..host_count)` without CBVs stays direct.
        let low_table = start + 1000 * 32;
        assert!(!plan.groups[0].needs_snapshot_by(host_count, low_table, &selected, index_of));

        // Mixed static + indexed declarations merge cleanly into one group.
        let used = [UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 8,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 3,
        }];
        let merged = TablePlan::new(&used, &indexed);
        assert_eq!(merged.groups.len(), 1);
        assert_eq!(merged.direct.len(), 1);
        assert_eq!(merged.groups[0].max_dword, 8);
    }

    #[test]
    fn tile_floor_and_pool_hole_budgets_do_not_starve_zero_vram_reuse() {
        // 14 MiB of free 64 KiB tiles (224 tiles) must pass the reuse floor.
        assert!((14u64 << 20) >= TILE_REUSE_FLOOR);
        assert_eq!(TILE_REUSE_FLOOR, 1 << 20);

        // When VRAM is at its ceiling (`!room`) but `pool::texture_room()` has
        // >= 64 MiB of committed holes (e.g. 1,111 MiB), zero-VRAM-growth reuse
        // keeps its full 192 MiB per-frame staging budget instead of dropping to 48 MiB.
        let base = 192u64 << 20;
        let span = Duration::from_millis(250);
        let (budget_with_holes, out_span) =
            heap_texture_pass_budget(base, span, 2048, true, false, true, false, false, 1111 << 20);
        assert_eq!(budget_with_holes, base);
        assert_eq!(out_span, span);
        assert_eq!(
            clamp_pass_bytes_when_full(128 << 20, false, false, false, POOL_REUSE_HEADROOM),
            128 << 20
        );

        // When committed pool holes are nearly exhausted (< 64 MiB), `!room`
        // still clamps non-bake passes to 48 MiB.
        let (budget_scarce, _) =
            heap_texture_pass_budget(base, span, 2048, true, false, true, false, false, 16 << 20);
        assert_eq!(budget_scarce, 48 << 20);
        assert_eq!(
            clamp_pass_bytes_when_full(128 << 20, false, false, false, 16 << 20),
            48 << 20
        );
    }

    #[test]
    fn baking_guard_restores_previous_thread_local_state() {
        crate::texture::BAKING.set(false);
        {
            let _outer = BakingGuard::enter();
            assert!(crate::texture::BAKING.get());
            {
                let _inner = BakingGuard::enter();
                assert!(crate::texture::BAKING.get());
            }
            assert!(crate::texture::BAKING.get());
        }
        assert!(!crate::texture::BAKING.get());
    }
}
