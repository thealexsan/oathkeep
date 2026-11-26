use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

pub(crate) const PAGE: usize = 64 << 10;
const KERNEL_PAGE: usize = 4096;
pub(crate) const HOT_WRITE: u64 = u64::MAX;
const DEFAULT_HOT_FRAMES: u64 = 6;
const DEFAULT_HOT_FAULTS: u16 = 4;
const HOT_STREAK_GAP: u64 = 2;
const MAX_COOL_BACKOFF: u8 = 1;
pub(crate) const CLEAN_EPOCH_SHARDS: usize = 8;

static GLOBAL_WRITE_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RANGED_WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WriteStamp {
    pub(crate) global: u64,
    pub(crate) ranged: u64,
}

const WRITE_RANGE_LOG_CAP: usize = 16384;

struct WriteRangeLog {
    seq: u64,
    entries: [(u64, usize, usize); WRITE_RANGE_LOG_CAP],
}

static WRITE_RANGE_LOG: std::sync::Mutex<WriteRangeLog> = std::sync::Mutex::new(WriteRangeLog {
    seq: 0,
    entries: [(0, 0, 0); WRITE_RANGE_LOG_CAP],
});

#[inline]
pub(crate) fn write_stamp() -> WriteStamp {
    let ranged = RANGED_WRITE_SEQ.load(std::sync::atomic::Ordering::Acquire);
    let global = GLOBAL_WRITE_EPOCH.load(std::sync::atomic::Ordering::Acquire);
    WriteStamp { global, ranged }
}

#[inline]
fn bump_global_write_epoch() {
    GLOBAL_WRITE_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
}

#[inline]
fn record_written_range(start: usize, end: usize) {
    if start >= end {
        return;
    }
    match WRITE_RANGE_LOG.try_lock() {
        Ok(mut log) => {
            let seq = log.seq.wrapping_add(1);
            log.seq = seq;
            log.entries[(seq as usize) % WRITE_RANGE_LOG_CAP] = (seq, start, end);
            RANGED_WRITE_SEQ.store(seq, std::sync::atomic::Ordering::Release);
        }
        Err(_) => bump_global_write_epoch(),
    }
}

pub(crate) fn written_ranges_since(prev: WriteStamp, now: WriteStamp) -> Option<Vec<(usize, usize)>> {
    if prev.global != now.global {
        return None;
    }
    if prev.ranged == now.ranged {
        return Some(Vec::new());
    }
    if now.ranged < prev.ranged || now.ranged - prev.ranged > WRITE_RANGE_LOG_CAP as u64 {
        return None;
    }
    let log = WRITE_RANGE_LOG.lock().unwrap_or_else(|e| e.into_inner());
    if log.seq < now.ranged || log.seq - prev.ranged > WRITE_RANGE_LOG_CAP as u64 {
        return None;
    }
    let count = (now.ranged - prev.ranged) as usize;
    let mut out = Vec::with_capacity(count);
    for seq in (prev.ranged + 1)..=now.ranged {
        let (entry_seq, start, end) = log.entries[(seq as usize) % WRITE_RANGE_LOG_CAP];
        if entry_seq != seq {
            return None;
        }
        out.push((start, end));
    }
    Some(out)
}

fn hot_frames() -> u64 {
    static FRAMES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("OATHKEEP_HOT_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_HOT_FRAMES)
    })
}

fn hot_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_GUARD_HOT").is_none_or(|v| v != "0"))
}

fn hot_faults() -> u16 {
    static FAULTS: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    *FAULTS.get_or_init(|| {
        std::env::var("OATHKEEP_HOT_FAULTS")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_HOT_FAULTS)
    })
}

fn merge_hot(out: &mut Vec<(usize, u64)>, hot: &[usize]) {
    if hot.is_empty() {
        return;
    }
    if out.is_empty() {
        out.extend(hot.iter().map(|&p| (p, HOT_WRITE)));
        return;
    }
    let existing = std::mem::take(out);
    out.reserve(existing.len() + hot.len());
    let (mut i, mut j) = (0, 0);
    while i < existing.len() && j < hot.len() {
        let (ep, ew) = existing[i];
        let hp = hot[j];
        if ep < hp {
            out.push((ep, ew));
            i += 1;
        } else if hp < ep {
            out.push((hp, HOT_WRITE));
            j += 1;
        } else {
            out.push((hp, HOT_WRITE));
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&existing[i..]);
    out.extend(hot[j..].iter().map(|&p| (p, HOT_WRITE)));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchKind {
    WriteWatch,
    Guarded,
}

#[derive(Default)]
struct IntervalSet(BTreeMap<usize, usize>);

impl IntervalSet {
    const fn new() -> Self {
        Self(BTreeMap::new())
    }

    fn contains(&self, block: usize) -> bool {
        self.0
            .range(..=block)
            .next_back()
            .is_some_and(|(_, &end)| end > block)
    }

    fn contains_range(&self, first: usize, last: usize) -> bool {
        if first >= last {
            return true;
        }
        self.0
            .range(..=first)
            .next_back()
            .is_some_and(|(_, &end)| end >= last)
    }

    fn overlaps(&self, first: usize, last: usize) -> bool {
        if first >= last {
            return false;
        }
        if self
            .0
            .range(..=first)
            .next_back()
            .is_some_and(|(_, &end)| end > first)
        {
            return true;
        }
        self.0.range(first..last).next().is_some()
    }

    fn insert(&mut self, block: usize) {
        self.insert_range(block, block.saturating_add(1));
    }

    fn insert_range(&mut self, mut first: usize, mut last: usize) {
        if first >= last {
            return;
        }
        if let Some((&prev_start, &prev_end)) = self.0.range(..=first).next_back()
            && prev_end >= first
        {
            first = prev_start;
            last = last.max(prev_end);
        }
        let overlapping: Vec<(usize, usize)> = self
            .0
            .range(first..=last)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in overlapping {
            self.0.remove(&s);
            last = last.max(e);
        }
        self.0.insert(first, last);
    }

    fn remove(&mut self, block: usize) {
        self.remove_range(block, block.saturating_add(1));
    }

    fn remove_range(&mut self, first: usize, last: usize) -> Vec<(usize, usize)> {
        let mut removed = Vec::new();
        if first >= last {
            return removed;
        }
        let mut to_remove = Vec::new();
        let mut to_insert = Vec::new();
        if let Some((&s, &e)) = self.0.range(..first).next_back()
            && e > first
        {
            to_remove.push(s);
            removed.push((first, e.min(last)));
            to_insert.push((s, first));
            if e > last {
                to_insert.push((last, e));
            }
        }
        for (&s, &e) in self.0.range(first..last) {
            to_remove.push(s);
            removed.push((s, e.min(last)));
            if e > last {
                to_insert.push((last, e));
            }
        }
        for s in to_remove {
            self.0.remove(&s);
        }
        for (s, e) in to_insert {
            self.0.insert(s, e);
        }
        removed
    }

    fn gaps_in(&self, first: usize, last: usize) -> Vec<(usize, usize)> {
        let mut gaps = Vec::new();
        if first >= last {
            return gaps;
        }
        let mut at = first;
        if let Some((_, &e)) = self.0.range(..first).next_back()
            && e > at
        {
            at = e;
        }
        if at >= last {
            return gaps;
        }
        for (&s, &e) in self.0.range(first..last) {
            if s > at {
                gaps.push((at, s.min(last)));
            }
            at = at.max(e);
            if at >= last {
                break;
            }
        }
        if at < last {
            gaps.push((at, last));
        }
        gaps
    }

    fn count_in(&self, first: usize, last: usize) -> u64 {
        if first >= last {
            return 0;
        }
        let mut count = 0u64;
        if let Some((_, &e)) = self.0.range(..first).next_back()
            && e > first
        {
            count += (e.min(last) - first) as u64;
        }
        for (&s, &e) in self.0.range(first..last) {
            count += (e.min(last) - s) as u64;
        }
        count
    }

    fn for_each_overlap(&self, first: usize, last: usize, mut f: impl FnMut(usize, usize)) {
        if first >= last {
            return;
        }
        if let Some((_, &e)) = self.0.range(..first).next_back()
            && e > first
        {
            f(first, e.min(last));
        }
        for (&s, &e) in self.0.range(first..last) {
            f(s, e.min(last));
        }
    }
}

struct Journal {
    asked: u64,
    /// Monotonic counter bumped ONLY when guarded pages or `wrote_journal_only` entries
    /// are inserted into `written` (never bumped by `GetWriteWatch` on `MEM_PRIVATE`
    /// buffers), so `writes_guarded_since` skips BTreeMap range scans when no guarded
    /// write happened since `since`.
    guarded_asked: u64,
    written: BTreeMap<usize, u64>,
    found: Vec<usize>,
    ahead: Vec<(usize, usize)>,
    ahead_asked: u64,
    kept: bool,
}

static JOURNAL: RwLock<Journal> = RwLock::new(Journal {
    asked: 1,
    guarded_asked: 1,
    written: BTreeMap::new(),
    found: Vec::new(),
    ahead: Vec::new(),
    ahead_asked: 1,
    kept: false,
});

static JOURNAL_KEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static JOURNAL_ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[inline]
fn bump_journal_asked(j: &mut Journal, delta: u64) -> u64 {
    j.asked += delta;
    JOURNAL_ASKED.fetch_max(j.asked, std::sync::atomic::Ordering::Release);
    j.asked
}

fn journal_read() -> std::sync::RwLockReadGuard<'static, Journal> {
    JOURNAL.read().unwrap_or_else(|e| e.into_inner())
}

fn journal_write() -> std::sync::RwLockWriteGuard<'static, Journal> {
    JOURNAL.write().unwrap_or_else(|e| e.into_inner())
}

const AHEAD_GAP: usize = (512 << 10) / PAGE;
const AHEAD_MAX: usize = (16 << 20) / PAGE;

fn ask_ahead_locked(j: &mut Journal, pieces: &[(usize, usize)], allow_single: bool) {
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len()
            && pieces[k].0 <= last + AHEAD_GAP
            && last.max(pieces[k].1) - first <= AHEAD_MAX
        {
            last = last.max(pieces[k].1);
            k += 1;
        }
        if k - i == 1 {
            if allow_single
                && !j.ahead.iter().any(|&(a, b)| a <= first && last <= b)
                && ask_kernel(j, first, last)
            {
                j.ahead.push((first, last));
            }
        } else {
            ask_run(j, &pieces[i..k], allow_single);
        }
        i = k;
    }
}

pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1)
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable();
    let mut j = journal_write();
    let j = &mut *j;
    if j.kept {
        return;
    }
    let now = bump_journal_asked(j, 1);
    j.ahead_asked = now;
    j.ahead.clear();
    ask_ahead_locked(j, &pieces, false);
}

#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
fn slow(us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if us >= 1000 {
        SLOW[0].fetch_add(1, Relaxed);
        SLOW[1].fetch_add(us, Relaxed);
    }
    SLOW[2].fetch_max(us, Relaxed);
}

#[cfg(feature = "diagnostics")]
pub(crate) fn slow_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

pub(crate) fn ask_ahead_done() {
    let mut j = journal_write();
    if !j.kept {
        j.ahead.clear();
    }
}

pub(crate) fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_ASK_ONCE").is_none_or(|v| v != "0"))
}

pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
    if !once() {
        return;
    }
    ask_once_end();
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1)
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.is_empty() {
        return;
    }
    pieces.sort_unstable();
    // Collapse strictly overlapping/adjacent page ranges before coalescing.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(pieces.len());
    for (a, b) in pieces {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let mut j = journal_write();
    let j = &mut *j;
    let now = bump_journal_asked(j, 1);
    j.ahead_asked = now;
    j.ahead.clear();
    ask_ahead_locked(j, &merged, true);
    j.kept = !j.ahead.is_empty();
    JOURNAL_KEPT.store(j.kept, std::sync::atomic::Ordering::Release);
}

pub(crate) fn is_journal_kept() -> bool {
    JOURNAL_KEPT.load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) fn ask_once_end() {
    bump_global_write_epoch();
    if !JOURNAL_KEPT.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut j = journal_write();
    if j.kept {
        j.kept = false;
        JOURNAL_KEPT.store(false, std::sync::atomic::Ordering::Release);
        j.ahead.clear();
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn wrote(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    record_written_range(at, at.saturating_add(len));
    wrote_journal_only(at, len);
}

pub(crate) fn wrote_after_writing(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    // `watch::writing(at, len)` already recorded the range in `WRITE_RANGE_LOG`.
    wrote_journal_only(at, len);
}

fn wrote_journal_only(at: usize, len: usize) {
    if !JOURNAL_KEPT.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut j = journal_write();
    if !j.kept {
        return;
    }
    let now = bump_journal_asked(&mut j, 1);
    j.guarded_asked = now;
    j.ahead_asked = now;
    let mut prev_shard = usize::MAX;
    let mut prev_sub = usize::MAX;
    for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        j.written.insert(page, now);
        let sub = sub_of(page);
        if sub != prev_sub {
            SUB_GUARDED_ASKED[sub].fetch_max(now, std::sync::atomic::Ordering::Release);
            prev_sub = sub;
        }
        let s = shard_of(page);
        if s != prev_shard {
            SHARD_GUARDED_ASKED[s].fetch_max(now, std::sync::atomic::Ordering::Release);
            prev_shard = s;
        }
    }
}

fn ask_run(j: &mut Journal, pieces: &[(usize, usize)], allow_single: bool) {
    if pieces.is_empty() {
        return;
    }
    if pieces.len() == 1 {
        if allow_single {
            let (first, last) = pieces[0];
            if !j.ahead.iter().any(|&(a, b)| a <= first && last <= b) && ask_kernel(j, first, last) {
                j.ahead.push((first, last));
            }
        }
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if ask_kernel(j, first, last) {
        j.ahead.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run(j, a, allow_single);
    ask_run(j, b, allow_single);
}

fn ask_kernel(j: &mut Journal, first: usize, last: usize) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    if failed {
        return false;
    }
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    let now = j.ahead_asked;
    let mut prev_page = usize::MAX;
    for k in 0..count {
        let page = j.found[k] / PAGE;
        if page != prev_page {
            j.written.insert(page, now);
            prev_page = page;
        }
    }
    true
}

pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let j = journal_read();
    j.ahead
        .iter()
        .any(|&(a, b)| a <= first && last <= b)
        .then(|| {
            (
                j.ahead_asked,
                j.written
                    .range(first..last)
                    .map(|(&p, &w)| (p, w))
                    .collect(),
            )
        })
}

pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    writes_kind(start, end, guarded).map(|(now, written, _)| (now, written))
}

pub(crate) fn writes_kind(
    start: usize,
    end: usize,
    guarded: bool,
) -> Option<(u64, Vec<(usize, u64)>, WatchKind)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    {
        let j = journal_read();
        if j.ahead.iter().any(|&(a, b)| a <= first && last <= b) {
            return Some((
                j.ahead_asked,
                j.written
                    .range(first..last)
                    .map(|(&p, &w)| (p, w))
                    .collect(),
                WatchKind::WriteWatch,
            ));
        }
    }
    let failed = {
        let mut j = journal_write();
        let j = &mut *j;
        if j.ahead.iter().any(|&(a, b)| a <= first && last <= b) {
            return Some((
                j.ahead_asked,
                j.written
                    .range(first..last)
                    .map(|(&p, &w)| (p, w))
                    .collect(),
                WatchKind::WriteWatch,
            ));
        }
        let mut count = (last - first) * PAGE / KERNEL_PAGE;
        j.found.resize(count.max(j.found.len()), 0);
        let mut granularity = 0;
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        // SAFETY: the answer buffer holds `count` entries.
        let failed = unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                j.found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        } != 0;
        if !failed {
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::Watches, 1);
                add(C::WatchUs, started.elapsed().as_micros() as u64);
                slow(started.elapsed().as_micros() as u64);
            }
            let now = bump_journal_asked(j, 1);
            let mut prev_page = usize::MAX;
            for k in 0..count {
                let page = j.found[k] / PAGE;
                if page != prev_page {
                    j.written.insert(page, now);
                    prev_page = page;
                }
            }
            return Some((
                now,
                j.written
                    .range(first..last)
                    .map(|(&p, &w)| (p, w))
                    .collect(),
                WatchKind::WriteWatch,
            ));
        }
        true
    };
    debug_assert!(failed);
    if !guarded {
        return None;
    }
    let (now, written) = writes_guarded(start, end)?;
    Some((now, written, WatchKind::Guarded))
}

pub(crate) fn writes_guarded(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    writes_guarded_since(start, end, 0)
}

pub(crate) fn writes_guarded_since(
    start: usize,
    end: usize,
    since: u64,
) -> Option<(u64, Vec<(usize, u64)>)> {
    writes_guarded_since_with_clean(start, end, since).map(|(now, written, _)| (now, written))
}

pub(crate) fn writes_guarded_since_with_clean(
    start: usize,
    end: usize,
    since: u64,
) -> Option<(u64, Vec<(usize, u64)>, Option<[u64; CLEAN_EPOCH_SHARDS]>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    #[cfg(feature = "diagnostics")]
    let guarding = std::time::Instant::now();
    let written = guard_with_hot_fast(first * PAGE, last * PAGE);
    #[cfg(feature = "diagnostics")]
    {
        let us = guarding.elapsed().as_micros() as u64;
        crate::copystat::add(crate::copystat::C::GuardUs, us);
        crate::copystat::add(crate::copystat::C::Watches, 1);
        crate::copystat::add(crate::copystat::C::WatchUs, us);
        slow(us);
    }
    let (guard_hot, clean_epochs) = written?;
    let (guarded, hot) = match guard_hot {
        GuardHot::HotOnly(pairs) => {
            if since != 0 && all_shards_guarded_asked_le(first, last, since) {
                let now = JOURNAL_ASKED
                    .load(std::sync::atomic::Ordering::Acquire)
                    .max(since);
                return Some((now, pairs, clean_epochs));
            }
            let j = journal_read();
            if since != 0 && j.guarded_asked <= since {
                return Some((j.asked, pairs, clean_epochs));
            }
            let mut out: Vec<(usize, u64)> = j
                .written
                .range(first..last)
                .filter(|&(_, &w)| since == 0 || w > since)
                .map(|(&p, &w)| (p, w))
                .collect();
            if !pairs.is_empty() {
                if out.is_empty() {
                    return Some((j.asked, pairs, clean_epochs));
                }
                let hot: Vec<usize> = pairs.into_iter().map(|(p, _)| p).collect();
                merge_hot(&mut out, &hot);
            }
            return Some((j.asked, out, clean_epochs));
        }
        GuardHot::Guarded { guarded, hot } => (guarded, hot),
    };
    if guarded.is_empty() {
        if since != 0 && all_shards_guarded_asked_le(first, last, since) {
            let out = if hot.is_empty() {
                Vec::new()
            } else {
                hot.into_iter().map(|p| (p, HOT_WRITE)).collect()
            };
            let now = JOURNAL_ASKED
                .load(std::sync::atomic::Ordering::Acquire)
                .max(since);
            return Some((now, out, clean_epochs));
        }
        let j = journal_read();
        if since != 0 && j.guarded_asked <= since {
            let out = if hot.is_empty() {
                Vec::new()
            } else {
                hot.into_iter().map(|p| (p, HOT_WRITE)).collect()
            };
            return Some((j.asked, out, clean_epochs));
        }
        let mut out: Vec<(usize, u64)> = j
            .written
            .range(first..last)
            .filter(|&(_, &w)| since == 0 || w > since)
            .map(|(&p, &w)| (p, w))
            .collect();
        merge_hot(&mut out, &hot);
        return Some((j.asked, out, clean_epochs));
    }
    let mut j = journal_write();
    let shards_clean_before = since != 0
        && (j.guarded_asked <= since || all_shards_guarded_asked_le(first, last, since));
    let now = bump_journal_asked(&mut j, 1);
    j.guarded_asked = now;
    let mut prev_shard = usize::MAX;
    let mut prev_sub = usize::MAX;
    for &page in &guarded {
        j.written.insert(page, now);
        let sub = sub_of(page);
        if sub != prev_sub {
            SUB_GUARDED_ASKED[sub].fetch_max(now, std::sync::atomic::Ordering::Release);
            prev_sub = sub;
        }
        let s = shard_of(page);
        if s != prev_shard {
            SHARD_GUARDED_ASKED[s].fetch_max(now, std::sync::atomic::Ordering::Release);
            prev_shard = s;
        }
    }
    let mut out: Vec<(usize, u64)> = if since >= now {
        Vec::new()
    } else if shards_clean_before {
        debug_assert!(guarded.windows(2).all(|w| w[0] < w[1]));
        guarded.into_iter().map(|p| (p, now)).collect()
    } else {
        j.written
            .range(first..last)
            .filter(|&(_, &w)| since == 0 || w > since)
            .map(|(&p, &w)| (p, w))
            .collect()
    };
    merge_hot(&mut out, &hot);
    Some((now, out, clean_epochs))
}

const BLOCK: usize = PAGE;

struct Guards {
    protected: IntervalSet,
    clean: IntervalSet,
    absent: IntervalSet,
    /// Partly-committed 64 KiB blocks that cannot be `VirtualProtect`'d whole;
    /// kept writable and reported as `HOT_WRITE` until fully committed or remapped.
    partly: IntervalSet,
    /// Exact union `clean ∪ hot ∪ absent ∪ partly` (pairwise disjoint) for O(log n) fast-path checks.
    accounted: IntervalSet,
    aliased: BTreeSet<usize>,
    /// `(frame, faults_in_frame)` for intra-frame hot-page promotion.
    recent_fault: BTreeMap<usize, (u64, u16)>,
    hot: BTreeMap<usize, u64>,
    /// Number of times a block has been cooled/expired from `hot`, used for exponential
    /// backoff (`hot_frames() << cool_count.min(MAX_COOL_BACKOFF)`) when it re-faults into hot.
    cool_count: BTreeMap<usize, u8>,
}

impl Guards {
    const fn new() -> Self {
        Self {
            protected: IntervalSet::new(),
            clean: IntervalSet::new(),
            absent: IntervalSet::new(),
            partly: IntervalSet::new(),
            accounted: IntervalSet::new(),
            aliased: BTreeSet::new(),
            recent_fault: BTreeMap::new(),
            hot: BTreeMap::new(),
            cool_count: BTreeMap::new(),
        }
    }

    #[inline]
    fn record_fault_and_check_hot(&mut self, block: usize, frame: u64, threshold: u16) -> bool {
        let entry = self.recent_fault.entry(block).or_insert((frame, 0));
        if entry.0 == frame {
            entry.1 = entry.1.saturating_add(1);
        } else if frame > entry.0 && frame <= entry.0.saturating_add(HOT_STREAK_GAP) {
            entry.0 = frame;
            entry.1 = entry.1.saturating_add(1);
        } else {
            *entry = (frame, 1);
        }
        entry.1 >= threshold
    }
}

const GUARD_SHARDS: usize = 64;
/// 1024 blocks (64 MiB) per contiguous address stripe.
const SHARD_SHIFT: usize = 10;
const SUB_BUCKETS: usize = 4096;
/// 16 blocks (1 MiB) per sub-epoch bucket (4 GiB direct-mapped).
const SUB_SHIFT: usize = 4;

static GUARDS: [RwLock<Guards>; GUARD_SHARDS] =
    [const { RwLock::new(Guards::new()) }; GUARD_SHARDS];
static SHARD_GUARDED_ASKED: [std::sync::atomic::AtomicU64; GUARD_SHARDS] =
    [const { std::sync::atomic::AtomicU64::new(1) }; GUARD_SHARDS];
static SHARD_CLEAN_EPOCH: [std::sync::atomic::AtomicU64; GUARD_SHARDS] =
    [const { std::sync::atomic::AtomicU64::new(1) }; GUARD_SHARDS];
static SUB_GUARDED_ASKED: [std::sync::atomic::AtomicU64; SUB_BUCKETS] =
    [const { std::sync::atomic::AtomicU64::new(1) }; SUB_BUCKETS];
static SUB_CLEAN_EPOCH: [std::sync::atomic::AtomicU64; SUB_BUCKETS] =
    [const { std::sync::atomic::AtomicU64::new(1) }; SUB_BUCKETS];

#[inline]
fn sub_of(block: usize) -> usize {
    (block >> SUB_SHIFT) & (SUB_BUCKETS - 1)
}

#[inline]
fn bump_sub_clean_block(block: usize) {
    SUB_CLEAN_EPOCH[sub_of(block)].fetch_add(1, std::sync::atomic::Ordering::Release);
}

#[inline]
fn bump_sub_clean_range(a: usize, b: usize) {
    if a >= b {
        return;
    }
    let s0 = a >> SUB_SHIFT;
    let s1 = (b - 1) >> SUB_SHIFT;
    let count = (s1 - s0 + 1).min(SUB_BUCKETS);
    for i in 0..count {
        SUB_CLEAN_EPOCH[(s0 + i) & (SUB_BUCKETS - 1)]
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

#[inline]
fn bump_shard_clean_epoch(shard: usize) {
    SHARD_CLEAN_EPOCH[shard].fetch_add(1, std::sync::atomic::Ordering::Release);
}

#[inline]
fn shard_of(block: usize) -> usize {
    (block >> SHARD_SHIFT) & (GUARD_SHARDS - 1)
}

#[inline]
fn all_shards_guarded_asked_le(first: usize, last: usize, since: u64) -> bool {
    if first >= last {
        return true;
    }
    let sub_first = first >> SUB_SHIFT;
    let sub_last = (last - 1) >> SUB_SHIFT;
    if sub_last - sub_first < 64 {
        for s in sub_first..=sub_last {
            if SUB_GUARDED_ASKED[s & (SUB_BUCKETS - 1)].load(std::sync::atomic::Ordering::Acquire)
                > since
            {
                return false;
            }
        }
        return true;
    }
    let s_first = first >> SHARD_SHIFT;
    let s_last = (last - 1) >> SHARD_SHIFT;
    if s_last - s_first >= GUARD_SHARDS {
        return SHARD_GUARDED_ASKED
            .iter()
            .all(|a| a.load(std::sync::atomic::Ordering::Acquire) <= since);
    }
    for s in s_first..=s_last {
        if SHARD_GUARDED_ASKED[s & (GUARD_SHARDS - 1)].load(std::sync::atomic::Ordering::Acquire)
            > since
        {
            return false;
        }
    }
    true
}

#[inline]
pub(crate) fn shards_for_range(start: usize, end: usize) -> Option<([usize; CLEAN_EPOCH_SHARDS], usize)> {
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    if first >= last {
        return Some(([0; CLEAN_EPOCH_SHARDS], 0));
    }
    let sub_first = first >> SUB_SHIFT;
    let sub_last = (last - 1) >> SUB_SHIFT;
    let sub_count = (sub_last - sub_first) + 1;
    if sub_count <= CLEAN_EPOCH_SHARDS {
        let mut buckets = [0usize; CLEAN_EPOCH_SHARDS];
        for (slot, b) in buckets.iter_mut().take(sub_count).enumerate() {
            *b = (sub_first + slot) & (SUB_BUCKETS - 1);
        }
        return Some((buckets, sub_count));
    }
    let s_first = first >> SHARD_SHIFT;
    let s_last = (last - 1) >> SHARD_SHIFT;
    let count = (s_last - s_first) + 1;
    if count > CLEAN_EPOCH_SHARDS {
        return None;
    }
    let mut shards = [0usize; CLEAN_EPOCH_SHARDS];
    for (slot, s) in shards.iter_mut().take(count).enumerate() {
        *s = SUB_BUCKETS + ((s_first + slot) & (GUARD_SHARDS - 1));
    }
    Some((shards, count))
}

#[inline]
pub(crate) fn guarded_shards_epochs_match(
    shards: &[usize],
    epochs: &[u64],
    since: u64,
) -> bool {
    use std::sync::atomic::Ordering::Acquire;
    if since == 0 || shards.is_empty() || shards.len() != epochs.len() {
        return false;
    }
    for (&id, &ep) in shards.iter().zip(epochs.iter()) {
        if ep == 0 {
            return false;
        }
        if id < SUB_BUCKETS {
            if SUB_CLEAN_EPOCH[id].load(Acquire) != ep || SUB_GUARDED_ASKED[id].load(Acquire) > since
            {
                return false;
            }
        } else {
            let s = id - SUB_BUCKETS;
            if SHARD_CLEAN_EPOCH[s].load(Acquire) != ep
                || SHARD_GUARDED_ASKED[s].load(Acquire) > since
            {
                return false;
            }
        }
    }
    true
}

#[inline]
pub(crate) fn guarded_clean_epochs_match(
    start: usize,
    end: usize,
    epochs: [u64; CLEAN_EPOCH_SHARDS],
    since: u64,
) -> bool {
    if since == 0 || epochs[0] == 0 {
        return false;
    }
    let Some((shards, count)) = shards_for_range(start, end) else {
        return false;
    };
    guarded_shards_epochs_match(&shards[..count], &epochs[..count], since)
}

#[inline]
fn record_clean_epoch_span(
    clean_epochs: &mut [u64; CLEAN_EPOCH_SHARDS],
    use_sub: bool,
    sub_first: usize,
    slot: usize,
    shard: usize,
    a: usize,
    b: usize,
) {
    if use_sub {
        let sa = a >> SUB_SHIFT;
        let sb = (b - 1) >> SUB_SHIFT;
        for s in sa..=sb {
            let idx = s - sub_first;
            if idx < CLEAN_EPOCH_SHARDS {
                clean_epochs[idx] = SUB_CLEAN_EPOCH[s & (SUB_BUCKETS - 1)]
                    .load(std::sync::atomic::Ordering::Acquire);
            }
        }
    } else if slot < CLEAN_EPOCH_SHARDS {
        clean_epochs[slot] =
            SHARD_CLEAN_EPOCH[shard].load(std::sync::atomic::Ordering::Acquire);
    }
}

/// Splits `[start, end)` into contiguous subranges that are NOT known to be uncommitted
/// (`on_present`) vs. subranges whose 64 KiB blocks are recorded in `g.absent` (`on_absent`).
/// Only active when HLE demand-commit hooks (`COMMIT_TITLE_MEMORY`) are installed so `g.absent`
/// is guaranteed exact; otherwise invokes `on_present(start, end)` directly.
pub(crate) fn split_absent_spans(
    start: usize,
    end: usize,
    mut on_present: impl FnMut(usize, usize),
    mut on_absent: impl FnMut(usize, usize),
) {
    if start >= end {
        return;
    }
    if COMMIT_TITLE_MEMORY.get().is_none() {
        on_present(start, end);
        return;
    }
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    let mut absent_blocks: Vec<(usize, usize)> = Vec::new();
    for_each_shard_span(first, last, |shard, a, b| {
        let g = guards_read_shard(shard);
        if !g.absent.overlaps(a, b) {
            return;
        }
        g.absent.for_each_overlap(a, b, |oa, ob| {
            match absent_blocks.last_mut() {
                Some(prev) if prev.1 == oa => prev.1 = ob,
                _ => absent_blocks.push((oa, ob)),
            }
        });
    });
    if absent_blocks.is_empty() {
        on_present(start, end);
        return;
    }
    let mut cur = start;
    for (ab_a, ab_b) in absent_blocks {
        let lo = ab_a.saturating_mul(BLOCK).max(start);
        let hi = ab_b.saturating_mul(BLOCK).min(end);
        if lo > cur {
            on_present(cur, lo);
        }
        if hi > lo {
            on_absent(lo, hi);
        }
        cur = cur.max(hi);
    }
    if cur < end {
        on_present(cur, end);
    }
}

#[inline]
fn guards_read_shard(shard: usize) -> std::sync::RwLockReadGuard<'static, Guards> {
    GUARDS[shard].read().unwrap_or_else(|e| e.into_inner())
}

#[inline]
fn guards_write_shard(shard: usize) -> std::sync::RwLockWriteGuard<'static, Guards> {
    GUARDS[shard].write().unwrap_or_else(|e| e.into_inner())
}

fn for_each_shard_span(first: usize, last: usize, mut f: impl FnMut(usize, usize, usize)) {
    let mut at = first;
    while at < last {
        let shard = shard_of(at);
        let next_boundary = (at >> SHARD_SHIFT)
            .checked_add(1)
            .and_then(|s| s.checked_shl(SHARD_SHIFT as u32))
            .unwrap_or(last);
        let end = next_boundary.min(last);
        f(shard, at, end);
        at = end;
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    let (mut guarded, hot) = guard_with_hot(start, end)?;
    if !hot.is_empty() {
        guarded.extend(hot);
        guarded.sort_unstable();
        guarded.dedup();
    }
    Some(guarded)
}

enum GuardHot {
    HotOnly(Vec<(usize, u64)>),
    Guarded {
        guarded: Vec<usize>,
        hot: Vec<usize>,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
fn guard_with_hot(start: usize, end: usize) -> Option<(Vec<usize>, Vec<usize>)> {
    match guard_with_hot_fast(start, end)?.0 {
        GuardHot::HotOnly(pairs) => Some((Vec::new(), pairs.into_iter().map(|(p, _)| p).collect())),
        GuardHot::Guarded { guarded, hot } => Some((guarded, hot)),
    }
}

fn guard_with_hot_fast(
    start: usize,
    end: usize,
) -> Option<(GuardHot, Option<[u64; CLEAN_EPOCH_SHARDS]>)> {
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ask = *ASK.get_or_init(|| std::env::var_os("OATHKEEP_GUARD_ASK").is_some_and(|v| v == "1"));
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    if first >= last {
        return Some((GuardHot::HotOnly(Vec::new()), None));
    }
    let frame = crate::frame::current();
    // When HLE demand-commit hooks are installed, all `MEM_MAPPED` commits notify
    // `title_memory_committed`; only probe `g.absent` via `VirtualQuery` when no hook
    // is installed (e.g. unit tests calling `VirtualAlloc(MEM_COMMIT)` directly).
    let probe_absent = COMMIT_TITLE_MEMORY.get().is_none();
    let mut pre_gaps: Vec<(usize, usize)> = Vec::new();
    let mut slow_spans: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut fast_hot_pairs: Vec<(usize, u64)> = Vec::new();
    let mut span_count = 0usize;
    let sub_first = first >> SUB_SHIFT;
    let sub_last = (last - 1) >> SUB_SHIFT;
    let use_sub = (sub_last - sub_first) + 1 <= CLEAN_EPOCH_SHARDS;
    let mut clean_ok = !ask;
    let mut clean_epochs = [0u64; CLEAN_EPOCH_SHARDS];
    if !ask {
        #[cfg(feature = "diagnostics")]
        let t_read = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let mut fast_hot_n = 0u64;
        let mut known = false;
        for_each_shard_span(first, last, |shard, a, b| {
            let slot = span_count;
            span_count += 1;
            if !use_sub && slot >= CLEAN_EPOCH_SHARDS {
                clean_ok = false;
            }
            let g = guards_read_shard(shard);
            if g.clean.contains_range(a, b) {
                known = true;
                if clean_ok {
                    record_clean_epoch_span(
                        &mut clean_epochs,
                        use_sub,
                        sub_first,
                        slot,
                        shard,
                        a,
                        b,
                    );
                }
                return;
            }
            if g.accounted.contains_range(a, b) && (!probe_absent || !g.absent.overlaps(a, b)) {
                known = true;
                let has_partly = g.partly.overlaps(a, b);
                let has_hot = !g.hot.is_empty() && g.hot.range(a..b).next().is_some();
                if !has_hot && !has_partly {
                    if !probe_absent && clean_ok {
                        // When HLE demand-commit hooks are installed, every commit to an
                        // `absent` block bumps its sub/shard clean epoch, so a span that is
                        // purely `clean ∪ absent` is valid for lock-free clean-epoch caching.
                        record_clean_epoch_span(
                            &mut clean_epochs,
                            use_sub,
                            sub_first,
                            slot,
                            shard,
                            a,
                            b,
                        );
                    } else {
                        clean_ok = false;
                    }
                    return;
                }
                let start_len = fast_hot_pairs.len();
                let mut all_active = true;
                if has_hot {
                    for (&block, &until) in g.hot.range(a..b) {
                        if until > frame {
                            fast_hot_pairs.push((block, HOT_WRITE));
                        } else {
                            all_active = false;
                            break;
                        }
                    }
                }
                if all_active {
                    clean_ok = false;
                    #[cfg(feature = "diagnostics")]
                    {
                        fast_hot_n += (fast_hot_pairs.len() - start_len) as u64;
                    }
                    if has_partly {
                        let had_hot = fast_hot_pairs.len() > start_len;
                        g.partly.for_each_overlap(a, b, |pa, pb| {
                            fast_hot_pairs.extend((pa..pb).map(|block| (block, HOT_WRITE)));
                        });
                        if had_hot {
                            fast_hot_pairs[start_len..].sort_unstable_by_key(|&(block, _)| block);
                        }
                    }
                    return;
                }
                fast_hot_pairs.truncate(start_len);
            }
            if g.accounted.overlaps(a, b) || g.protected.overlaps(a, b) {
                known = true;
            }
            if view_guard() {
                pre_gaps.extend(g.accounted.gaps_in(a, b));
                if !g.hot.is_empty() {
                    for (&block, &until) in g.hot.range(a..b) {
                        if until <= frame {
                            match pre_gaps.last_mut() {
                                Some(last_gap) if last_gap.1 == block => last_gap.1 = block + 1,
                                _ => pre_gaps.push((block, block + 1)),
                            }
                        }
                    }
                }
            }
            slow_spans.push((slot, shard, a, b));
        });
        if slow_spans.is_empty() {
            #[cfg(feature = "diagnostics")]
            {
                let total_n = fast_hot_pairs.len() as u64;
                crate::copystat::add(crate::copystat::C::GuardReadFastN, 1);
                crate::copystat::add(
                    crate::copystat::C::GuardReadFastUs,
                    t_read.elapsed().as_micros() as u64,
                );
                if total_n > 0 {
                    crate::copystat::add(crate::copystat::C::GuardRetHotPages, fast_hot_n);
                    crate::copystat::add(
                        crate::copystat::C::GuardRetPartlyPages,
                        total_n.saturating_sub(fast_hot_n),
                    );
                }
            }
            let epochs =
                (clean_ok && (use_sub || span_count <= CLEAN_EPOCH_SHARDS)).then_some(clean_epochs);
            return Some((GuardHot::HotOnly(fast_hot_pairs), epochs));
        }
        if !known && !is_view(start, end) {
            return None;
        }
    } else {
        if !is_view(start, end) {
            return None;
        }
        for_each_shard_span(first, last, |shard, a, b| {
            let slot = span_count;
            span_count += 1;
            if view_guard() {
                let g = guards_read_shard(shard);
                pre_gaps.extend(g.accounted.gaps_in(a, b));
                if !g.hot.is_empty() {
                    for (&block, &until) in g.hot.range(a..b) {
                        if until <= frame {
                            match pre_gaps.last_mut() {
                                Some(last_gap) if last_gap.1 == block => last_gap.1 = block + 1,
                                _ => pre_gaps.push((block, block + 1)),
                            }
                        }
                    }
                }
            }
            slow_spans.push((slot, shard, a, b));
        });
    }
    // Pre-query `inplace::VIEWS` OUTSIDE shard write locks so view-map mutations on other
    // threads never stall `GUARDS` readers behind `inplace::VIEWS.read()`.
    #[cfg(feature = "diagnostics")]
    let t_pre = std::time::Instant::now();
    let pre_aliases = if view_guard() && !pre_gaps.is_empty() {
        let seq = crate::inplace::views_seq();
        let mut all_blocks = Vec::new();
        let mut other_runs = Vec::new();
        let mut total_count = 0u64;
        for &(from, to) in &pre_gaps {
            let (all, others, count) = crate::inplace::alias_blocks_between(
                from.saturating_mul(BLOCK),
                to.saturating_mul(BLOCK),
                MANY_VIEWS,
            );
            if count > 0 {
                total_count += count;
                all_blocks.extend(all);
                other_runs.extend(others);
            }
        }
        Some((seq, pre_gaps, all_blocks, other_runs, total_count))
    } else {
        None
    };
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::GuardPreAliasUs,
        t_pre.elapsed().as_micros() as u64,
    );
    install();
    let mut hot_pages: Vec<usize> = fast_hot_pairs.into_iter().map(|(p, _)| p).collect();
    let mut written = Vec::new();
    let mut protected_runs = Vec::new();
    let mut already_aliased = Vec::new();
    for &(slot, shard, a, b) in &slow_spans {
        #[cfg(feature = "diagnostics")]
        let t_wlock = std::time::Instant::now();
        let mut g = guards_write_shard(shard);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::GuardWriteWaitUs,
            t_wlock.elapsed().as_micros() as u64,
        );
        if g.clean.contains_range(a, b) {
            if clean_ok && (use_sub || slot < CLEAN_EPOCH_SHARDS) {
                record_clean_epoch_span(
                    &mut clean_epochs,
                    use_sub,
                    sub_first,
                    slot,
                    shard,
                    a,
                    b,
                );
            }
            continue;
        }
        let span_hot_start = hot_pages.len();
        let mut expired_hot = Vec::new();
        if !g.hot.is_empty() {
            for (&block, &until) in g.hot.range(a..b) {
                if until > frame {
                    hot_pages.push(block);
                } else {
                    expired_hot.push(block);
                }
            }
        }
        if !expired_hot.is_empty() {
            for &block in &expired_hot {
                bump_sub_clean_block(block);
            }
            bump_shard_clean_epoch(shard);
        }
        for block in expired_hot {
            g.hot.remove(&block);
            g.accounted.remove(block);
            g.cool_count
                .entry(block)
                .and_modify(|c| *c = c.saturating_add(1).min(MAX_COOL_BACKOFF))
                .or_insert(1);
        }
        if probe_absent && g.absent.overlaps(a, b) {
            let mut newly_committed: Vec<(usize, usize)> = Vec::new();
            let mut uncommitted_until = a;
            g.absent.for_each_overlap(a, b, |oa, ob| {
                let mut block = oa;
                while block < ob {
                    if block >= uncommitted_until {
                        uncommitted_until = uncommitted_end(block);
                    }
                    if block < uncommitted_until {
                        block = uncommitted_until.min(ob);
                    } else {
                        match newly_committed.last_mut() {
                            Some(last_run) if last_run.1 == block => last_run.1 = block + 1,
                            _ => newly_committed.push((block, block + 1)),
                        }
                        block += 1;
                    }
                }
            });
            if !newly_committed.is_empty() {
                for &(na, nb) in &newly_committed {
                    bump_sub_clean_range(na, nb);
                }
                bump_shard_clean_epoch(shard);
            }
            for (na, nb) in newly_committed {
                g.absent.remove_range(na, nb);
                g.accounted.remove_range(na, nb);
            }
        }
        if g.partly.overlaps(a, b) {
            let had_hot = hot_pages.len() > span_hot_start;
            g.partly.for_each_overlap(a, b, |pa, pb| {
                hot_pages.extend(pa..pb);
            });
            if had_hot {
                hot_pages[span_hot_start..].sort_unstable();
            }
        }
        if g.accounted.contains_range(a, b) {
            if clean_ok && (use_sub || slot < CLEAN_EPOCH_SHARDS) && g.clean.contains_range(a, b) {
                record_clean_epoch_span(
                    &mut clean_epochs,
                    use_sub,
                    sub_first,
                    slot,
                    shard,
                    a,
                    b,
                );
            } else {
                clean_ok = false;
            }
            continue;
        }
        #[cfg(feature = "diagnostics")]
        let t_prot = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let written_before = written.len();
        for (gap_first, gap_last) in g.accounted.gaps_in(a, b) {
            written.extend(gap_first * BLOCK / PAGE..gap_last * BLOCK / PAGE);
            if !g.protected.overlaps(gap_first, gap_last) {
                protect_here(&mut g, gap_first, gap_last, &mut protected_runs);
            } else {
                let mut already = Vec::new();
                g.protected.for_each_overlap(gap_first, gap_last, |pa, pb| {
                    already.push((pa, pb));
                });
                for (pa, pb) in g.protected.gaps_in(gap_first, gap_last) {
                    protect_here(&mut g, pa, pb, &mut protected_runs);
                }
                for (pa, pb) in already {
                    if view_guard() && g.aliased.range(pa..pb).next().is_some() {
                        protected_runs.push((pa, pb));
                        already_aliased.push((pa, pb));
                    } else {
                        g.clean.insert_range(pa, pb);
                        g.accounted.insert_range(pa, pb);
                    }
                }
            }
        }
        if clean_ok && (use_sub || slot < CLEAN_EPOCH_SHARDS) && g.clean.contains_range(a, b) {
            record_clean_epoch_span(
                &mut clean_epochs,
                use_sub,
                sub_first,
                slot,
                shard,
                a,
                b,
            );
        } else {
            clean_ok = false;
        }
        #[cfg(feature = "diagnostics")]
        {
            crate::copystat::add(
                crate::copystat::C::GuardProtectUs,
                t_prot.elapsed().as_micros() as u64,
            );
            crate::copystat::add(
                crate::copystat::C::GuardRetGuardedPages,
                (written.len() - written_before) as u64,
            );
        }
    }
    if span_count > 1 && hot_pages.len() > 1 {
        hot_pages.sort_unstable();
    }
    if !protected_runs.is_empty() {
        if protected_runs.len() > 1 {
            protected_runs.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::with_capacity(protected_runs.len());
            for (pa, pb) in protected_runs {
                match merged.last_mut() {
                    Some(last) if pa <= last.1 => last.1 = last.1.max(pb),
                    _ => merged.push((pa, pb)),
                }
            }
            protected_runs = merged;
        }
        #[cfg(feature = "diagnostics")]
        let t_alias = std::time::Instant::now();
        let reused = if let Some((seq, ref covered, all_blocks, other_runs, total_count)) =
            pre_aliases
            && crate::inplace::views_seq() == seq
            && protected_runs
                .iter()
                .all(|&(a, b)| covered.iter().any(|&(ca, cb)| ca <= a && b <= cb))
        {
            if total_count > 0 {
                apply_alias_protection_sharded(all_blocks, other_runs, total_count);
            }
            true
        } else {
            false
        };
        if !reused {
            protect_aliases_sharded(&protected_runs);
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::GuardAliasApplyUs,
            t_alias.elapsed().as_micros() as u64,
        );
    }
    if !already_aliased.is_empty() {
        for (aa, ab) in already_aliased {
            for_each_shard_span(aa, ab, |shard, sa, sb| {
                let mut g = guards_write_shard(shard);
                let mut still_prot = Vec::new();
                g.protected.for_each_overlap(sa, sb, |pa, pb| {
                    still_prot.push((pa, pb));
                });
                for (pa, pb) in still_prot {
                    g.clean.insert_range(pa, pb);
                    g.accounted.insert_range(pa, pb);
                }
            });
        }
    }
    let epochs =
        (clean_ok && (use_sub || span_count <= CLEAN_EPOCH_SHARDS)).then_some(clean_epochs);
    if written.is_empty() {
        Some((
            GuardHot::HotOnly(hot_pages.into_iter().map(|p| (p, HOT_WRITE)).collect()),
            epochs,
        ))
    } else {
        Some((
            GuardHot::Guarded {
                guarded: written,
                hot: hot_pages,
            },
            epochs,
        ))
    }
}

fn is_view(start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let holes =
        *HOLES.get_or_init(|| std::env::var_os("OATHKEEP_GUARD_HOLES").is_none_or(|v| v != "0"));
    let mut at = start;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.Type == MEM_MAPPED {
            return true;
        }
        if info.State == MEM_COMMIT || !holes {
            return false;
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at || next >= end {
            return true;
        }
        at = next;
    }
}

fn protect_aliases_sharded(runs: &[(usize, usize)]) {
    if !view_guard() || runs.is_empty() {
        return;
    }
    let mut all_blocks: Vec<usize> = Vec::new();
    let mut all_other_runs: Vec<(usize, usize)> = Vec::new();
    let mut total_count = 0u64;
    for &(from, to) in runs {
        let (all, other_runs, count) = crate::inplace::alias_blocks_between(
            from.saturating_mul(BLOCK),
            to.saturating_mul(BLOCK),
            MANY_VIEWS,
        );
        if count == 0 {
            continue;
        }
        total_count += count;
        all_blocks.extend(all);
        all_other_runs.extend(other_runs);
    }
    if total_count == 0 {
        return;
    }
    apply_alias_protection_sharded(all_blocks, all_other_runs, total_count);
}

fn apply_alias_protection_sharded(
    mut all_blocks: Vec<usize>,
    mut all_other_runs: Vec<(usize, usize)>,
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))] total_count: u64,
) {
    if !all_blocks.is_empty() {
        if all_blocks.len() > 1 {
            all_blocks.sort_unstable();
            all_blocks.dedup();
        }
        let mut i = 0;
        while i < all_blocks.len() {
            let shard = shard_of(all_blocks[i]);
            let mut k = i + 1;
            while k < all_blocks.len() && shard_of(all_blocks[k]) == shard {
                k += 1;
            }
            let slice = &all_blocks[i..k];
            let needs_insert = {
                let g = guards_read_shard(shard);
                slice.iter().any(|alias| !g.aliased.contains(alias))
            };
            if needs_insert {
                let mut g = guards_write_shard(shard);
                for &alias in slice {
                    g.aliased.insert(alias);
                }
            }
            i = k;
        }
    }
    #[cfg(feature = "diagnostics")]
    ALIASES[0].fetch_add(total_count, std::sync::atomic::Ordering::Relaxed);
    if all_other_runs.len() > 1 {
        all_other_runs.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(all_other_runs.len());
        for (a, b) in all_other_runs {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        all_other_runs = merged;
    }
    for (a, b) in all_other_runs {
        for_each_shard_span(a, b, |shard, sa, sb| {
            let already_protected = {
                let g = guards_read_shard(shard);
                g.protected.contains_range(sa, sb)
            };
            if !already_protected {
                let mut g = guards_write_shard(shard);
                protect_alias_range(&mut g, sa, sb);
            }
        });
    }
}

pub(crate) const MANY_VIEWS: usize = 64;

fn view_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_VIEW_GUARD").is_none_or(|v| v != "0"))
}

#[cfg(feature = "diagnostics")]
pub(crate) static ALIASES: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

fn protect_alias_range(g: &mut Guards, from: usize, to: usize) {
    let frame = crate::frame::current();
    for (a, b) in g.protected.gaps_in(from, to) {
        let has_hot = g
            .hot
            .range(a..b)
            .any(|(_, &until)| until > frame);
        if has_hot {
            let mut run_start = a;
            for block in a..b {
                if g.hot.get(&block).is_some_and(|&until| until > frame) {
                    if run_start < block {
                        protect_alias_span(g, run_start, block);
                    }
                    run_start = block + 1;
                }
            }
            if run_start < b {
                protect_alias_span(g, run_start, b);
            }
            continue;
        }
        protect_alias_span(g, a, b);
    }
}

fn protect_alias_span(g: &mut Guards, a: usize, b: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READONLY, VirtualProtect, VirtualQuery,
    };
    let mut old = 0;
    // SAFETY: title view memory; only its protection changes.
    if unsafe {
        VirtualProtect(
            (a * BLOCK) as *const _,
            (b - a) * BLOCK,
            PAGE_READONLY,
            &mut old,
        ) != 0
    } {
        g.protected.insert_range(a, b);
        return;
    }
    let mut block = a;
    while block < b {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let queried = unsafe {
            VirtualQuery(
                (block * BLOCK) as *const _,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 || info.RegionSize == 0 {
            block += 1;
            continue;
        }
        let region_end = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        let next = (region_end / BLOCK).max(block + 1).min(b);
        if info.State == MEM_COMMIT {
            // SAFETY: committed contiguous region within `[block, next)`.
            if unsafe {
                VirtualProtect(
                    (block * BLOCK) as *const _,
                    (next - block) * BLOCK,
                    PAGE_READONLY,
                    &mut old,
                ) != 0
            } {
                g.protected.insert_range(block, next);
            } else {
                for single in block..next {
                    if unsafe {
                        VirtualProtect((single * BLOCK) as *const _, BLOCK, PAGE_READONLY, &mut old)
                            != 0
                    } {
                        g.protected.insert(single);
                    }
                }
            }
        }
        block = next;
    }
}

fn protect_here(
    g: &mut Guards,
    from: usize,
    to: usize,
    protected_runs: &mut Vec<(usize, usize)>,
) {
    protect_range(g, from, to, true, protected_runs);
}

fn protect_range(
    g: &mut Guards,
    from: usize,
    to: usize,
    mark_clean: bool,
    protected_runs: &mut Vec<(usize, usize)>,
) {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_READONLY, VirtualProtect,
        VirtualQuery,
    };
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let mut note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.insert_range(from, to);
        if mark_clean {
            g.clean.insert_range(from, to);
            g.accounted.insert_range(from, to);
        }
        protected_runs.push((from, to));
    };
    if set(from, to) {
        note(g, from, to);
        return;
    }
    let mut block = from;
    while block < to {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
        let raw_end = if n != 0 {
            ((info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK).min(to)
        } else {
            block
        };
        if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
            if raw_end > block {
                g.absent.insert_range(block, raw_end);
                g.accounted.insert_range(block, raw_end);
                block = raw_end;
            } else {
                g.partly.insert(block);
                g.accounted.insert(block);
                block += 1;
            }
            continue;
        }
        let reg_end = raw_end.max(block + 1);
        if set(block, reg_end) {
            note(g, block, reg_end);
            block = reg_end;
            continue;
        }
        for single in block..reg_end {
            if set(single, single + 1) {
                note(g, single, single + 1);
            } else if single < uncommitted_end(single) {
                g.absent.insert(single);
                g.accounted.insert(single);
            } else {
                g.partly.insert(single);
                g.accounted.insert(single);
            }
        }
        block = reg_end;
    }
}

/// Removes quiet blocks from `g.hot` and re-arms their page guards (`PAGE_READONLY`)
/// without marking them `g.clean` yet so the next `guard_with_hot` check catches any
/// write that raced before `VirtualProtect` and then promotes them to `g.clean` with
/// zero extra syscalls.
pub(crate) fn cool_hot(blocks: &[usize]) -> usize {
    if blocks.is_empty() {
        return 0;
    }
    let mut sorted = blocks.to_vec();
    if sorted.len() > 1 {
        sorted.sort_unstable();
        sorted.dedup();
    }
    let mut total_cooled = 0usize;
    let mut protected_runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let shard = shard_of(sorted[i]);
        let mut k = i + 1;
        while k < sorted.len() && shard_of(sorted[k]) == shard {
            k += 1;
        }
        let slice = &sorted[i..k];
        i = k;
        {
            let g = guards_read_shard(shard);
            if g.hot.is_empty() || !slice.iter().any(|b| g.hot.contains_key(b)) {
                continue;
            }
        }
        let mut g = guards_write_shard(shard);
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for &block in slice {
            if g.hot.remove(&block).is_none() {
                continue;
            }
            total_cooled += 1;
            g.accounted.remove(block);
            g.recent_fault.remove(&block);
            g.cool_count
                .entry(block)
                .and_modify(|c| *c = c.saturating_add(1).min(MAX_COOL_BACKOFF))
                .or_insert(1);
            match runs.last_mut() {
                Some(last) if last.1 == block => last.1 = block + 1,
                _ => runs.push((block, block + 1)),
            }
        }
        if !runs.is_empty() {
            for &(from, to) in &runs {
                bump_sub_clean_range(from, to);
            }
            bump_shard_clean_epoch(shard);
        }
        for (from, to) in runs {
            for (a, b) in g.protected.gaps_in(from, to) {
                protect_range(&mut g, a, b, false, &mut protected_runs);
            }
        }
    }
    if !protected_runs.is_empty() {
        protect_aliases_sharded(&protected_runs);
    }
    total_cooled
}

fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

fn open_and_aliases_block_spans_sharded(
    first: usize,
    last: usize,
) -> (Vec<(usize, usize)>, Vec<usize>) {
    if first >= last {
        return (Vec::new(), Vec::new());
    }
    let mut opened = Vec::new();
    let mut aliased = Vec::new();
    for_each_shard_span(first, last, |shard, a, b| {
        let mut g = guards_write_shard(shard);
        let opened_here = open_here_block_spans(shard, &mut g, a, b);
        if !opened_here.is_empty() && !g.aliased.is_empty() {
            for &(oa, ob) in &opened_here {
                aliased.extend(g.aliased.range(oa..ob).copied());
            }
        }
        opened.extend(opened_here);
    });
    if aliased.is_empty() {
        return (opened, Vec::new());
    }
    let (all, other_runs) = crate::inplace::alias_spans_of_blocks(&aliased);
    for (a, b) in other_runs {
        for_each_shard_span(a, b, |shard, sa, sb| {
            let mut g = guards_write_shard(shard);
            let _alias_dirtied = dirty_alias_block_spans(shard, &mut g, sa, sb);
            #[cfg(feature = "diagnostics")]
            {
                let count: u64 = _alias_dirtied.iter().map(|&(lo, hi)| (hi - lo) as u64).sum();
                if count > 0 {
                    ALIASES[1].fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    }
    (opened, all)
}

/// Marks alias blocks dirty (`clean` and `accounted` removed, sub-clean epochs bumped)
/// while leaving their `PAGE_READONLY` hardware protection (`g.protected`) intact.
/// Because the write occurred through a different virtual alias, untouched aliases stay
/// read-only so neither the fault nor the next `apply_alias_protection_sharded` needs
/// `VirtualProtect` TLB shootdowns on them.
fn dirty_alias_block_spans(
    shard: usize,
    g: &mut Guards,
    first: usize,
    last: usize,
) -> Vec<(usize, usize)> {
    let clean_removed = g.clean.remove_range(first, last);
    if !clean_removed.is_empty() {
        for &(a, b) in &clean_removed {
            bump_sub_clean_range(a, b);
        }
        bump_shard_clean_epoch(shard);
        for &(a, b) in &clean_removed {
            g.accounted.remove_range(a, b);
        }
    }
    clean_removed
}

fn open_fault_sharded(at: usize) -> bool {
    let fault_block = at / BLOCK;
    let (opened_spans, aliases) =
        open_and_aliases_block_spans_sharded(fault_block, fault_block + 1);
    if opened_spans.is_empty() {
        return false;
    }
    record_written_range(
        fault_block.saturating_mul(BLOCK),
        fault_block.saturating_add(1).saturating_mul(BLOCK),
    );
    for &alias in &aliases {
        if alias != fault_block {
            record_written_range(
                alias.saturating_mul(BLOCK),
                alias.saturating_add(1).saturating_mul(BLOCK),
            );
        }
    }
    if hot_enabled() {
        let frame = crate::frame::current();
        let threshold = hot_faults();
        let fault_shard = shard_of(fault_block);
        let (mut become_hot, block_shift) = {
            let mut g = guards_write_shard(fault_shard);
            let hit = g.record_fault_and_check_hot(fault_block, frame, threshold);
            let shift = g
                .cool_count
                .get(&fault_block)
                .copied()
                .unwrap_or(0)
                .min(MAX_COOL_BACKOFF);
            (hit, shift)
        };
        for &alias in &aliases {
            if alias != fault_block {
                let mut g = guards_write_shard(shard_of(alias));
                if g.record_fault_and_check_hot(alias, frame, threshold) {
                    become_hot = true;
                }
            }
        }
        if become_hot {
            let base_hot = hot_frames();
            {
                let mut g = guards_write_shard(fault_shard);
                let until = frame.saturating_add(base_hot.saturating_mul(1u64 << block_shift));
                g.hot.insert(fault_block, until);
                g.accounted.insert(fault_block);
                bump_sub_clean_block(fault_block);
                bump_shard_clean_epoch(fault_shard);
            }
            for alias in aliases {
                if alias != fault_block {
                    let alias_shard = shard_of(alias);
                    let mut g = guards_write_shard(alias_shard);
                    if !g.absent.contains(alias)
                        && !g.clean.contains(alias)
                        && !g.partly.contains(alias)
                    {
                        if g.protected.contains(alias) {
                            let _ = open_here_block_spans(alias_shard, &mut g, alias, alias + 1);
                        }
                        let alias_shift = g
                            .cool_count
                            .get(&alias)
                            .copied()
                            .unwrap_or(0)
                            .max(block_shift)
                            .min(MAX_COOL_BACKOFF);
                        let alias_until =
                            frame.saturating_add(base_hot.saturating_mul(1u64 << alias_shift));
                        g.hot.insert(alias, alias_until);
                        g.accounted.insert(alias);
                        bump_sub_clean_block(alias);
                        bump_shard_clean_epoch(alias_shard);
                    }
                }
            }
        }
    }
    true
}

fn open_here_block_spans(
    shard: usize,
    g: &mut Guards,
    first: usize,
    last: usize,
) -> Vec<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let spans = g.protected.remove_range(first, last);
    if spans.is_empty() {
        return spans;
    }
    let clean_removed = g.clean.remove_range(first, last);
    if !clean_removed.is_empty() {
        for &(a, b) in &clean_removed {
            bump_sub_clean_range(a, b);
        }
        bump_shard_clean_epoch(shard);
    }
    for (a, b) in clean_removed {
        g.accounted.remove_range(a, b);
    }
    for &(a, b) in &spans {
        let mut old = 0;
        // SAFETY: blocks we made read-only.
        if unsafe {
            VirtualProtect(
                (a * BLOCK) as *const _,
                (b - a) * BLOCK,
                PAGE_READWRITE,
                &mut old,
            )
        } == 0
        {
            for block in a..b {
                // SAFETY: fallback per block if the span crosses a view boundary.
                unsafe {
                    VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old);
                }
            }
        }
    }
    spans
}

pub(crate) fn commit_title_memory(at: usize, len: usize) {
    if len > 0
        && let Some(commit) = COMMIT_TITLE_MEMORY.get()
    {
        commit(at, len);
    }
}

static COMMIT_TITLE_MEMORY: std::sync::OnceLock<fn(usize, usize)> = std::sync::OnceLock::new();
static DEMAND_RESERVED_END: std::sync::OnceLock<fn(usize) -> Option<usize>> =
    std::sync::OnceLock::new();

pub fn set_commit_title_memory(hook: fn(usize, usize)) {
    let _ = COMMIT_TITLE_MEMORY.set(hook);
}

pub fn set_demand_reserved_end(hook: fn(usize) -> Option<usize>) {
    let _ = DEMAND_RESERVED_END.set(hook);
}

pub(crate) fn is_demand_reserved(at: usize) -> bool {
    DEMAND_RESERVED_END
        .get()
        .is_some_and(|hook| hook(at).is_some())
}

pub fn title_memory_committed(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    record_written_range(at, at.saturating_add(len));
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    for_each_shard_span(first, last, |shard, a, b| {
        {
            let g = guards_read_shard(shard);
            if !g.absent.overlaps(a, b) && !g.partly.overlaps(a, b) {
                return;
            }
        }
        let mut g = guards_write_shard(shard);
        let removed_absent = g.absent.remove_range(a, b);
        for (ra, rb) in &removed_absent {
            g.accounted.remove_range(*ra, *rb);
        }
        let removed_partly = g.partly.remove_range(a, b);
        for (ra, rb) in &removed_partly {
            g.accounted.remove_range(*ra, *rb);
        }
        if !removed_absent.is_empty() || !removed_partly.is_empty() {
            for &(ra, rb) in removed_absent.iter().chain(removed_partly.iter()) {
                bump_sub_clean_range(ra, rb);
            }
            bump_shard_clean_epoch(shard);
        }
    });
}

fn promote_opened_hot_sharded(opened_spans: &[(usize, usize)]) {
    if !hot_enabled() || opened_spans.is_empty() {
        return;
    }
    let frame = crate::frame::current();
    let base_hot = hot_frames();
    let threshold = hot_faults();
    for &(span_a, span_b) in opened_spans {
        for_each_shard_span(span_a, span_b, |shard, a, b| {
            let mut g = guards_write_shard(shard);
            let mut promoted = false;
            for block in a..b {
                if g.absent.contains(block)
                    || g.clean.contains(block)
                    || g.partly.contains(block)
                {
                    continue;
                }
                if g.record_fault_and_check_hot(block, frame, threshold) {
                    let shift = g
                        .cool_count
                        .get(&block)
                        .copied()
                        .unwrap_or(0)
                        .min(MAX_COOL_BACKOFF);
                    let until = frame.saturating_add(base_hot.saturating_mul(1u64 << shift));
                    g.hot.insert(block, until);
                    g.accounted.insert(block);
                    bump_sub_clean_block(block);
                    promoted = true;
                }
            }
            if promoted {
                bump_shard_clean_epoch(shard);
            }
        });
    }
}

fn range_needs_open(first: usize, last: usize) -> bool {
    let mut needs = false;
    for_each_shard_span(first, last, |shard, a, b| {
        if needs {
            return;
        }
        let g = guards_read_shard(shard);
        if g.protected.overlaps(a, b) {
            needs = true;
        }
    });
    needs
}

pub fn writing(at: usize, len: usize) {
    if len > 0 {
        record_written_range(at, at.saturating_add(len));
        if let Some(commit) = COMMIT_TITLE_MEMORY.get() {
            commit(at, len);
        }
    }
    let end = at.saturating_add(len);
    if at < end {
        let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
        if range_needs_open(first, last) {
            let _ = open_and_aliases_block_spans_sharded(first, last);
        }
    }
    crate::descriptor::host::remapped(at, len);
}

/// Fast path for deferred `Step::Copy` writes: records the write range and opens any
/// page guards, deferring `COMMIT_TITLE_MEMORY` to the rare unmapped-page fallback and
/// letting the caller batch `descriptor::host::remapped_batch`.
pub(crate) fn writing_copy(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    record_written_range(at, end);
    let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
    if range_needs_open(first, last) {
        let _ = open_and_aliases_block_spans_sharded(first, last);
    }
}

/// Opens guarded blocks for GPU-to-title readback merges and promotes repeatedly
/// written blocks (within `HOT_WINDOW_FRAMES`) into `g.hot` + `g.accounted` so
/// active GPU writebacks avoid `VirtualProtect` ping-pong without flooding `g.hot`.
/// Does not call `descriptor::host::remapped` (card buffer writebacks are not texture remaps).
pub(crate) fn writing_gpu_back(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    record_written_range(at, end);
    if let Some(commit) = COMMIT_TITLE_MEMORY.get() {
        commit(at, len);
    }
    let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
    if range_needs_open(first, last) {
        let (opened_spans, _) = open_and_aliases_block_spans_sharded(first, last);
        promote_opened_hot_sharded(&opened_spans);
    }
}

pub(crate) fn page_writes() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("OATHKEEP_PAGE_WRITES")
            .as_deref()
            .map(str::trim)
        {
            Ok("0") => 0,
            Ok("guards") => 1,
            Ok("slots") => 2,
            Ok("all") => 7,
            _ => 3,
        }
    })
}

pub(crate) static WRITTEN: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

pub fn title_memory_written(at: usize, len: usize, pages: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if len == 0 {
        return;
    }
    record_written_range(at, at.saturating_add(len));
    let mode = page_writes();
    let end = at.saturating_add(len);
    if pages {
        WRITTEN[0].fetch_add(1, Relaxed);
        WRITTEN[1].fetch_add(len as u64, Relaxed);
    } else {
        WRITTEN[3].fetch_add(1, Relaxed);
        if mode & 4 == 0 {
            return;
        }
    }
    {
        let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
        let mut guarded = 0u64;
        for_each_shard_span(first, last, |shard, a, b| {
            guarded += guards_read_shard(shard).protected.count_in(a, b);
        });
        if pages {
            WRITTEN[2].fetch_add(guarded, Relaxed);
        }
        if mode & 1 != 0 && guarded != 0 {
            let _ = open_and_aliases_block_spans_sharded(first, last);
        }
    }
    crate::descriptor::host::page_written(at, len);
}

pub(crate) fn views_mapped(section: usize, offset: u64, len: usize) {
    if !view_guard() {
        return;
    }
    let ranges = crate::inplace::views_of_section(section, offset, len);
    if ranges.is_empty() {
        return;
    }
    for &(at, n) in &ranges {
        let end = at.saturating_add(n);
        if at >= end {
            continue;
        }
        let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
        for_each_shard_span(first, last, |shard, a, b| {
            let needs_open = guards_read_shard(shard).protected.overlaps(a, b);
            if !needs_open {
                return;
            }
            let mut g = guards_write_shard(shard);
            let opened_spans = open_here_block_spans(shard, &mut g, a, b);
            if !opened_spans.is_empty() {
                #[cfg(feature = "diagnostics")]
                ALIASES[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
}

static MAPPING: std::sync::RwLock<()> = std::sync::RwLock::new(());
static REMAPPING_ACTIVE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static REMAPPING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    static READING_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static REMAPPING_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub struct Remapping(#[allow(dead_code)] std::sync::RwLockWriteGuard<'static, ()>);

impl Drop for Remapping {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        REMAPPING_SEQ.fetch_add(1, Ordering::Release);
        REMAPPING_ACTIVE.fetch_sub(1, Ordering::Release);
        REMAPPING_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

pub fn title_memory_remapping() -> Remapping {
    use std::sync::atomic::Ordering;
    let guard = MAPPING.write().unwrap_or_else(|e| e.into_inner());
    REMAPPING_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
    REMAPPING_ACTIVE.fetch_add(1, Ordering::Release);
    REMAPPING_SEQ.fetch_add(1, Ordering::Release);
    Remapping(guard)
}

pub struct Reading(#[allow(dead_code)] std::sync::RwLockReadGuard<'static, ()>);

impl Drop for Reading {
    fn drop(&mut self) {
        READING_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

pub fn title_memory_reading() -> Reading {
    let guard = MAPPING.read().unwrap_or_else(|e| e.into_inner());
    READING_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
    Reading(guard)
}

pub fn title_memory_reading_now() -> Option<Reading> {
    let guard = match MAPPING.try_read() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    READING_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
    Some(Reading(guard))
}

pub fn title_memory_remapping_seq() -> u64 {
    REMAPPING_SEQ.load(std::sync::atomic::Ordering::Acquire)
}

pub fn wait_if_title_memory_remapping() -> bool {
    if REMAPPING_ACTIVE.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return false;
    }
    if READING_DEPTH.with(|d| d.get() > 0) || REMAPPING_DEPTH.with(|d| d.get() > 0) {
        return false;
    }
    let _reading = title_memory_reading();
    true
}

pub fn title_memory_remapped(at: usize, len: usize) {
    bump_global_write_epoch();
    crate::descriptor::host::remapped(at, len);
    crate::inplace::views_gone(at, len);
    crate::inplace::title_memory_freeing(at, len);
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    if first >= last {
        return;
    }
    let has_journal = journal_read().written.range(first..last).next().is_some();
    if has_journal {
        let mut j = journal_write();
        let keys: Vec<usize> = j.written.range(first..last).map(|(&k, _)| k).collect();
        for page in keys {
            j.written.remove(&page);
        }
    }
    for_each_shard_span(first, last, |shard, a, b| {
        let has_any = {
            let g = guards_read_shard(shard);
            g.accounted.overlaps(a, b)
                || g.protected.overlaps(a, b)
                || g.aliased.range(a..b).next().is_some()
                || g.recent_fault.range(a..b).next().is_some()
                || g.cool_count.range(a..b).next().is_some()
        };
        if !has_any {
            return;
        }
        let mut g = guards_write_shard(shard);
        bump_sub_clean_range(a, b);
        bump_shard_clean_epoch(shard);
        g.absent.remove_range(a, b);
        g.partly.remove_range(a, b);
        g.protected.remove_range(a, b);
        g.clean.remove_range(a, b);
        g.accounted.remove_range(a, b);
        let aliased: Vec<usize> = g.aliased.range(a..b).copied().collect();
        for block in aliased {
            g.aliased.remove(&block);
        }
        let hot_keys: Vec<usize> = g.hot.range(a..b).map(|(&k, _)| k).collect();
        for block in hot_keys {
            g.hot.remove(&block);
        }
        let recent_keys: Vec<usize> = g.recent_fault.range(a..b).map(|(&k, _)| k).collect();
        for block in recent_keys {
            g.recent_fault.remove(&block);
        }
        let cool_keys: Vec<usize> = g.cool_count.range(a..b).map(|(&k, _)| k).collect();
        for block in cool_keys {
            g.cool_count.remove(&block);
        }
    });
}

fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        let block = at / BLOCK;
        let tracked = {
            let g = guards_read_shard(shard_of(block));
            g.protected.contains(block) || g.aliased.contains(&block)
        };
        if tracked && open_fault_sharded(at) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if writable(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

pub fn try_open_write(at: usize) -> bool {
    let block = at / BLOCK;
    let tracked = {
        let g = guards_read_shard(shard_of(block));
        g.protected.contains(block) || g.aliased.contains(&block)
    };
    if tracked && open_fault_sharded(at) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::GuardFaults, 1);
        return true;
    }
    writable(at)
}

fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == MEM_COMMIT
        && (info.Protect & PAGE_GUARD) == 0
        && (info.Protect & 0xff) == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some());
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        title_memory_remapped(view, 2 * BLOCK);
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
    }

    #[test]
    fn writes_in_partly_committed_view_blocks_are_reported() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                title_memory_remapped(self.0.Value as usize, SIZE);
                // SAFETY: our view is still mapped.
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        // SAFETY: a new reserved pagefile section. The view holds its own
        // reference after we close the handle, including on assertion failure.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            View(view)
        };
        let start = view.0.Value as usize;
        let commit = |at: usize, len| {
            // SAFETY: these pages belong to our reserved view.
            let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
            assert_eq!(committed as usize, at);
        };
        let early = start + BLOCK + 3 * KERNEL_PAGE;
        let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
        commit(start, BLOCK);
        commit(early, KERNEL_PAGE);
        let refresh = || guard(start, start + SIZE).unwrap();
        assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
        assert!(!refresh().contains(&(late / PAGE)));

        commit(late, KERNEL_PAGE);
        for at in [early, late] {
            assert!(writable(at));
            // SAFETY: each address is inside its committed read-write page.
            unsafe { (at as *mut u8).write_volatile(1) };
        }
        assert_eq!(refresh(), [early / PAGE, late / PAGE]);
    }

    #[test]
    fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of three; the middle block is mapped first.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert!(is_view(p, p + 3 * BLOCK));
        let middle = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert!(is_view(p, p + 3 * BLOCK));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        // SAFETY: inside the mapped middle block.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        let first = map(0);
        title_memory_remapped(p, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        let mut local = [0u8; 16];
        let at = local.as_mut_ptr() as usize;
        assert!(!is_view(at, at + local.len()));
        title_memory_remapped(p, 3 * BLOCK);
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
    }

    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        title_memory_remapped(p, 3 * BLOCK);
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
    }

    fn two_views(bytes: usize) -> (usize, usize, usize) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        // SAFETY: a fresh pagefile section, mapped twice; the views keep
        // the section alive and the handle is closed by the caller.
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            assert_ne!(one, 0);
            assert_ne!(two, 0);
            crate::inplace::title_memory_view(one, bytes, section as usize, 0);
            crate::inplace::title_memory_view(two, bytes, section as usize, 0);
            (section as usize, one, two)
        }
    }

    fn drop_views(section: usize, views: &[usize], bytes: usize) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for &view in views {
            title_memory_remapped(view, bytes);
            // SAFETY: a view of the section mapped by `two_views`.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                })
            };
        }
        // SAFETY: the section's last reference.
        unsafe { CloseHandle(section as *mut std::ffi::c_void) };
    }

    #[test]
    fn a_write_through_another_view_is_reported_at_the_guarded_one() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    #[test]
    fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        title_memory_remapped(other, SIZE);
        crate::inplace::title_memory_view(other, SIZE, section, 0);
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    #[test]
    fn a_write_told_at_one_view_is_reported_at_the_others() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        writing(other + BLOCK, BLOCK);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        writing(other + BLOCK / 2, BLOCK);
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        drop_views(section, &[guarded, other], SIZE);
    }

    #[test]
    fn a_write_is_reported_at_every_checked_view() {
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(
            guard(one, one + SIZE),
            Some(vec![pages(one, 0), pages(one, 1)])
        );
        assert_eq!(
            guard(two, two + SIZE),
            Some(vec![pages(two, 0), pages(two, 1)]),
            "the second view's first check"
        );
        assert_eq!(
            guard(one, one + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        assert_eq!(
            guard(two, two + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((two + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 1)]));
        assert_eq!(
            guard(two, two + SIZE),
            Some(vec![pages(two, 1)]),
            "written through this view, checked second"
        );
        // SAFETY: the first view, as above.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0)]));
        assert_eq!(
            guard(one, one + SIZE),
            Some(vec![pages(one, 0)]),
            "written through this view, checked second"
        );
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
        drop_views(section, &[one, two], SIZE);
    }

    #[test]
    fn a_page_of_many_views_is_guarded_one_view_at_a_time() {
        let (section, one, two) = two_views(BLOCK);
        let filler = one + (1 << 45);
        for n in 0..MANY_VIEWS {
            crate::inplace::title_memory_view(filler + n * BLOCK, BLOCK, section, 0);
        }
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        assert!(writable(two), "another view of it is left alone");
        // SAFETY: the guarded view, opened by the handler.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        title_memory_remapped(filler, MANY_VIEWS * BLOCK);
        drop_views(section, &[one, two], BLOCK);
    }

    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }

    #[test]
    fn interval_set_merges_splits_and_finds_gaps() {
        let mut set = IntervalSet::new();
        assert!(!set.contains(5));
        assert!(!set.contains_range(5, 8));
        assert_eq!(set.gaps_in(5, 8), vec![(5, 8)]);
        set.insert_range(10, 20);
        set.insert_range(20, 30);
        set.insert_range(5, 10);
        assert!(set.contains_range(5, 30));
        assert!(!set.contains_range(4, 30));
        assert!(!set.contains_range(5, 31));
        assert_eq!(set.count_in(0, 40), 25);
        assert_eq!(set.gaps_in(5, 30), Vec::<(usize, usize)>::new());
        assert_eq!(set.remove_range(12, 18), vec![(12, 18)]);
        assert!(set.contains_range(5, 12));
        assert!(set.contains_range(18, 30));
        assert!(!set.contains(15));
        assert_eq!(set.gaps_in(4, 32), vec![(4, 5), (12, 18), (30, 32)]);
        assert_eq!(set.count_in(10, 20), 4);
        assert_eq!(set.remove_range(10, 25), vec![(10, 12), (18, 25)]);
        assert_eq!(set.count_in(0, 40), 10);

        let mut overlaps = Vec::new();
        set.for_each_overlap(7, 28, |a, b| overlaps.push((a, b)));
        assert_eq!(overlaps, vec![(7, 10), (25, 28)]);
    }

    #[test]
    fn accounted_invariant_and_writes_guarded_since_fast_paths() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a dedicated 3-block pagefile view owned by this test.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 3 * BLOCK);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            view
        };
        let start = view.Value as usize;
        let end = start + 3 * BLOCK;
        let b0 = start / BLOCK;
        let b1 = b0 + 1;
        let b2 = b0 + 2;

        // Initial guard with since=u64::MAX protects blocks and skips journal scan.
        let (ts0, w0) = writes_guarded_since(start, end, u64::MAX).unwrap();
        assert!(w0.is_empty());
        for b in b0..=b2 {
            let g = guards_read_shard(shard_of(b));
            assert!(g.clean.contains(b));
            assert!(g.accounted.contains(b));
        }

        // Subsequent check with since=ts0 returns empty immediately.
        let (ts1, w1) = writes_guarded_since(start, end, ts0).unwrap();
        assert!(ts1 >= ts0);
        assert!(w1.is_empty());

        // Fault block 0 once, then re-guard with since=ts1 -> fast path returns only b0.
        // SAFETY: committed mapped view memory.
        unsafe { (start as *mut u8).write_volatile(1) };
        let (ts2, w2) = writes_guarded_since(start, end, ts1).unwrap();
        assert!(ts2 > ts1);
        assert_eq!(w2, vec![(b0, ts2)]);

        // Fault block 0 up to `hot_faults()` times in the same frame -> enters hot cooldown;
        // accounted still covers b0..=b2.
        let mut last_ts = ts2;
        for round in 2..=hot_faults() {
            // SAFETY: committed mapped view memory.
            unsafe { (start as *mut u8).write_volatile(round as u8) };
            let (ts_next, w_next) = writes_guarded_since(start, end, last_ts).unwrap();
            last_ts = ts_next;
            if round < hot_faults() {
                assert_eq!(w_next, vec![(b0, ts_next)]);
            } else {
                assert_eq!(w_next, vec![(b0, HOT_WRITE)]);
            }
        }
        let ts3 = last_ts;
        {
            let g = guards_read_shard(shard_of(b0));
            assert!(!g.clean.contains(b0));
            assert!(g.hot.contains_key(&b0));
            assert!(g.accounted.contains(b0));
        }
        for b in b1..=b2 {
            let g = guards_read_shard(shard_of(b));
            assert!(g.clean.contains(b));
            assert!(g.accounted.contains(b));
        }

        // Fast path with since=ts3 still preserves HOT_WRITE for b0.
        let (ts4, w4) = writes_guarded_since(start, end, ts3).unwrap();
        assert!(ts4 >= ts3);
        assert_eq!(w4, vec![(b0, HOT_WRITE)]);

        // `writing()` on hot block b0 stays open without disturbing b1..=b2;
        // `writing()` on protected block b2 opens b2 and removes it from clean & accounted.
        writing(start, 16);
        for b in b1..=b2 {
            assert!(guards_read_shard(shard_of(b)).clean.contains(b));
        }
        writing(start + 2 * BLOCK, 16);
        {
            let g = guards_read_shard(shard_of(b2));
            assert!(!g.clean.contains(b2));
            assert!(!g.accounted.contains(b2));
        }
        for b in b0..b2 {
            assert!(guards_read_shard(shard_of(b)).accounted.contains(b));
        }

        // Remap cleans all guard sets including accounted.
        title_memory_remapped(start, 3 * BLOCK);
        for b in b0..=b2 {
            let g = guards_read_shard(shard_of(b));
            assert!(!g.accounted.contains(b));
            assert!(!g.clean.contains(b));
            assert!(!g.protected.contains(b));
            assert!(!g.hot.contains_key(&b));
        }
        // SAFETY: unmap our test view.
        unsafe { UnmapViewOfFile(view) };
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::{title_memory_reading, title_memory_remapping};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn reads_in_place_and_remappings_wait_for_each_other() {
        let changed = Arc::new(AtomicBool::new(false));
        let reading = title_memory_reading();
        let (began, wait) = mpsc::channel();
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _changing = title_memory_remapping();
                changed.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !changed.load(Ordering::SeqCst),
            "the mapping changed under a read in place"
        );
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
        let read = Arc::new(AtomicBool::new(false));
        let changing = title_memory_remapping();
        let (began, wait) = mpsc::channel();
        let reader = {
            let read = read.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _reading = title_memory_reading();
                read.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !read.load(Ordering::SeqCst),
            "a read in place began during a change of mapping"
        );
        drop(changing);
        reader.join().unwrap();
        assert!(read.load(Ordering::SeqCst));
    }

    #[test]
    fn merge_hot_preserves_sorted_order_and_marks_hot_writes() {
        use super::{HOT_WRITE, merge_hot};
        let mut out = vec![(10, 5), (20, 7), (30, 9)];
        merge_hot(&mut out, &[5, 20, 25, 40]);
        assert_eq!(
            out,
            vec![
                (5, HOT_WRITE),
                (10, 5),
                (20, HOT_WRITE),
                (25, HOT_WRITE),
                (30, 9),
                (40, HOT_WRITE),
            ]
        );
    }

    #[test]
    fn repeated_faults_enter_hot_cooldown_and_skip_reprotect() {
        use super::{
            BLOCK, guard_with_hot, guards_read_shard, shard_of, title_memory_remapped, writable,
        };
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        };
        // SAFETY: a dedicated 2-block pagefile view owned by this test.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            view
        };
        let start = view.Value as usize;
        let end = start + 2 * BLOCK;
        let block0 = start / BLOCK;
        let block1 = block0 + 1;
        // Initial guard protects both blocks.
        let (first_guarded, first_hot) = guard_with_hot(start, end).unwrap();
        assert_eq!(first_guarded, vec![block0, block1]);
        assert!(first_hot.is_empty());
        // First `hot_faults() - 1` writes in the same frame re-protect; the `hot_faults()`-th
        // write promotes block 0 into hot cooldown.
        let threshold = super::hot_faults();
        for round in 1..threshold {
            // SAFETY: committed mapped view memory.
            unsafe { (start as *mut u8).write_volatile(round as u8) };
            let (guarded, hot) = guard_with_hot(start, end).unwrap();
            assert_eq!(guarded, vec![block0]);
            assert!(hot.is_empty());
        }
        // SAFETY: committed mapped view memory.
        unsafe { (start as *mut u8).write_volatile(threshold as u8) };
        let (third_guarded, third_hot) = guard_with_hot(start, end).unwrap();
        assert!(third_guarded.is_empty());
        assert_eq!(third_hot, vec![block0]);
        // Block 0 stays writable (no reprotect) while block 1 stays clean.
        assert!(writable(start));
        // Remap clears hot state cleanly.
        title_memory_remapped(start, 2 * BLOCK);
        assert!(!guards_read_shard(shard_of(block0)).hot.contains_key(&block0));
        // SAFETY: unmap our test view.
        unsafe { UnmapViewOfFile(view) };
    }

    #[test]
    fn cool_hot_rearms_guard_and_clears_hot_state() {
        use super::{
            BLOCK, cool_hot, guard_with_hot, guards_read_shard, shard_of, title_memory_remapped,
            writable,
        };
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        };
        // SAFETY: a dedicated 3-block pagefile view owned by this test.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 3 * BLOCK);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            view
        };
        let start = view.Value as usize;
        let end = start + 3 * BLOCK;
        let b0 = start / BLOCK;
        let b1 = b0 + 1;
        let b2 = b0 + 2;

        assert_eq!(guard_with_hot(start, end), Some((vec![b0, b1, b2], vec![])));
        // Fault b0 and b1 `hot_faults()` times in the same frame to promote both to hot.
        let threshold = super::hot_faults();
        for round in 1..threshold {
            // SAFETY: committed mapped view memory.
            unsafe {
                (start as *mut u8).write_volatile(round as u8);
                ((start + BLOCK) as *mut u8).write_volatile(round as u8);
            }
            assert_eq!(guard_with_hot(start, end), Some((vec![b0, b1], vec![])));
        }
        // SAFETY: committed mapped view memory.
        unsafe {
            (start as *mut u8).write_volatile(threshold as u8);
            ((start + BLOCK) as *mut u8).write_volatile(threshold as u8);
        }
        assert_eq!(guard_with_hot(start, end), Some((vec![], vec![b0, b1])));
        assert!(writable(start));
        assert!(writable(start + BLOCK));

        // Cool only b0; b1 stays hot and b2 stays clean.
        assert_eq!(cool_hot(&[b0, b2]), 1);
        assert_eq!(cool_hot(&[b0]), 0);
        {
            let g0 = guards_read_shard(shard_of(b0));
            assert!(!g0.hot.contains_key(&b0));
            assert!(g0.protected.contains(b0));
            assert!(!g0.clean.contains(b0));
            assert!(!g0.accounted.contains(b0));
        }
        assert!(guards_read_shard(shard_of(b1)).hot.contains_key(&b1));
        {
            let g2 = guards_read_shard(shard_of(b2));
            assert!(g2.clean.contains(b2));
            assert!(g2.accounted.contains(b2));
        }
        assert!(!writable(start));
        assert!(writable(start + BLOCK));

        // Next guard reports b0 once (catching any raced write) and promotes b0 to clean.
        assert_eq!(guard_with_hot(start, end), Some((vec![b0], vec![b1])));
        {
            let g0 = guards_read_shard(shard_of(b0));
            assert!(g0.clean.contains(b0));
            assert!(g0.accounted.contains(b0));
        }
        // Subsequent guard skips b0 completely while b1 remains hot.
        assert_eq!(guard_with_hot(start, end), Some((vec![], vec![b1])));

        // Writing b0 once after cooling does not immediately re-enter hot (recent_fault cleared).
        // SAFETY: committed mapped view memory.
        unsafe { (start as *mut u8).write_volatile(3) };
        assert_eq!(guard_with_hot(start, end), Some((vec![b0], vec![b1])));
        assert!(!guards_read_shard(shard_of(b0)).hot.contains_key(&b0));
        assert_eq!(guard_with_hot(start, end), Some((vec![], vec![b1])));

        title_memory_remapped(start, 3 * BLOCK);
        // SAFETY: unmap our test view.
        unsafe { UnmapViewOfFile(view) };
    }

    #[test]
    fn cool_hot_coalesces_contiguous_runs_and_catches_raced_write() {
        use super::{
            BLOCK, cool_hot, guard_with_hot, guards_read_shard, shard_of, title_memory_remapped,
            writable,
        };
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        };
        // SAFETY: a dedicated 4-block pagefile view owned by this test.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (4 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 4 * BLOCK);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            view
        };
        let start = view.Value as usize;
        let end = start + 4 * BLOCK;
        let blocks: Vec<usize> = (0..4).map(|i| start / BLOCK + i).collect();

        assert_eq!(guard_with_hot(start, end), Some((blocks.clone(), vec![])));
        for round in 1..=super::hot_faults() {
            for i in 0..4 {
                // SAFETY: committed mapped view memory.
                unsafe { ((start + i * BLOCK) as *mut u8).write_volatile(round as u8) };
            }
            let _ = guard_with_hot(start, end).unwrap();
        }
        assert_eq!(guard_with_hot(start, end), Some((vec![], blocks.clone())));

        // Simulate a write racing right before cool_hot arms PAGE_READONLY.
        // SAFETY: committed mapped view memory, currently hot/writable.
        unsafe { ((start + 2 * BLOCK) as *mut u8).write_volatile(99) };
        assert_eq!(cool_hot(&[blocks[3], blocks[1], blocks[0], blocks[2], blocks[1]]), 4);
        for i in 0..4 {
            assert!(!writable(start + i * BLOCK));
            assert!(!guards_read_shard(shard_of(blocks[i])).hot.contains_key(&blocks[i]));
        }

        // Single transition verification reports all 4 blocks once, so the raced write is seen,
        // and the next check is completely clean.
        assert_eq!(guard_with_hot(start, end), Some((blocks.clone(), vec![])));
        assert_eq!(guard_with_hot(start, end), Some((vec![], vec![])));

        title_memory_remapped(start, 4 * BLOCK);
        // SAFETY: unmap our test view.
        unsafe { UnmapViewOfFile(view) };
    }

    #[test]
    fn cooling_backoff_doubles_hot_duration_and_resets_on_remap() {
        use super::{
            BLOCK, MAX_COOL_BACKOFF, cool_hot, guard_with_hot, guards_read_shard, hot_frames,
            shard_of, title_memory_remapped,
        };
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        };
        // SAFETY: a dedicated 1-block pagefile view owned by this test.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                BLOCK as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, BLOCK);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            view
        };
        let start = view.Value as usize;
        let end = start + BLOCK;
        let b0 = start / BLOCK;
        let s0 = shard_of(b0);
        let base_hot = hot_frames();

        let promote_to_hot = |val: u8| {
            let threshold = super::hot_faults();
            let _ = guard_with_hot(start, end).unwrap();
            for step in 0..threshold {
                // SAFETY: committed mapped view memory.
                unsafe { (start as *mut u8).write_volatile(val.wrapping_add(step as u8)) };
                let (guarded, hot) = guard_with_hot(start, end).unwrap();
                if step + 1 == threshold {
                    assert!(guarded.is_empty());
                    assert_eq!(hot, vec![b0]);
                }
            }
        };

        // First promotion: cool_count = 0 -> duration = 1x hot_frames.
        promote_to_hot(1);
        let frame = crate::frame::current();
        assert_eq!(
            guards_read_shard(s0).hot.get(&b0).copied(),
            Some(frame + base_hot)
        );
        assert_eq!(guards_read_shard(s0).cool_count.get(&b0).copied(), None);

        // Cool once -> cool_count = 1 -> next promotion lasts 2x hot_frames (MAX_COOL_BACKOFF=1).
        assert_eq!(cool_hot(&[b0]), 1);
        assert_eq!(guards_read_shard(s0).cool_count.get(&b0).copied(), Some(1));
        let _ = guard_with_hot(start, end).unwrap();
        promote_to_hot(10);
        let frame = crate::frame::current();
        assert_eq!(
            guards_read_shard(s0).hot.get(&b0).copied(),
            Some(frame + base_hot * 2)
        );

        // Repeated cooling saturates at MAX_COOL_BACKOFF.
        for step in 0..=MAX_COOL_BACKOFF {
            let _ = cool_hot(&[b0]);
            let _ = guard_with_hot(start, end).unwrap();
            promote_to_hot(30 + step * 8);
        }
        assert_eq!(
            guards_read_shard(s0).cool_count.get(&b0).copied(),
            Some(MAX_COOL_BACKOFF)
        );
        let frame = crate::frame::current();
        assert_eq!(
            guards_read_shard(s0).hot.get(&b0).copied(),
            Some(frame + base_hot * (1u64 << MAX_COOL_BACKOFF))
        );

        // Remap resets cool_count cleanly.
        title_memory_remapped(start, BLOCK);
        assert_eq!(guards_read_shard(s0).cool_count.get(&b0).copied(), None);
        // SAFETY: unmap our test view.
        unsafe { UnmapViewOfFile(view) };
    }

    #[test]
    fn writing_gpu_back_promotes_opened_blocks_to_hot_and_guarded_asked_fast_paths() {
        use super::{
            BLOCK, HOT_WRITE, guards_read_shard, journal_write, shard_of, title_memory_remapped,
            writes_guarded_since, writing_gpu_back,
        };
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        };
        // SAFETY: anonymous pagefile-backed section for testing.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (BLOCK * 2) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, BLOCK * 2);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            view
        };
        let start = view.Value as usize;
        let end = start + BLOCK * 2;
        let b0 = start / BLOCK;
        let b1 = b0 + 1;

        // Guard both blocks clean.
        let (since0, _) = writes_guarded_since(start, end, 0).unwrap();
        assert!(guards_read_shard(shard_of(b0)).protected.contains(b0));
        assert!(guards_read_shard(shard_of(b1)).protected.contains(b1));

        // First `hot_faults() - 1` GPU writebacks open b0 without flooding `g.hot`; next guard
        // re-arms it.
        let threshold = super::hot_faults();
        let mut since = since0;
        for _ in 1..threshold {
            writing_gpu_back(start, BLOCK);
            {
                let g0 = guards_read_shard(shard_of(b0));
                assert!(!g0.protected.contains(b0));
                assert!(!g0.hot.contains_key(&b0));
            }
            assert!(guards_read_shard(shard_of(b1)).protected.contains(b1));
            let (next_since, pages1) = writes_guarded_since(start, end, since).unwrap();
            assert_eq!(pages1, vec![(b0, next_since)]);
            assert!(guards_read_shard(shard_of(b0)).protected.contains(b0));
            since = next_since;
        }

        // `hot_faults()`-th GPU writeback in the same frame promotes b0 into `g.hot` +
        // `g.accounted` so the next `writes_guarded_since` does not re-protect it.
        writing_gpu_back(start, BLOCK);
        {
            let g0 = guards_read_shard(shard_of(b0));
            assert!(!g0.protected.contains(b0));
            assert!(g0.hot.contains_key(&b0));
            assert!(g0.accounted.contains(b0));
        }
        assert!(guards_read_shard(shard_of(b1)).protected.contains(b1));

        // Simulate `GetWriteWatch` on a private buffer bumping `j.asked` without bumping
        // `j.guarded_asked`: `writes_guarded_since` must still fast-path and return only hot `b0`.
        let bumped_since = {
            let mut j = journal_write();
            super::bump_journal_asked(&mut j, 5);
            since
        };
        let (now, pages) = writes_guarded_since(start, end, bumped_since).unwrap();
        assert!(now > bumped_since);
        assert_eq!(pages, vec![(b0, HOT_WRITE)]);
        // Block 0 must still be hot (not re-protected).
        {
            let g0 = guards_read_shard(shard_of(b0));
            assert!(!g0.protected.contains(b0));
            assert!(g0.hot.contains_key(&b0));
        }

        title_memory_remapped(start, BLOCK * 2);
        {
            let mut j = journal_write();
            j.written.remove(&b0);
            j.written.remove(&b1);
        }
        // SAFETY: unmap our test view.
        unsafe { UnmapViewOfFile(view) };
    }

    #[test]
    fn clean_guard_epochs_match_until_fault_or_remap() {
        use super::{
            BLOCK, guarded_clean_epochs_match, title_memory_remapped,
            writes_guarded_since_with_clean,
        };
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        };
        // SAFETY: dedicated 2-block pagefile-backed view.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (BLOCK * 2) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, BLOCK * 2);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            view
        };
        let start = view.Value as usize;
        let end = start + BLOCK * 2;
        let shard = super::shard_of(start / BLOCK);

        let (ts0, _, ep0) = writes_guarded_since_with_clean(start, end, u64::MAX).unwrap();
        let mut ts0 = ts0;
        let mut ep0 = ep0.expect("clean epochs returned for clean view");
        for _ in 0..8 {
            if guarded_clean_epochs_match(start, end, ep0, ts0) {
                break;
            }
            let (ts_retry, _, ep_retry) = writes_guarded_since_with_clean(start, end, ts0).unwrap();
            ts0 = ts_retry;
            ep0 = ep_retry.expect("clean epochs on retry");
        }
        assert!(guarded_clean_epochs_match(start, end, ep0, ts0));

        // Writing faults block 0 and immediately bumps its shard clean epoch.
        let before_fault = super::SHARD_CLEAN_EPOCH[shard].load(Ordering::Acquire);
        // SAFETY: committed mapped view memory.
        unsafe { (start as *mut u8).write_volatile(0x42) };
        let after_fault = super::SHARD_CLEAN_EPOCH[shard].load(Ordering::Acquire);
        assert!(after_fault > before_fault);
        assert!(!guarded_clean_epochs_match(start, end, ep0, ts0));

        // Re-guarding returns fresh epochs and bumps this shard's guarded_asked.
        let (mut ts1, w1, ep1) = writes_guarded_since_with_clean(start, end, ts0).unwrap();
        assert_eq!(w1.len(), 1);
        let mut ep1 = ep1.expect("re-protected clean epochs");
        assert!(!guarded_clean_epochs_match(start, end, ep1, ts0));

        // Ensure clean epochs settle if a parallel test touched this shard, then verify an
        // unrelated shard's guarded_asked bump does NOT invalidate this shard's clean epochs.
        for _ in 0..8 {
            if guarded_clean_epochs_match(start, end, ep1, ts1) {
                break;
            }
            let (ts_retry, _, ep_retry) = writes_guarded_since_with_clean(start, end, ts1).unwrap();
            ts1 = ts_retry;
            ep1 = ep_retry.expect("clean epochs on retry");
        }
        let other_shard = (shard + 3) & (super::GUARD_SHARDS - 1);
        super::SHARD_GUARDED_ASKED[other_shard].fetch_max(ts1 + 1, Ordering::Release);
        assert!(guarded_clean_epochs_match(start, end, ep1, ts1));

        // Remap bumps shard clean epoch again.
        let before_remap = super::SHARD_CLEAN_EPOCH[shard].load(Ordering::Acquire);
        title_memory_remapped(start, BLOCK * 2);
        assert!(super::SHARD_CLEAN_EPOCH[shard].load(Ordering::Acquire) > before_remap);
        assert!(!guarded_clean_epochs_match(start, end, ep1, ts1));
        // SAFETY: unmap our test view.
        unsafe { UnmapViewOfFile(view) };
    }

    static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn sub_clean_epoch_isolates_1mb_buckets_within_same_64mb_shard() {
        let _guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use super::{
            BLOCK, guarded_clean_epochs_match, title_memory_remapped,
            writes_guarded_since_with_clean,
        };
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        };
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (BLOCK * 32) as u32, // 2 MiB: two distinct 1 MiB sub-buckets
                std::ptr::null(),
            )
        };
        assert!(!mapping.is_null());
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, BLOCK * 32) };
        unsafe { CloseHandle(mapping) };
        assert!(!view.Value.is_null());

        let base = view.Value as usize;
        // Align to two distinct 1 MiB sub-buckets inside the mapped 2 MiB view.
        let sub0_start = base;
        let sub0_end = base + BLOCK;
        let sub1_start = base + BLOCK * 16; // +1 MiB
        let sub1_end = sub1_start + BLOCK;
        let same_shard = super::shard_of(sub0_start / BLOCK) == super::shard_of(sub1_start / BLOCK);
        assert_ne!(
            super::sub_of(sub0_start / BLOCK),
            super::sub_of(sub1_start / BLOCK)
        );

        let (mut ts0, _, ep0_opt) = writes_guarded_since_with_clean(sub0_start, sub0_end, u64::MAX).unwrap();
        let mut ep0 = ep0_opt.expect("sub0 clean epochs");
        for _ in 0..8 {
            if guarded_clean_epochs_match(sub0_start, sub0_end, ep0, ts0) {
                break;
            }
            let (ts_r, _, ep_r) = writes_guarded_since_with_clean(sub0_start, sub0_end, ts0).unwrap();
            ts0 = ts_r;
            ep0 = ep_r.expect("sub0 clean epochs retry");
        }
        assert!(guarded_clean_epochs_match(sub0_start, sub0_end, ep0, ts0));

        // Guard sub1, then fault sub1 and re-guard sub1.
        let _ = writes_guarded_since_with_clean(sub1_start, sub1_end, u64::MAX).unwrap();
        unsafe { (sub1_start as *mut u8).write_volatile(0x99) };
        let _ = writes_guarded_since_with_clean(sub1_start, sub1_end, ts0).unwrap();

        // Even though sub0 and sub1 share a 64 MiB shard (in most placements),
        // sub0's 1 MiB clean epoch MUST remain valid!
        if same_shard {
            assert!(
                guarded_clean_epochs_match(sub0_start, sub0_end, ep0, ts0),
                "faulting sub1 must not invalidate sub0's 1 MiB clean epoch in the same 64 MiB shard"
            );
        }

        title_memory_remapped(base, BLOCK * 32);
        unsafe { UnmapViewOfFile(view) };
    }

    #[test]
    fn lazy_alias_unlock_keeps_untouched_alias_protected_and_catches_alias_reads() {
        let _guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use super::{
            BLOCK, guards_read_shard, shard_of, title_memory_remapped,
            writes_guarded_since_with_clean,
        };
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        };
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                BLOCK as u32,
                std::ptr::null(),
            )
        };
        assert!(!mapping.is_null());
        let v1 = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, BLOCK) };
        let v2 = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, BLOCK) };
        assert!(!v1.Value.is_null() && !v2.Value.is_null());
        let a = v1.Value as usize;
        let b = v2.Value as usize;
        let sec = mapping as usize;
        crate::inplace::title_memory_view(a, BLOCK, sec, 0);
        crate::inplace::title_memory_view(b, BLOCK, sec, 0);

        // Guard view A: both A and its alias B must become PAGE_READONLY (`protected`).
        let (ts_a0, _, _) = writes_guarded_since_with_clean(a, a + BLOCK, u64::MAX).unwrap();
        let (ts_b0, _, _) = writes_guarded_since_with_clean(b, b + BLOCK, u64::MAX).unwrap();
        assert!(guards_read_shard(shard_of(a / BLOCK)).protected.contains(a / BLOCK));
        assert!(guards_read_shard(shard_of(b / BLOCK)).protected.contains(b / BLOCK));

        // Write through A: A faults & unlocks, while untouched alias B STAYS protected
        // (zero VirtualProtect on B!) yet is removed from clean/accounted.
        unsafe { (a as *mut u8).write_volatile(0x42) };
        assert!(!guards_read_shard(shard_of(a / BLOCK)).protected.contains(a / BLOCK));
        assert!(
            guards_read_shard(shard_of(b / BLOCK)).protected.contains(b / BLOCK),
            "untouched alias B must remain PAGE_READONLY after write fault on A"
        );
        assert!(!guards_read_shard(shard_of(b / BLOCK)).clean.contains(b / BLOCK));

        // Shadow reading B must see the write and re-arm A back to PAGE_READONLY!
        let (ts_b1, written_b, _) = writes_guarded_since_with_clean(b, b + BLOCK, ts_b0).unwrap();
        assert!(
            written_b.iter().any(|&(p, t)| p == b / BLOCK && t > ts_b0),
            "alias B must report dirty after write through A"
        );
        assert!(
            guards_read_shard(shard_of(a / BLOCK)).protected.contains(a / BLOCK),
            "guarding alias B must re-arm A back to PAGE_READONLY"
        );
        assert!(guards_read_shard(shard_of(b / BLOCK)).clean.contains(b / BLOCK));

        // A second write through A must fault again and dirty B again.
        unsafe { (a as *mut u8).write_volatile(0x43) };
        let (_, written_b2, _) = writes_guarded_since_with_clean(b, b + BLOCK, ts_b1).unwrap();
        assert!(
            written_b2.iter().any(|&(p, t)| p == b / BLOCK && t > ts_b1),
            "second write through A must be caught after alias B re-armed A"
        );
        let _ = ts_a0;

        title_memory_remapped(a, BLOCK);
        title_memory_remapped(b, BLOCK);
        crate::inplace::views_gone(a, BLOCK);
        crate::inplace::views_gone(b, BLOCK);
        unsafe {
            UnmapViewOfFile(v1);
            UnmapViewOfFile(v2);
            CloseHandle(mapping);
        }
    }
}
