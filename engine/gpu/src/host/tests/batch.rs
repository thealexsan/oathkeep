use super::super::*;

#[cfg(test)]
mod wide_refresh_tests {
    use super::wide_refresh_enabled;

    #[test]
    fn parallel_refresh_is_default_on_multicore_and_can_be_overridden() {
        assert!(wide_refresh_enabled(None, Some(4)));
        assert!(wide_refresh_enabled(None, Some(16)));
        assert!(!wide_refresh_enabled(None, Some(2)));
        assert!(!wide_refresh_enabled(None, None));
        assert!(wide_refresh_enabled(Some("1"), Some(1)));
        assert!(!wide_refresh_enabled(Some("0"), Some(16)));
        assert!(wide_refresh_enabled(Some("auto"), Some(4)));
    }
}

/// A submission's refreshes not done yet, merged per buffer copy into the
/// range that covers them: done before a step that writes title memory or
/// gives a range to shaders, and before the lists run.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
#[cfg(test)]
fn front_spans(steps: impl Iterator<Item = Touch>) -> Vec<(usize, usize, usize)> {
    let mut at: ByCopy<usize> = ByCopy::default();
    // (Copy, from, to), in the order they come, and the part that gave it to
    // shaders last.
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    let mut given: Vec<u32> = Vec::new();
    let (mut part, mut last) = (1, usize::MAX);
    for step in steps {
        match step {
            Touch::Refresh(copy, from, to) => {
                // (Most come several times on end.)
                let i = if spans.get(last).is_some_and(|s| s.0 == copy) {
                    last
                } else {
                    *at.entry(copy).or_insert_with(|| {
                        spans.push((copy, usize::MAX, 0));
                        given.push(0);
                        spans.len() - 1
                    })
                };
                last = i;
                if given[i] != part {
                    let span = &mut spans[i];
                    (span.1, span.2) = (span.1.min(from), span.2.max(to));
                }
            }
            Touch::Write(copy) => {
                let i = *at.entry(copy).or_insert_with(|| {
                    spans.push((copy, usize::MAX, 0));
                    given.push(0);
                    spans.len() - 1
                });
                given[i] = part;
            }
            Touch::Part => part += 1,
            Touch::Title | Touch::Nothing => {}
        }
    }
    spans.retain(|s| s.1 < s.2);
    spans
}

/// What a step is to the copies' refreshes, for [`written_spans`].
#[cfg_attr(not(test), allow(dead_code))]
#[cfg(test)]
mod written_spans_tests {
    use super::{Touch, front_spans, split, written_spans};

    #[test]
    fn everything_is_refreshed_in_front_but_what_a_part_gave_its_shaders() {
        use Touch::{Nothing, Part, Refresh, Title, Write};
        let found = front_spans(
            [
                Refresh(1, 10, 20),
                Refresh(1, 12, 25),
                Refresh(2, 0, 5),
                Nothing,
                Write(1),
                // (Around what the part's own shaders write.)
                Refresh(1, 0, 100),
                Write(3),
                Refresh(3, 1, 2),
                Title,
                Refresh(4, 7, 9),
                Part,
                // The next part's bytes under the first one's writes.
                Refresh(1, 30, 40),
                Write(4),
                Refresh(4, 0, 9),
            ]
            .into_iter(),
        );
        assert_eq!(found, [(1, 10, 40), (2, 0, 5), (4, 7, 9)]);
    }

    #[test]
    fn a_submission_is_cut_between_its_lists() {
        // Lists c and e are in neither half's order: they go with the first.
        let Ok([first, second]) = split(vec!['a', 'b', 'c', 'd', 'e'], vec![1, 3, 0], 1) else {
            panic!("no list is named twice");
        };
        let run = |(lists, order): &(Vec<char>, Vec<usize>)| order.iter().map(|&i| lists[i]).collect::<String>();
        assert_eq!((run(&first), run(&second)), ("b".into(), "da".into()));
        assert_eq!(first.0.len() + second.0.len(), 5);
        assert!(split(vec!['a', 'b'], vec![0, 1, 0], 2).is_err());
        assert!(split(vec!['a', 'b'], vec![0, 1], 0).is_err());
    }

    #[test]
    fn what_is_written_is_refreshed_ahead_up_to_its_write_and_to_title_writes() {
        use Touch::{Nothing, Refresh, Title, Write};
        let mut found = written_spans(
            [
                Refresh(1, 10, 20),
                Refresh(2, 0, 5),
                Nothing,
                Refresh(1, 30, 40),
                Write(1),
                Refresh(1, 0, 100),
                Write(3),
                Refresh(3, 1, 2),
                Refresh(4, 7, 9),
                Write(4),
                Title,
                Write(2),
            ]
            .into_iter(),
        );
        found.sort_unstable();
        assert_eq!(found, [(1, 10, 40), (4, 7, 9)]);
    }

    #[test]
    fn sweep_throttles_when_unpressured_and_stays_eager_under_pressure() {
        use super::{should_settle_for_sweep, should_sweep_idle};
        // Under VRAM pressure: idle sweep at 1s, settle at >1.5s.
        assert!(!should_sweep_idle(true, 999));
        assert!(should_sweep_idle(true, 1000));
        assert!(!should_settle_for_sweep(true, 1500));
        assert!(should_settle_for_sweep(true, 1501));

        // Without VRAM pressure: idle sweep at 3s, settle at >5s.
        assert!(!should_sweep_idle(false, 2999));
        assert!(should_sweep_idle(false, 3000));
        assert!(!should_settle_for_sweep(false, 5000));
        assert!(should_settle_for_sweep(false, 5001));
    }

    #[test]
    fn invalidate_written_preserves_non_overlapping_ahead_entries() {
        let mut batch = super::Batch::default();
        batch.ahead.insert(1, (0x1000, 0x2000));
        batch.ahead.insert(2, (0x2000, 0x3000));
        batch.ahead.insert(3, (0x4000, 0x5000));
        batch.textures.insert(99);

        batch.invalidate_written(0x1800, 0x1000, false);
        assert!(!batch.ahead.contains_key(&1));
        assert!(!batch.ahead.contains_key(&2));
        assert_eq!(batch.ahead.get(&3), Some(&(0x4000, 0x5000)));
        assert!(batch.textures.is_empty());

        batch.invalidate_written(0x4000, 0x100, true);
        assert!(batch.ahead.is_empty());
    }
}

/// Whether the fills of a submission's upload memory are done together on
/// several threads (`OATHKEEP_WIDE_FILLS=0`: one after the other, where the
/// lists name them).
///
/// They are 7,000 to 12,000 a frame, some 500 bytes each, 5 ms of the
/// submitting thread. Each reads title memory and writes memory of its own
/// that the card alone reads, when the lists run; they are done before the
/// next step that writes title memory or runs code of ours, and before the
/// submission goes on from its steps.
#[cfg(test)]
mod trail {
    /// The trail keeps the newest things, and the report made when the
    /// host device is removed says them and the pipeline library's state
    /// (no card is asked for any of it).
    #[test]
    fn the_removal_report_says_the_newest_things_done() {
        for n in 0..super::TRAIL_MOST + 5 {
            super::trail(|| format!("thing {n} done"));
        }
        let text = super::removal_context();
        assert!(text.contains("the submitting thread was"));
        assert!(text.contains("pipeline library: no call running"));
        assert!(text.contains(&format!("thing {} done", super::TRAIL_MOST + 4)));
        assert!(!text.contains("thing 4 done"), "the oldest entries are dropped");
    }
}

#[cfg(test)]
mod spare {
    use super::Spare;

    /// Kept things come back by size, the last given first, and nothing is
    /// kept past the limit.
    #[test]
    fn kept_things_come_back_by_size_within_the_limit() {
        let mut spare: Spare<u32> = Spare::new();
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.give(1, 4, 10), None);
        assert_eq!(spare.give(2, 1, 10), None);
        assert_eq!(spare.give(3, 4, 10), None);
        assert_eq!(spare.bytes, 9);
        // Past the limit: handed back, nothing changes.
        assert_eq!(spare.give(4, 4, 10), Some(4));
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.take(2), None, "no thing of that size");
        assert_eq!(spare.take(4), Some(3));
        assert_eq!(spare.take(4), Some(1));
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.take(1), Some(2));
        assert_eq!(spare.bytes, 0);
        assert!(spare.kept.is_empty());
    }
}

#[cfg(test)]
mod batch {   // Batch::texture_due lives here; `mod tests` is already taken at the top of this file
    use super::Batch;
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p oathkeep-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }

    #[test]
    fn vram_idle_ms_scales_with_pressure() {
        let vram_idle_ms = |memory| super::vram_idle_for(memory, false);
        let keeps = |memory| super::vram_idle_for(memory, true);
        let gib = 1024 * 1024 * 1024;
        assert_eq!(keeps(Some([100, 67, 0, 0])), 600_000);
        assert_eq!(keeps(Some([100, 68, 0, 0])), 30_000);
        assert_eq!(keeps(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(keeps(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(keeps(None), 5_000);
        // Normal (< 75%)
        assert_eq!(vram_idle_ms(Some([10 * gib, 0, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 7 * gib, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([100, 74, 0, 0])), 5_000);
        // Moderate (75%..85%)
        assert_eq!(vram_idle_ms(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(vram_idle_ms(Some([100, 84, 0, 0])), 2_500);
        // High (>= 85%, including over-budget)
        assert_eq!(vram_idle_ms(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 12 * gib, 0, 0])), 1_000);
        // Missing or zero budget falls back to normal
        assert_eq!(vram_idle_ms(None), 5_000);
        assert_eq!(vram_idle_ms(Some([0, 100, 0, 0])), 5_000);
    }

    #[test]
    fn textures_are_kept_while_the_card_has_room() {
        use super::vram_keep;
        let gib = 1 << 30;
        assert_eq!(vram_keep(100 * gib, 87 * gib).0, 600_000);
        // Past it: the longest idle ones, enough to come down to 85 %.
        let (ms, bytes) = vram_keep(100 * gib, 90 * gib);
        assert_eq!((ms, bytes), (5_000, 5 * gib));
        assert_eq!(vram_keep(100 * gib, 120 * gib), (1_500, 35 * gib));
        assert_eq!(vram_keep(0, 7).0, 5_000);
    }

    #[test]
    fn ram_idle_ms_scales_with_pressure() {
        let ram_idle_ms = |avail| super::ram_idle_for(avail, false);
        assert_eq!(super::ram_idle_for(Some(12 << 30), true), 300_000);
        assert_eq!(super::ram_idle_for(Some((12 << 30) - 1), true), 10_000);
        assert_eq!(super::ram_idle_for(None, true), 10_000);
        let gib = 1024 * 1024 * 1024;
        // Normal (>= 4 GiB)
        assert_eq!(ram_idle_ms(Some(8 * gib)), 10_000);
        assert_eq!(ram_idle_ms(Some(4 * gib)), 10_000);
        // Moderate (2 GiB..4 GiB)
        assert_eq!(ram_idle_ms(Some(4 * gib - 1)), 5_000);
        assert_eq!(ram_idle_ms(Some(2 * gib)), 5_000);
        // High (< 2 GiB)
        assert_eq!(ram_idle_ms(Some(2 * gib - 1)), 2_000);
        assert_eq!(ram_idle_ms(Some(0)), 2_000);
        // Missing falls back to normal
        assert_eq!(ram_idle_ms(None), 10_000);
    }
}

