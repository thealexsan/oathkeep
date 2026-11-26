//! What shaders wrote goes into title memory on a few threads
//! (`OATHKEEP_LAND`, on by default; `=0` merges piece by piece on the thread
//! that lands flights, as before).
//!
//! The relay's thread (`crate::backend::relay`) was 58-60 ms of a 104 ms frame
//! landing (run record-1): some 1,500 pieces of 64 KiB, 98 MB, fetched from
//! copies in the card's memory, and each of them searched from both ends,
//! cleared room for, read from title memory, copied, merged and compared
//! again while it was put, one after the other; a lock and a protection
//! call to open the guard of each, and the kernel's fault on the first
//! write to each 4 KiB page of memory just made writable again.
//!
//! Here a piece is merged in one pass ([`crate::memory::safecopy::merge`]): what
//! the card holds is compared with what it held 32 bytes at a time, title
//! memory is read only where they differ, just before the dwords that
//! change are stored. The pieces of a landing are cut into jobs for a few
//! threads kept for it ([`pool::Pool`]).
//!
//! What stays in order: pieces in the same 64 KiB of title memory are in
//! one job, in the order they were given (two copies may hold the same
//! title memory); a landing's copies in the card's memory come before its
//! other copies; a landing is done, whole, before the title is told
//! anything and before the next one starts (`Host::relay_land`).
//!
//! With `OATHKEEP_LAND_BESIDE` (on by default) title memory that is a view of
//! the title's physical pages is written through a view of our own of the
//! same pages ([`crate::memory::inplace::beside`]). The guards (`crate::memory::watch`) are
//! read-only title views: they stay as they are, no protection changes, and
//! [`crate::memory::watch::landed_beside`] notes the blocks written afterwards, so
//! that the next refresh takes them as before.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

mod had;
mod piece;
mod pool;

pub(crate) use had::{Had, Piece, jobs, ordered};
pub(crate) use piece::{Notes, piece};
pub(crate) use pool::{each, each_under, each_view, each_wide};

/// The unit pieces are cut in (of title memory), and the guards' blocks.
pub(crate) const BLOCK: usize = 64 << 10;
/// The unit title memory is there or not in.
const PAGE: usize = 4096;
/// About this many bytes to a job.
const JOB: usize = 256 << 10;

/// See the file.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("OATHKEEP_LAND").is_none_or(|v| v != "0") && crate::memory::safecopy::on() && crate::memory::shadow::exact_back()
    })
}

/// How many threads land, the relay's own counted (`OATHKEEP_LAND_THREADS`).
/// Scales to `available_parallelism / 2` clamped to `2..=6` (e.g. 6 threads on
/// a 6c/12t CPU, matching `OATHKEEP_UNDER_THREADS`) so 250–350 piece-merge jobs
/// per frame do not bottleneck on 3 threads.
fn threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        let asked = std::env::var("OATHKEEP_LAND_THREADS").ok().and_then(|v| v.trim().parse().ok());
        asked
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get() / 2).clamp(2, 6))
            .clamp(1, 16)
    })
}

/// Whether views of the title's pages are written through one of ours.
fn beside_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LAND_BESIDE").is_none_or(|v| v != "0"))
}

/// Whether pieces an earlier fetch holds as they are now are left out
/// ([`Had`]; `OATHKEEP_LAND_SKIP`, on by default unless `=0` or `upcheck` is active).
pub(crate) fn skip_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LAND_SKIP").is_none_or(|v| v != "0"))
        && !crate::backend::upload_verify::on()
}

/// Since the last frame-rate line; microseconds first.
pub(crate) mod stat {
    /// Which pieces to fetch; their list recorded and sent.
    pub(crate) const GATHER: usize = 0;
    /// Looking for what shaders wrote to copies in the PC's memory, the part
    /// done while the fetch was on its way.
    pub(crate) const SCAN_BESIDE: usize = 1;
    /// The same, with nothing else to wait for.
    pub(crate) const SCAN: usize = 2;
    /// Merging what was fetched: until the last job is done, and the jobs'
    /// time on all threads.
    pub(crate) const CARD: usize = 3;
    pub(crate) const CARD_ALL: usize = 4;
    /// Merging what the scan found.
    pub(crate) const BACK: usize = 5;
    pub(crate) const BACK_ALL: usize = 6;
    /// In the jobs: waiting for the lock against the title's bytes going in.
    pub(crate) const LOCK: usize = 7;
    /// In the jobs: guards opened before a write, or noted after one.
    pub(crate) const NOTES: usize = 8;
    /// The flights' ranges done with; reading back, resets; what the title
    /// is told; the rest (the line's account).
    pub(crate) const DONE: usize = 9;
    pub(crate) const READ_BACKS: usize = 10;
    pub(crate) const TOLD: usize = 11;
    pub(crate) const REST: usize = 12;
    /// Counts: pieces merged, those with a new byte, their 4 KiB pages,
    /// those with a new byte, blocks of 32 bytes with a new byte, dwords put.
    pub(crate) const PIECES: usize = 13;
    pub(crate) const NEW_PIECES: usize = 14;
    pub(crate) const PAGES: usize = 15;
    pub(crate) const NEW_PAGES: usize = 16;
    pub(crate) const NEW_BLOCKS: usize = 17;
    pub(crate) const DWORDS: usize = 18;
    /// Pieces put through a view of ours, through the title's; pages where
    /// title memory was not there.
    pub(crate) const BESIDE: usize = 19;
    pub(crate) const DIRECT: usize = 20;
    pub(crate) const MISSING: usize = 21;
    /// Bytes asked for twice in a landing (fetched once); pieces and bytes
    /// an earlier fetch held already.
    pub(crate) const TWICE: usize = 22;
    pub(crate) const HAD: usize = 23;
    pub(crate) const HAD_BYTES: usize = 24;
    /// Jobs.
    pub(crate) const JOBS: usize = 25;
    pub(crate) const COUNT: usize = 26;
}

static STATS: [AtomicU64; stat::COUNT] = [const { AtomicU64::new(0) }; stat::COUNT];

pub(crate) fn count(which: usize, n: u64) {
    STATS[which].fetch_add(n, Ordering::Relaxed);
}

/// Adds the microseconds since `from` to `which`; now.
pub(crate) fn spent(which: usize, from: std::time::Instant) -> std::time::Instant {
    let now = std::time::Instant::now();
    count(which, (now - from).as_micros() as u64);
    now
}

/// For the frame-rate line: where landing went, per frame.
pub(crate) fn log(frames: f64) {
    let t: Vec<f64> = piece::TAKEN.iter().map(|t| t.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    if t[0] > 0.0 {
        tracing::info!(
            "blocks landed in beside the guards per frame: {:.0}, {:.0} of them held by the copy they were landed through, which takes \
             them no more (OATHKEEP_LAND_TAKEN={})",
            t[0],
            t[1],
            u8::from(piece::taken()),
        );
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    if s[stat::PIECES] == 0.0 && s[stat::REST] == 0.0 {
        return;
    }
    let ms = |i: usize| s[i] / 1000.0;
    let mb = |v: f64| v / (1 << 20) as f64;
    tracing::info!(
        "landing per frame (OATHKEEP_LAND={}, {} threads): which pieces to fetch {:.2} ms, then the fetches (the relay's line); \
         looking through copies in the PC's memory {:.2} ms beside them and {:.2} ms more; merging what was fetched {:.2} ms \
         ({:.2} ms on all threads, {:.0} jobs), merging the others {:.2} ms ({:.2} on all threads); of the threads' time \
         {:.2} ms waiting for the title's bytes going in, {:.2} ms opening or noting guards; then ranges done with {:.2} ms, \
         read-backs and lists {:.2}, the title told {:.2}, the rest {:.2}",
        u8::from(on()),
        threads(),
        ms(stat::GATHER),
        ms(stat::SCAN_BESIDE),
        ms(stat::SCAN),
        ms(stat::CARD),
        ms(stat::CARD_ALL),
        s[stat::JOBS],
        ms(stat::BACK),
        ms(stat::BACK_ALL),
        ms(stat::LOCK),
        ms(stat::NOTES),
        ms(stat::DONE),
        ms(stat::READ_BACKS),
        ms(stat::TOLD),
        ms(stat::REST),
    );
    tracing::info!(
        "landed per frame: {:.0} pieces, {:.0} with a new byte; of their {:.0} pages of 4 KiB {:.0} with a new byte ({:.1} MB), \
         {:.1} MB in blocks of 32 bytes with one, {:.2} MB put as dwords; {:.0} pieces through a view of ours \
         (OATHKEEP_LAND_BESIDE={}), {:.0} through the title's; {:.0} pages not there; asked for twice in a landing {:.1} MB; \
         held already by an earlier fetch {:.0} pieces {:.1} MB (OATHKEEP_LAND_SKIP={})",
        s[stat::PIECES],
        s[stat::NEW_PIECES],
        s[stat::PAGES],
        s[stat::NEW_PAGES],
        mb(s[stat::NEW_PAGES] * PAGE as f64),
        mb(s[stat::NEW_BLOCKS] * 32.0),
        mb(s[stat::DWORDS] * 4.0),
        s[stat::BESIDE],
        u8::from(beside_on()),
        s[stat::DIRECT],
        s[stat::MISSING],
        mb(s[stat::TWICE]),
        s[stat::HAD],
        mb(s[stat::HAD_BYTES]),
        u8::from(skip_on()),
    );
}

#[cfg(test)]
mod tests;
