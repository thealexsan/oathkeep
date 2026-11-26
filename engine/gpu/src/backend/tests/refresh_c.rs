use super::super::*;
use super::refresh_a::*;
use crate::memory::shadow::Shadow;
use std::sync::Arc;


/// The title's bytes put in under shader writes, for a copy of 174 MB
/// whose shader writes were declared as 40 ranges that overlap, with 12
/// pages written since. Run with `--ignored --nocapture`, and with
/// `OATHKEEP_LIGHT_REFRESH=0` for the way it was.
#[test]
#[ignore]
fn bytes_under_overlapping_ranges() {
    const BIG: usize = 174 << 20;
    let big = title(BIG, true);
    // SAFETY (both): the test's own memory.
    unsafe { std::ptr::write_bytes(big as *mut u8, 1, BIG) };
    let copy = Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true);
    copy.update(big, BIG);
    copy.take_dirty();
    for i in 0..40 {
        copy.writing(big + i * 5920, BIG - i * 5920);
    }
    let mut best = f64::MAX;
    for round in 0..30 {
        for i in 0..12 {
            unsafe { ((big + (i * 211 + round) % 2700 * PAGE + 64) as *mut u8).write_volatile(round as u8) };
        }
        let started = std::time::Instant::now();
        let mut runs = usize::MAX;
        assert!(!copy.merge_under_writes(big, BIG, &mut runs));
        best = best.min(started.elapsed().as_secs_f64() * 1e3);
        copy.take_dirty();
    }
    println!("OATHKEEP_LIGHT_REFRESH {}: best {best:.3} ms", crate::memory::watch::light());
}

/// A range nothing wrote is known to be as it was, at its ends too, and
/// is so no more once something wrote it.
#[test]
fn unchanged_ranges_are_known() {
    for view in [false, true] {
        let region = title(8 * PAGE, view);
        // (A page read for the first time counts as written.)
        // SAFETY: the test's own memory.
        unsafe { std::ptr::write_bytes(region as *mut u8, 1, 8 * PAGE) };
        let data = memory(8 * PAGE);
        let copy = Shadow::unbacked(region as u64, data, (8 * PAGE) as u64, false);
        // Ends in the middle of pages.
        let (at, len) = (region + 500, 5 * PAGE);
        let look = |expected: bool, what: &str| {
            // The submission's one asking, which takes two ranges. (Again
            // if a submission of another test ended it.)
            let mut known = false;
            for _ in 0..100 {
                crate::memory::watch::ask_once(&[(at, at + len), (region + 7 * PAGE, region + 8 * PAGE)]);
                known = copy.nothing_new(at, len);
                if known || !expected {
                    break;
                }
            }
            assert_eq!(known, expected, "view {view}: {what}");
            let copied = copy.update(at, len);
            crate::memory::watch::ask_once_end();
            copied
        };
        assert_eq!(look(false, "never taken"), len);
        assert_eq!(look(true, "taken"), 0);
        assert_eq!(look(true, "taken"), 0);
        // SAFETY: the test's own memory.
        unsafe { ((region + 600) as *mut u8).write_volatile(1) };
        // (The kernel names the 4 KiB, the guards only the block.)
        assert_eq!(look(false, "written at its end"), if view { PAGE } else { 4096 } - 500);
        assert_eq!(look(true, "taken again"), 0);
        // Outside the range, on its last page.
        // SAFETY: as above.
        unsafe { ((region + 5 * PAGE + 600) as *mut u8).write_volatile(1) };
        assert_eq!(look(false, "written on its last page"), 500);
        // A longer range was never taken whole.
        assert!(!copy.nothing_new(region, 6 * PAGE));
    }
}

/// Of a copy in the card's memory only the dwords title memory changed go
/// up, and none beside them, not even between two in 64 bytes; a page
/// never taken goes up whole.
#[test]
fn only_what_differs_goes_up() {
    let region = title(4 * PAGE, true);
    let data = memory(4 * PAGE);
    let copy = Shadow::unbacked(region as u64, data, (4 * PAGE) as u64, true);
    copy.update(region, 4 * PAGE);
    assert_eq!(copy.take_dirty(), [(0, 4 * PAGE)], "all of it, though zeros are no change");
    // What the relay does: shader writes into title memory, and into the
    // CPU's side.
    crate::memory::watch::landing(region + PAGE + 640, 64);
    // SAFETY: the test's own memory.
    unsafe {
        std::ptr::write_bytes((region + PAGE + 640) as *mut u8, 7, 64);
        std::ptr::write_bytes(data.add(PAGE + 640), 7, 64);
    }
    copy.update(region, 4 * PAGE);
    let again = copy.take_dirty();
    // SAFETY: as above.
    unsafe {
        ((region + 2 * PAGE + 129) as *mut u8).write_volatile(9);
        ((region + 2 * PAGE + 136) as *mut u32).write_volatile(9);
        ((region + 2 * PAGE + 140) as *mut u32).write_volatile(9);
        // The value that was there.
        ((region + 2 * PAGE + 132) as *mut u32).write_volatile(0);
    }
    copy.update(region, 4 * PAGE);
    let written = copy.take_dirty();
    if std::env::var_os("OATHKEEP_SEND_CHANGED").is_some_and(|v| v != "0") {
        assert_eq!(again, []);
        assert_eq!(written, [(2 * PAGE + 128, 2 * PAGE + 132), (2 * PAGE + 136, 2 * PAGE + 144)]);
    } else {
        assert_eq!(again, [(PAGE, 2 * PAGE)]);
        assert_eq!(written, [(2 * PAGE, 3 * PAGE)]);
    }
    // SAFETY: as above.
    assert_eq!(unsafe { data.add(2 * PAGE + 129).read() }, 9);
}

/// The refreshes of a frame as run record-1 had them: 30 submissions, the
/// first with a copy for each of 4,400 ranges, the others with 60 and a
/// big copy that 55 landed pieces and 9 title writes opened, while a
/// thread lands what shaders wrote. Run with `--ignored --nocapture`,
/// and with `OATHKEEP_LIGHT_REFRESH=0` for the way it was.
#[test]
#[ignore]
fn frame_of_refreshes() {
    let mut random = Random(0x9876_5432_1234_5671);
    const EACH: usize = 224 << 10;
    const BIG: usize = 174 << 20;
    let watched = title(3000 * EACH, false);
    // `OATHKEEP_BENCH_HOLES=0`: views without holes.
    let holes = std::env::var_os("OATHKEEP_BENCH_HOLES").is_none_or(|v| v != "0");
    let views = if holes { with_holes(1400 * EACH / PAGE) } else { title(1400 * EACH, true) };
    let there = |at: usize| !holes || (at - views) / PAGE % 4 != 3;
    let big = title(BIG, true);
    // SAFETY: the test's own memory.
    unsafe {
        std::ptr::write_bytes(watched as *mut u8, 1, 3000 * EACH);
        for at in (views..views + 1400 * EACH).step_by(PAGE).filter(|&at| there(at)) {
            std::ptr::write_bytes(at as *mut u8, 1, PAGE);
        }
        std::ptr::write_bytes(big as *mut u8, 1, BIG);
    }
    let mut small = Vec::new();
    for i in 0..4400 {
        let (start, at) = if i < 3000 { (watched, i * EACH) } else { (views, (i - 3000) * EACH) };
        let data = memory(EACH);
        // The range views read, not all of the copy.
        small.push((Shadow::unbacked((start + at) as u64, data, EACH as u64, false), start + at + 4000, EACH - 9000));
    }
    let heap = (Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true), big, BIG);
    // The same memory, as another copy: what is landed is landed through
    // it, and the refreshes are of the first.
    let lands = Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true);
    let (mut best, mut asking) = ([f64::MAX; 2], [0f64; 2]);
    let (mut sum, mut landing) = (0.0, 0.0);
    const FRAMES: usize = 12;
    for frame in 0..FRAMES {
        let mut took = [0f64; 2];
        for i in 0..30 {
            // The title's writes, and the relay's beside the submission.
            for _ in 0..9 {
                let at = views + random.below(1400 * EACH / 8) * 8;
                if there(at) {
                    // SAFETY: the test's own memory.
                    unsafe { (at as *mut u64).write_volatile(frame as u64) };
                }
            }
            for _ in 0..60 {
                let at = watched + random.below(3000 * EACH / 8) * 8;
                // SAFETY: the test's own memory.
                unsafe { (at as *mut u64).write_volatile(frame as u64) };
            }
            let first = random.below(BIG / PAGE - 60);
            let lands = lands.clone();
            let land = std::thread::spawn(move || {
                let now = vec![frame as u8 ^ i as u8 ^ 0x55; PAGE];
                let started = std::time::Instant::now();
                for pieces in (first..first + 55).collect::<Vec<_>>().chunks(4) {
                    let pieces: Vec<_> =
                        pieces.iter().map(|p| (p * PAGE + 8, (p + 1) * PAGE - 8, now.as_ptr() as usize)).collect();
                    let mut notes = crate::memory::writeback_pool::Notes::default();
                    // SAFETY: the bytes of `now`; nothing else lands here.
                    unsafe { lands.land_card(&pieces, 0, &mut notes) };
                    notes.flush();
                }
                started.elapsed().as_secs_f64() * 1e3
            });
            let ranges: Vec<(Arc<Shadow>, usize, usize)> = if i == 0 {
                small.iter().cloned().chain([heap.clone()]).collect()
            } else {
                (0..60).map(|_| small[random.below(4400)].clone()).chain([heap.clone()]).collect()
            };
            let started = std::time::Instant::now();
            submission(&ranges, 3);
            took[usize::from(i != 0)] += started.elapsed().as_secs_f64() * 1e3;
            let put = land.join().unwrap();
            if frame >= 2 {
                landing += put;
            }
            heap.0.take_dirty();
        }
        if frame >= 2 {
            sum += took[0] + took[1];
            for k in 0..2 {
                best[k] = best[k].min(took[k]);
                asking[k] += took[k];
            }
        }
    }
    println!(
        "OATHKEEP_LIGHT_REFRESH {}: a frame's refreshes {:.2} ms in the mean; the submission of all copies {:.2} (best {:.2}), the 29 \
         others {:.2} (best {:.2}); the landing thread's puts {:.2}",
        crate::memory::watch::light(),
        sum / (FRAMES - 2) as f64,
        asking[0] / (FRAMES - 2) as f64,
        best[0],
        asking[1] / (FRAMES - 2) as f64,
        best[1],
        landing / (FRAMES - 2) as f64,
    );
    #[cfg(feature = "diagnostics")]
    println!("per frame, as `crate::memory::watch::asks`: {:?}", crate::memory::watch::asks().map(|n| n / FRAMES as u64));
}

