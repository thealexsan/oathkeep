use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, RwLock};

use super::ask::writes;
use super::bits::PageHash;
#[cfg(feature = "diagnostics")]
use super::early::slow;
#[cfg(feature = "diagnostics")]
use super::guards::tally;
use super::guards::published;
use super::{KERNEL_PAGE, PAGE, PARTS, WHOLE};

/// Whether the parts of a page that the kernel names are kept, and a refresh
/// copies those (`OATHKEEP_PAGE_PARTS=0`: the 64 KiB page, as before).
///
/// The kernel says which 4 KiB pages were written, and a copy took the 64 KiB
/// around each: 29 MB a frame written so were most of 114 MB copied, and as
/// much sent up and hashed again on the card (session play-1125-0203, run
/// cpu-2). What is checked is as before, a page.
fn parts_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_PAGE_PARTS").is_none_or(|v| v != "0"))
}

impl Journal {
    /// Page `page` was found written by asking `now`: the parts in `parts`
    /// (a bit each). The asking that found it written before (0: none).
    pub(super) fn note(&mut self, page: usize, parts: u16, now: u64) -> u64 {
        let before = self.written.insert(page, now).unwrap_or(0);
        if parts == WHOLE || !parts_on() {
            self.parts.remove(&page);
            return before;
        }
        let all = self.parts.entry(page).or_insert([before; PARTS]);
        for (k, at) in all.iter_mut().enumerate() {
            if parts >> k & 1 != 0 {
                *at = now;
            }
        }
        before
    }

    /// The parts of `page` written by an asking after `taken`.
    pub(super) fn parts_since(&self, page: usize, taken: u64) -> u16 {
        match self.parts.get(&page) {
            Some(all) => all.iter().enumerate().filter(|(_, at)| **at > taken).fold(0, |m, (k, _)| m | 1 << k),
            None => WHOLE,
        }
    }

    /// Whether `[first, last)` (in pages) is covered by an interval in `ahead`.
    /// `ahead` is kept sorted and disjoint.
    pub(super) fn has_ahead(&self, first: usize, last: usize) -> bool {
        let i = self.ahead.partition_point(|&(a, _)| a <= first);
        i > 0 && self.ahead[i - 1].1 >= last
    }
}

/// Sort and merge overlapping/adjacent `(first, last)` page intervals in place.
pub(super) fn normalize_ahead(ahead: &mut Vec<(usize, usize)>) {
    if ahead.len() <= 1 {
        return;
    }
    ahead.sort_unstable();
    let mut write = 0;
    for read in 1..ahead.len() {
        let (a, b) = ahead[read];
        if a <= ahead[write].1 {
            ahead[write].1 = ahead[write].1.max(b);
        } else {
            write += 1;
            ahead[write] = (a, b);
        }
    }
    ahead.truncate(write + 1);
}

/// Of the pages of a copy that were written after it took them (page number,
/// the asking it took the page at): the parts to take again, a bit for each
/// 4 KiB, in the same order.
pub(crate) fn parts_written(pages: &[(usize, u64)]) -> Vec<u16> {
    let j = journal();
    pages.iter().map(|&(page, taken)| j.parts_since(page, taken)).collect()
}

/// Numbers the askings; 0 is "never". Counted up with the journal held
/// (shared will do: what notes an answer holds it alone).
pub(super) static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn next_asking() -> u64 {
    ASKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// A title-memory range whose changed pages another subsystem needs to read.
pub(crate) struct Tracked {
    start: usize,
    end: usize,
    seen: Mutex<BTreeMap<usize, u64>>,
}

/// Tracks a runtime GPU-memory range, including physical-page views when
/// `views` is true. Other process memory is not tracked.
pub(crate) fn track(start: usize, len: usize, views: bool) -> Option<Tracked> {
    let end = start.checked_add(len)?;
    if len == 0 || writes(start, end, views).is_none() {
        return None;
    }
    Some(Tracked {
        start,
        end,
        seen: Mutex::default(),
    })
}

pub(super) struct Journal {
    /// Page number → the asking that last found it written.
    pub(super) written: BTreeMap<usize, u64>,
    /// The same for each of a page's sixteen parts ([`KERNEL_PAGE`]), where
    /// the parts are known ([`parts_on`]); a page that is not here was
    /// written whole, as far as anyone knows.
    pub(super) parts: std::collections::HashMap<usize, [u64; PARTS], std::hash::BuildHasherDefault<PageHash>>,
    /// Page ranges asked ahead for a submission's refreshes (`ask_ahead`)
    /// and the asking that did: `writes` answers for a range inside one
    /// from the journal, without another kernel call.
    pub(super) ahead: Vec<(usize, usize)>,
    pub(super) ahead_asked: u64,
    /// The ahead asking stands for a whole submission (`ask_once`): no
    /// other ahead asking is made, and `ahead` stays, until
    /// `ask_once_end`.
    pub(super) kept: bool,
}

/// Read by many threads at once (a submission's refreshes run on several),
/// written only to note an answer: the kernel is asked, and view memory
/// guarded, with the range held in [`ASKING`] and not under this lock. Under
/// one lock for all of it those threads ran one at a time: 22 of 96 looks at
/// the submitting thread, and one in six of those at the six threads beside
/// it, found it waiting for that lock (run pooloff1).
static JOURNAL: RwLock<Journal> = RwLock::new(Journal {
    written: BTreeMap::new(),
    parts: std::collections::HashMap::with_hasher(std::hash::BuildHasherDefault::new()),
    ahead: Vec::new(),
    ahead_asked: 0,
    kept: false,
});

/// Lock-free mirror of `JOURNAL.kept`: true while a submission's `ask_once`
/// asking stands.
pub(super) static KEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lock-free mirror of `!JOURNAL.ahead.is_empty()`.
pub(super) static AHEAD_NONEMPTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a submission's `ask_once` asking is currently active.
pub(crate) fn once_kept() -> bool {
    KEPT.load(std::sync::atomic::Ordering::Acquire)
}

/// Fast lock-free pre-check for `nothing_new`: false if neither `published`
/// nor any `ahead` range can answer `[start, end)`.
pub(crate) fn can_answer_without_asking(start: usize, end: usize) -> bool {
    published(start, end) || AHEAD_NONEMPTY.load(std::sync::atomic::Ordering::Acquire)
}

pub(super) fn journal() -> std::sync::RwLockReadGuard<'static, Journal> {
    if let Ok(j) = JOURNAL.try_read() {
        return j;
    }
    waited(1, || JOURNAL.read().unwrap_or_else(|e| e.into_inner()))
}

pub(super) fn journal_mut() -> std::sync::RwLockWriteGuard<'static, Journal> {
    if let Ok(j) = JOURNAL.try_write() {
        return j;
    }
    waited(2, || JOURNAL.write().unwrap_or_else(|e| e.into_inner()))
}

/// Since the last line: waits and nanoseconds of waiting, of all threads, for
/// the ranges held ([`ASKING`]), the journal to read it, the journal to note
/// in it, and the guards (faults of the title's among them).
#[cfg(feature = "diagnostics")]
static WAITS: [[std::sync::atomic::AtomicU64; 2]; 4] = [const { [const { std::sync::atomic::AtomicU64::new(0) }; 2] }; 4];

#[cfg(feature = "diagnostics")]
pub(super) fn waited<T>(what: usize, f: impl FnOnce() -> T) -> T {
    use std::sync::atomic::Ordering::Relaxed;
    let began = std::time::Instant::now();
    let got = f();
    WAITS[what][0].fetch_add(1, Relaxed);
    WAITS[what][1].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
    got
}

#[cfg(not(feature = "diagnostics"))]
pub(super) fn waited<T>(_: usize, f: impl FnOnce() -> T) -> T {
    f()
}

/// [`WAITS`], taken, for the log: per frame.
#[cfg(feature = "diagnostics")]
pub(crate) fn lock_waits(frames: f64) -> String {
    let of = |what: usize| {
        let [n, ns] = [0, 1].map(|i| WAITS[what][i].swap(0, std::sync::atomic::Ordering::Relaxed));
        format!("{:.0} {:.2} ms", n as f64 / frames, ns as f64 / 1e6 / frames)
    };
    format!("ranges held {}, journal read {}, journal noted in {}, guards {}", of(0), of(1), of(2), of(3))
}

/// The page ranges some thread is asking about: asking forgets what it
/// answers, so until the answer is in the journal no other thread may ask
/// about any of the same pages (it would be told they were not written).
/// Taken before the journal's lock, never under it.
static ASKING: (Mutex<Vec<(usize, usize)>>, Condvar) = (Mutex::new(Vec::new()), Condvar::new());

/// Pages `[first, last)` held in [`ASKING`] until dropped.
pub(super) struct Asking(usize, usize);

impl Asking {
    pub(super) fn hold(first: usize, last: usize) -> Asking {
        let mut held = ASKING.0.lock().unwrap_or_else(|e| e.into_inner());
        if held.iter().any(|&(a, b)| a < last && first < b) {
            held = waited(0, || {
                let mut held = held;
                while held.iter().any(|&(a, b)| a < last && first < b) {
                    held = ASKING.1.wait(held).unwrap_or_else(|e| e.into_inner());
                }
                held
            });
        }
        held.push((first, last));
        Asking(first, last)
    }

    /// Everything: an asking for a whole submission.
    pub(super) fn hold_all() -> Asking {
        Asking::hold(0, usize::MAX)
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        let mut held = ASKING.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = held.iter().position(|&r| r == (self.0, self.1)) {
            held.swap_remove(i);
        }
        drop(held);
        ASKING.1.notify_all();
    }
}

thread_local! {
    /// Room for the kernel's answer.
    static FOUND: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The pages of `[first, last)` the kernel says were written since it was
/// last asked, in order, each once, with their written parts; `None` if the range is not write-watched
/// memory of one reservation. The caller holds the range ([`Asking`]).
pub(super) fn kernel_written(first: usize, last: usize) -> Option<Vec<(usize, u16)>> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    FOUND.with(|found| {
        let mut found = found.borrow_mut();
        let mut count = (last - first) * PAGE / KERNEL_PAGE;
        if found.len() < count {
            found.resize(count, 0);
        }
        let mut granularity = 0;
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        // SAFETY: the answer buffer holds `count` entries.
        let failed = unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        } != 0;
        #[cfg(feature = "diagnostics")]
        {
            tally(10, u64::from(failed));
            tally(11, started.elapsed().as_nanos() as u64);
        }
        if failed {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        {
            tally(12, count as u64);
            use crate::copystat::{C, add};
            add(C::Watches, 1);
            add(C::WatchUs, started.elapsed().as_micros() as u64);
            slow(started.elapsed().as_micros() as u64);
            tally(0, 1);
            tally(1, ((last - first) * (PAGE >> 10)) as u64);
        }
        let mut pages: Vec<(usize, u16)> = Vec::new();
        for &at in &found[..count] {
            let (page, part) = (at / PAGE, 1u16 << (at % PAGE / KERNEL_PAGE));
            match pages.last_mut() {
                Some((last, parts)) if *last == page => *parts |= part,
                _ => pages.push((page, part)),
            }
        }
        Some(pages)
    })
}

impl Tracked {
    /// Takes changed pages of `[from, to)`, cut to the tracked range. A
    /// partial page stays changed so a later take can read its other bytes.
    pub(crate) fn take(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let (from, to) = (from.max(self.start), to.min(self.end));
        if from >= to {
            return Vec::new();
        }
        let Some((_, pages)) = writes(from, to, true) else {
            return Vec::new();
        };
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut new: Vec<(usize, u64)> = Vec::new();
        for (page, write) in pages {
            let last = seen.get(&page).copied();
            if last.is_some_and(|last| last >= write) || (page * PAGE).max(from) >= ((page + 1) * PAGE).min(to) {
                continue;
            }
            // Keep partial pages dirty: the caller has not taken the rest.
            if (page * PAGE).max(self.start) >= from
                && ((page + 1) * PAGE).min(self.end) <= to
            {
                seen.insert(page, write);
            }
            new.push((page, last.unwrap_or(0)));
        }
        // Of a page taken before, the parts written since, where they are
        // known (`OATHKEEP_TAKE_PARTS=0`: the page).
        let parts = if crate::toggled!("OATHKEEP_TAKE_PARTS") { parts_written(&new) } else { vec![WHOLE; new.len()] };
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (&(page, last), &parts) in new.iter().zip(&parts) {
            let mut parts = if last == 0 { WHOLE } else { parts };
            while parts != 0 {
                let first = parts.trailing_zeros() as usize;
                let past = first + (!(parts >> first)).trailing_zeros() as usize;
                parts &= if past >= PARTS { 0 } else { u16::MAX << past };
                let (start, end) = ((page * PAGE + first * KERNEL_PAGE).max(from), (page * PAGE + past.min(PARTS) * KERNEL_PAGE).min(to));
                if start >= end {
                    continue;
                }
                match ranges.last_mut() {
                    Some((_, last)) if *last == start => *last = end,
                    _ => ranges.push((start, end)),
                }
            }
        }
        ranges
    }
}
