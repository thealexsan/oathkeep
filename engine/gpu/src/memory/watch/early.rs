use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};

use super::PAGE;
use super::ask::writes;
use super::guards::published;
use super::journal::ASKED;

/// Since the last frame line: the kernel askings that took a millisecond
/// or more, their microseconds, and the longest asking's. An asking walks
/// page tables, some ten microseconds for most ranges; in run 75 the 2,644
/// askings of a frame took 219 ms in the lines where the pass over the
/// heap's views ran long, 25-50 ms in the others. These numbers say
/// whether a few askings wait for something or all of them are slow.
#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
pub(super) fn slow(us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if us >= 1000 {
        SLOW[0].fetch_add(1, Relaxed);
        SLOW[1].fetch_add(us, Relaxed);
    }
    SLOW[2].fetch_max(us, Relaxed);
}

/// [`SLOW`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn slow_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// Whether a thread asks, between a submission's askings, about the memory
/// they found written (`OATHKEEP_ASK_EARLY=<ms>` between its rounds; off
/// unless given: in run cpu-2 the submissions' own askings were none the
/// cheaper for its 650 a frame, 12 to 18 ms where 9 were).
///
/// An asking's cost is the pages it finds written and resets, some 1.4
/// microseconds each, and the title writes 7,000 a frame: the first
/// submission that refreshes its 174 MB heap stood 8 to 10 ms in one call
/// (session play-1125-0203). Asked a little at a time while the title
/// records, the same pages are in the journal when the submission comes, and
/// its own asking finds those written since.
///
/// Nothing is seen later than before: a page goes into the journal as found
/// written by an asking of its own number, as from any thread that asks, and
/// the submission still asks for itself. A page the title writes again after
/// such a round faults once more.
fn early() -> Option<std::time::Duration> {
    static EVERY: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        let ms = match std::env::var("OATHKEEP_ASK_EARLY") {
            Ok(v) => v.trim().parse::<f64>().unwrap_or(2.0),
            Err(_) => 0.0,
        };
        std::time::Duration::try_from_secs_f64(ms / 1000.0).ok().filter(|d| !d.is_zero())
    })
}

/// The pieces of memory (by [`EARLY_PIECE`] pages) that submissions' askings
/// found written lately: how many pages each, halved every round of 64.
static HOT: Mutex<BTreeMap<usize, u32>> = Mutex::new(BTreeMap::new());
/// The pieces asked in a round, at most.
const EARLY_MOST: usize = 24;
/// The pages of a piece: 2 MiB.
const EARLY_PIECE: usize = (2 << 20) / PAGE;
/// Told when a submission asked.
static HOT_MOVED: Condvar = Condvar::new();

/// Since the last frame line: pieces asked early, the microseconds of it.
static EARLY: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// [`EARLY`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn early_asks() -> [u64; 2] {
    [0, 1].map(|i| EARLY[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// A submission's asking found `written` (pages).
pub(super) fn note_hot(written: &[(usize, u16)]) {
    let Some(every) = early() else {
        return;
    };
    let mut hot = HOT.lock().unwrap_or_else(|e| e.into_inner());
    for (page, _) in written {
        let n = hot.entry(page / EARLY_PIECE).or_default();
        *n = (*n + 1).min(1 << 10);
    }
    HOT_MOVED.notify_one();
    drop(hot);
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("victory-ask-early".into()).spawn(move || ask_early(every));
    });
}

fn ask_early(every: std::time::Duration) {
    use std::sync::atomic::Ordering::Relaxed;
    // So many rounds without a submission's asking: wait for one.
    const IDLE: u32 = 100;
    let mut round = 0u32;
    let mut idle = 0;
    let mut last = 0u64;
    loop {
        std::thread::sleep(every);
        let pieces: Vec<usize> = {
            let mut hot = HOT.lock().unwrap_or_else(|e| e.into_inner());
            let now = ASKED.load(Relaxed);
            idle = if now == last { idle + 1 } else { 0 };
            while idle > IDLE {
                hot = HOT_MOVED.wait(hot).unwrap_or_else(|e| e.into_inner());
                idle = 0;
            }
            round += 1;
            if round % 64 == 0 {
                hot.retain(|_, n| {
                    *n /= 2;
                    *n > 0
                });
            }
            // The most written: a piece with a few pages is as dear to ask as
            // one with many.
            let mut most: Vec<(u32, usize)> = hot.iter().filter(|h| *h.1 >= 16).map(|(&p, &n)| (n, p)).collect();
            most.sort_unstable_by(|a, b| b.cmp(a));
            most.into_iter().take(EARLY_MOST).map(|(_, p)| p).collect()
        };
        let began = std::time::Instant::now();
        let mut asked = 0;
        for piece in pieces {
            // (A submission's asking stands: what it holds is answered from
            // the journal, and costs nothing here.)
            if published(piece * EARLY_PIECE * PAGE, (piece + 1) * EARLY_PIECE * PAGE) {
                continue;
            }
            asked += 1;
            let _ = writes(piece * EARLY_PIECE * PAGE, (piece + 1) * EARLY_PIECE * PAGE, false);
        }
        EARLY[0].fetch_add(asked, Relaxed);
        EARLY[1].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
        last = ASKED.load(Relaxed);
    }
}
