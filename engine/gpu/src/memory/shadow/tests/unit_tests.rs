use super::*;

#[cfg(test)]
mod difference_tests {
    use super::{first_difference, last_difference};

    /// The chunk searches agree with a byte-wise scan across chunk boundaries.
    #[test]
    fn differences_are_found_as_byte_by_byte() {
        let base: Vec<u8> = (0..98u32).map(|i| (i * 7 + 3) as u8).collect();
        for len in [0, 1, 31, 32, 33, 63, 64, 65, 97] {
            let a = &base[1..1 + len];
            let same = a.to_vec();
            assert_eq!(first_difference(&same, a), None);
            assert_eq!(last_difference(&same, a), None);
            for at in 0..len {
                for other in at..len {
                    let mut b = a.to_vec();
                    b[at] ^= 0x80;
                    b[other] ^= 0x01;
                    let first = a.iter().zip(&b).position(|(x, y)| x != y);
                    let last = a.iter().zip(&b).rposition(|(x, y)| x != y);
                    assert_eq!(first_difference(a, &b), first, "{len} {at} {other}");
                    assert_eq!(last_difference(a, &b), last, "{len} {at} {other}");
                }
            }
        }
    }

    #[test]
    fn differences_ignore_bytes_past_the_shorter_slice() {
        let a = [0; 65];
        let mut b = [0; 67];
        b[65] = 1;
        assert_eq!(first_difference(&a, &b), None);
        assert_eq!(last_difference(&b, &a), None);
        b[32] = 1;
        b[64] = 1;
        assert_eq!(first_difference(&b, &a), Some(32));
        assert_eq!(last_difference(&a, &b), Some(64));
    }
}

#[cfg(test)]
mod wide_tests {
    use super::{Job, PIECE, WIDE_JOB, copy_jobs, find_pieces, wide};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Every job runs once, however many there are, and `wide` returns
    /// only when all have.
    #[test]
    fn all_jobs_run_before_wide_returns() {
        for n in [0usize, 1, 2, 7, 300] {
            let ran = Arc::new(AtomicUsize::new(0));
            let jobs: Vec<Job> = (0..n)
                .map(|i| {
                    let ran = ran.clone();
                    Box::new(move || {
                        ran.fetch_add(i + 1, Ordering::Relaxed);
                    }) as Job
                })
                .collect();
            wide(jobs);
            assert_eq!(ran.load(Ordering::Relaxed), n * (n + 1) / 2, "{n} jobs");
        }
    }

    /// Copies cut into jobs copy every byte once: long ones, short ones
    /// and empty ones.
    #[test]
    fn copies_cut_into_jobs_copy_the_same_bytes() {
        let len = 3 * WIDE_JOB + 12345;
        let src: Vec<u8> = (0..len).map(|i| (i * 31 + i / 977) as u8).collect();
        let mut dst = vec![0u8; len];
        let (s, d) = (src.as_ptr() as usize, dst.as_mut_ptr() as usize);
        // Ranges of the buffer, each to the same offsets of the other.
        let cuts = [0, 1, 1, 4096, 70_000, WIDE_JOB + 3, 2 * WIDE_JOB + 3, 2 * WIDE_JOB + 9, len];
        let copies: Vec<(usize, usize, usize)> = cuts.windows(2).map(|w| (s + w[0], d + w[0], w[1] - w[0])).collect();
        let jobs = copy_jobs(copies);
        assert!(jobs.len() >= 3);
        wide(jobs);
        assert!(src == dst);
    }

    /// The pieces found on several threads are the ones a walk piece by
    /// piece finds, for copies that start anywhere in a 64 KiB piece of
    /// title memory and ranges that start and end anywhere.
    #[test]
    fn pieces_found_wide_are_those_found_one_by_one() {
        let len = 5 * WIDE_JOB / 2 + 777;
        let old: Vec<u8> = (0..len).map(|i| (i * 7 + i / 513) as u8).collect();
        let mut now = old.clone();
        // Changes: single bytes at piece edges, a run across two pieces,
        // the last byte, and nothing in between.
        for at in [0usize, 1, PIECE - 1, PIECE, 3 * PIECE + 5, WIDE_JOB - 1, WIDE_JOB, WIDE_JOB + PIECE / 2, 2 * WIDE_JOB + 9, len - 1] {
            now[at] ^= 0x5a;
        }
        for byte in &mut now[9 * PIECE - 40..9 * PIECE + 40] {
            *byte ^= 0x01;
        }
        let (data, before) = (now.as_ptr() as usize, old.as_ptr() as usize);
        let mut scans = Vec::new();
        let mut wanted = Vec::new();
        for start in [0usize, 4096, PIECE - 16, 0xd_b790_df00] {
            for (from, to) in [(0, len), (0, 1), (1, 2), (PIECE - 1, PIECE + 1), (3 * PIECE, 3 * PIECE), (12345, len - 1), (WIDE_JOB - 3, 2 * WIDE_JOB + 10)] {
                scans.push((data, before, start, from, to));
                let mut pieces = Vec::new();
                let mut at = from;
                while at < to {
                    let n = (PIECE - (start + at) % PIECE).min(to - at);
                    if now[at..at + n] != old[at..at + n] {
                        pieces.push((at, n));
                    }
                    at += n;
                }
                wanted.push(pieces);
            }
        }
        // A scan of nothing (a range whose snapshot is gone).
        scans.push((0, 0, 0, 0, 0));
        wanted.push(Vec::new());
        assert_eq!(find_pieces(scans), wanted);
    }
}

#[cfg(test)]
mod piece_tests {
    use super::{PIECE, piece_at, pieces_of};

    /// The pieces cover a copy without gaps, each cut where the compare
    /// cuts (at multiples of 64 KiB of title memory).
    #[test]
    fn pieces_cover_the_copy_as_the_compare_cuts_it() {
        for start in [0usize, 1, 4096, PIECE - 1, PIECE, PIECE + 77, 5 * PIECE + 12345] {
            for len in [1usize, 100, PIECE - 1, PIECE, PIECE + 1, 3 * PIECE + 500] {
                let count = pieces_of(start, 0, len).end;
                let mut at = 0;
                for piece in 0..count {
                    let (lo, hi) = piece_at(start, len, piece);
                    assert_eq!(lo, at, "{start} {len} {piece}");
                    // As `Shadow::differing` cuts: to the next multiple.
                    let n = (PIECE - (start + at) % PIECE).min(len - at);
                    assert_eq!(hi, at + n, "{start} {len} {piece}");
                    assert_eq!(pieces_of(start, lo, hi), piece..piece + 1);
                    assert_eq!(pieces_of(start, hi - 1, hi), piece..piece + 1);
                    at = hi;
                }
                assert_eq!(at, len, "{start} {len}");
                assert_eq!(pieces_of(start, 0, len), 0..count);
            }
        }
    }
}

#[cfg(test)]
mod landing_tests {
    use super::{KERNEL_PAGE, Shadow};

    /// A page that only a landing wrote is not sent up again; one the title
    /// wrote too is, and so is every page no landing was noted in.
    #[test]
    fn what_a_landing_wrote_is_not_sent_up_again() {
        const PAGES: usize = 6;
        let room = vec![7u8; (PAGES + 1) * KERNEL_PAGE];
        let title = room.as_ptr() as usize;
        let title = title.next_multiple_of(KERNEL_PAGE);
        let mut side = vec![7u8; PAGES * KERNEL_PAGE];
        let copy = Shadow::unbacked(title as u64, side.as_mut_ptr(), side.len() as u64, true);
        let write = |page: usize| {
            // SAFETY: in `room`.
            unsafe { ((title + page * KERNEL_PAGE + 9) as *mut u8).write_volatile(8) }
        };
        let page = |from: usize, to: usize| (title + from * KERNEL_PAGE, title + to * KERNEL_PAGE);
        copy.note_landing(KERNEL_PAGE, 4 * KERNEL_PAGE);
        // Page 2: the title's too.
        write(2);
        let (from, to) = page(0, 6);
        copy.take_landed(from, to);
        let mut dirty = copy.take_dirty();
        dirty.sort_unstable();
        let part = |a: usize, b: usize| (a * KERNEL_PAGE, b * KERNEL_PAGE);
        assert_eq!(super::joined(dirty), [part(0, 1), part(2, 3), part(4, 6)]);
        assert_eq!(side[2 * KERNEL_PAGE + 9], 8);
        // Looked at: from here on as any other.
        let (from, to) = page(1, 2);
        copy.take_landed(from, to);
        assert_eq!(copy.take_dirty(), [part(1, 2)]);
        // In part: looked at again when the rest is.
        copy.note_landing(3 * KERNEL_PAGE, 3 * KERNEL_PAGE + 1);
        let (from, to) = page(3, 4);
        copy.take_landed(from, from + 100);
        copy.take_landed(from, to);
        assert_eq!(copy.take_dirty(), []);
        drop(room);
    }
}

#[cfg(test)]
mod parts_tests {
    use super::{PAGE, by_parts};

    #[test]
    fn runs_are_cut_to_the_parts_written() {
        const P: usize = PAGE / 16;
        let at = |page: usize, part: usize| page * PAGE + part * P;
        // Pages 10 and 11 again (from the middle of 10's part 1 on), 12
        // never taken, 14 again.
        let runs = vec![
            (at(10, 1) + 100, at(12, 0), true),
            (at(12, 0), at(13, 0), false),
            (at(14, 0), at(14, 9) + 7, true),
        ];
        let pages = [(10, 5), (11, 5), (14, 6)];
        assert_eq!(by_parts(runs.clone(), &pages, &[u16::MAX; 3]), runs);
        assert_eq!(
            by_parts(runs, &pages, &[0b1000_0000_0000_0011, 0b11, 0b10_0000_0000]),
            [
                (at(10, 1) + 100, at(10, 2), true),
                (at(10, 15), at(11, 2), true),
                (at(12, 0), at(13, 0), false),
                (at(14, 9), at(14, 9) + 7, true),
            ]
        );
    }
}

#[cfg(test)]
mod stale_tests {
    use super::*;

    #[test]
    fn pages_never_taken_are_stale_written_or_not() {
        let base = 0x100;
        let copied = [5, 0, 5, 5];
        let at = |page: usize| page * PAGE;
        // Page 0x101 was never taken; page 0x102 was written after it was
        // taken, page 0x103 before.
        let written = [(0x102, 6), (0x103, 4)];
        assert_eq!(stale_pages_in(base, &copied, at(0x100), at(0x104), &written), [0x101, 0x102]);
        // Part of a page counts as the page; pages outside the copy do not.
        assert_eq!(stale_pages_in(base, &copied, at(0x101) + 8, at(0x101) + 16, &[]), [0x101]);
        assert!(stale_pages_in(base, &copied, at(0x104), at(0x106), &[(0x104, 9)]).is_empty());
    }

    #[test]
    fn copied_state_caches_many_subranges_and_largest_containment() {
        let mut state = CopiedState::new(64);
        // Record 100 distinct sub-ranges (far more than the old 2-slot cache).
        for i in 0..100usize {
            let from = 0x1_0000 + i * 0x100;
            let to = from + 0x80;
            state.record_range(from, to, (i + 1) as u64);
        }
        for i in 0..100usize {
            let from = 0x1_0000 + i * 0x100;
            let to = from + 0x80;
            assert_eq!(state.ranged(from, to), (i + 1) as u64);
        }
        // A larger range updates containment queries for sub-ranges inside it.
        state.record_range(0x1_0000, 0x2_0000, 500);
        assert_eq!(state.ranged(0x1_0000, 0x1_0080), 500);
        assert_eq!(state.ranged(0x1_5000, 0x1_5100), 500);
        assert_eq!(state.ranged(0x2_0000, 0x2_0100), 0);
    }
}
