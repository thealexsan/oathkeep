use super::had::Had;
use super::piece::{differing_page_bounds, for_pages};
use super::pool::Pool;
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE};

mod landing;

pub(super) struct Random(pub(super) u64);

impl Random {
    pub(super) fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 8) as usize
    }
}

/// View memory as the runtime makes the title's: a view of a section, noted
/// as it notes it. Kept: the journal of what was written (`crate::memory::watch`)
/// knows its addresses, which must not become another test's fresh memory.
pub(super) struct Title {
    pub(super) at: usize,
    pub(super) len: usize,
}

impl Title {
    pub(super) fn new(len: usize) -> Title {
        // SAFETY: a pagefile section of our own and a view of it.
        unsafe {
            let section =
                CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, len as u32, std::ptr::null());
            assert!(!section.is_null());
            let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            assert_ne!(at, 0);
            crate::memory::inplace::title_memory_section(section as usize, len as u64, true);
            crate::title_memory_remapped(at, len);
            crate::memory::inplace::title_memory_view(at, len, section as usize, 0);
            Title { at, len }
        }
    }

    pub(super) fn bytes(&self) -> &[u8] {
        // SAFETY: the view.
        unsafe { std::slice::from_raw_parts(self.at as *const u8, self.len) }
    }
}

/// Every number once, whatever the count and the threads, batch after batch
/// and from several callers.
#[test]
fn the_pool_calls_every_number_once() {
    for threads in [1usize, 2, 4, 7] {
        let pool = Pool::new("victory-land", threads);
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    for round in 0..300usize {
                        let count = round * 7 % 50;
                        let calls: Vec<AtomicUsize> = (0..count).map(|_| AtomicUsize::new(0)).collect();
                        pool.each(count, &|i| {
                            calls[i].fetch_add(1, Ordering::Relaxed);
                        });
                        assert!(calls.iter().all(|c| c.load(Ordering::Relaxed) == 1));
                    }
                });
            }
        });
    }
}

/// A job that panics is told to the caller, and the pool goes on.
#[test]
fn a_panicking_job_reaches_the_caller() {
    let pool = Pool::new("victory-land", 3);
    for bad in 0..8usize {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.each(8, &|i| assert_ne!(i, bad, "(expected)"));
        }));
        assert!(ran.is_err());
        let n = AtomicUsize::new(0);
        pool.each(20, &|i| {
            n.fetch_add(i, Ordering::Relaxed);
        });
        assert_eq!(n.into_inner(), 190);
    }
}

/// The order pieces are landed in holds every byte of every piece, under its
/// copy, and no other; blocks come in order, pieces of several copies in one
/// block stay in the order given, and jobs never cut a block.
#[test]
fn ordered_pieces_hold_the_same_bytes() {
    let mut random = Random(99);
    for _ in 0..400 {
        let count = random.next() % 40;
        let pieces: Vec<Piece> = (0..count)
            .map(|_| {
                let block = random.next() % 12 * BLOCK;
                let from = random.next() % BLOCK / 4 * 4;
                let to = (from + 4 + random.next() % BLOCK / 4 * 4).min(BLOCK);
                (random.next() as u64 % 3, block + from, to - from)
            })
            .collect();
        let out = ordered(&pieces);
        let bytes = |copy: u64, at: usize, all: &[Piece]| all.iter().any(|p| p.0 == copy && p.1 <= at && at < p.1 + p.2);
        let landed: Vec<Piece> = out.iter().map(|&(i, at, len)| (pieces[i].0, at, len)).collect();
        for copy in 0..3 {
            for at in (0..12 * BLOCK).step_by(1024 * 4) {
                assert_eq!(bytes(copy, at, &pieces), bytes(copy, at, &landed), "{pieces:?}");
            }
        }
        assert!(out.iter().all(|&(_, at, len)| len > 0 && at / BLOCK == (at + len - 1) / BLOCK));
        assert!(out.windows(2).all(|w| w[0].1 / BLOCK <= w[1].1 / BLOCK));
        for block in 0..12 {
            let given: Vec<Piece> = pieces.iter().copied().filter(|p| p.1 / BLOCK == block).collect();
            let here: Vec<Piece> = landed.iter().copied().filter(|p| p.1 / BLOCK == block).collect();
            if given.iter().any(|p| p.0 != given[0].0) {
                assert_eq!(here, given);
            } else {
                assert!(here.windows(2).all(|w| w[0].1 + w[0].2 < w[1].1), "apart, in order");
            }
        }
        let cut: Vec<(usize, usize)> = out.iter().map(|&(_, at, len)| (at, len)).collect();
        let jobs = jobs(&cut);
        assert_eq!(jobs.iter().cloned().flatten().collect::<Vec<_>>(), (0..cut.len()).collect::<Vec<_>>());
        for w in jobs.windows(2) {
            assert_ne!(cut[w[0].end - 1].0 / BLOCK, cut[w[1].start].0 / BLOCK);
        }
    }
}

/// A piece is left out only if a fetch behind its own flight, or behind a
/// later one, brought all of it, and is forgotten when that flight is done.
#[test]
fn only_what_a_fetch_behind_the_flight_brought_is_left_out() {
    let mut had = Had::default();
    had.fetched(7, [(1, BLOCK, BLOCK), (2, 0x100, 0x200)].into_iter());
    assert!(had.holds(6, (1, BLOCK, BLOCK)) && had.holds(7, (1, BLOCK + 64, 128)));
    assert!(!had.holds(8, (1, BLOCK, BLOCK)), "the fetch ran ahead of flight 8");
    assert!(!had.holds(7, (3, BLOCK, BLOCK)) && !had.holds(7, (1, 0, 64)));
    assert!(had.holds(7, (2, 0x100, 0x200)) && !had.holds(7, (2, 0x100, 0x204)) && !had.holds(7, (2, 0xfc, 8)));
    had.landing(7);
    assert!(had.holds(7, (1, BLOCK, BLOCK)));
    had.landing(8);
    assert!(had.0.is_empty());

    // Disjoint sub-ranges in the same 64 KiB block are all retained and coalesced when bridged.
    had.fetched(10, [(4, 0x100, 0x200), (4, 0x800, 0x400), (4, 0x2000, 0x100)].into_iter());
    assert!(had.holds(10, (4, 0x100, 0x200)));
    assert!(had.holds(10, (4, 0x800, 0x400)));
    assert!(had.holds(10, (4, 0x2000, 0x100)));
    assert!(!had.holds(10, (4, 0x300, 0x500)));
    had.fetched(10, [(4, 0x300, 0x500)].into_iter());
    assert!(had.holds(10, (4, 0x100, 0xb00)));
}

#[test]
fn nested_pool_calls_execute_inline_without_deadlock() {
    let pool = Pool::new("victory-land", 4);
    let sum = AtomicUsize::new(0);
    pool.each(12, &|i| {
        pool.each(5, &|j| {
            sum.fetch_add(i * 10 + j, Ordering::Relaxed);
        });
    });
    let expected: usize = (0..12).flat_map(|i| (0..5).map(move |j| i * 10 + j)).sum();
    assert_eq!(sum.into_inner(), expected);
}

#[test]
fn differing_page_bounds_match_full_page_scan_across_alignments_and_parts() {
    let mut random = Random(0x5150);
    for title in [0x10_0000usize, 0x10_0010, 0x10_0ff4, 0x10_3abc] {
        let base = title / PAGE;
        let max_len = BLOCK - title % BLOCK;
        let mut held = vec![0x22u8; max_len];
        for (i, b) in held.iter_mut().enumerate() {
            *b = (i * 17 + 3) as u8;
        }
        // Empty / completely identical -> Err(page_slices).
        let parts = [(0, max_len.min(5 * PAGE + 123)), (6 * PAGE, max_len.min(9 * PAGE + 7))];
        let parts: Vec<(usize, usize)> = parts.into_iter().filter(|(a, b)| a < b).collect();
        let mut expected_slices = 0u64;
        for_pages(title, &parts, |_, _| expected_slices += 1);
        // SAFETY: `held` has `max_len` bytes.
        let same = unsafe { differing_page_bounds(title, held.as_ptr(), held.as_ptr(), &parts) };
        assert_eq!(same, Err(expected_slices));

        for _ in 0..80 {
            let mut now = held.clone();
            let edits = 1 + random.next() % 4;
            for _ in 0..edits {
                let p = &parts[random.next() % parts.len()];
                let off = p.0 + random.next() % (p.1 - p.0);
                now[off] ^= 0x81;
            }
            let mut full = [false; BLOCK / PAGE + 1];
            for_pages(title, &parts, |from, n| {
                if now[from..from + n] != held[from..from + n] {
                    full[(title + from) / PAGE - base] = true;
                }
            });
            let exp_first = full.iter().position(|&d| d).unwrap();
            let exp_last = full.iter().rposition(|&d| d).unwrap();
            // SAFETY: `now` and `held` both hold `max_len` bytes.
            let got = unsafe { differing_page_bounds(title, now.as_ptr(), held.as_ptr(), &parts) };
            assert_eq!(got, Ok((exp_first, exp_last)), "title={title:#x}");
        }
    }
}
