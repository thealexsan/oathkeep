use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

pub(crate) const PAGE: usize = 64 << 10;
const KERNEL_PAGE: usize = 4096;
pub(crate) const HOT_WRITE: u64 = u64::MAX;
const HOT_FRAMES: u64 = 256;
const HOT_WINDOW_FRAMES: u64 = 16;

fn hot_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_GUARD_HOT").is_none_or(|v| v != "0"))
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
}

struct Journal {
    asked: u64,
    written: BTreeMap<usize, u64>,
    found: Vec<usize>,
    ahead: Vec<(usize, usize)>,
    ahead_asked: u64,
    kept: bool,
}

static JOURNAL: RwLock<Journal> = RwLock::new(Journal {
    asked: 1,
    written: BTreeMap::new(),
    found: Vec::new(),
    ahead: Vec::new(),
    ahead_asked: 1,
    kept: false,
});

fn journal_read() -> std::sync::RwLockReadGuard<'static, Journal> {
    JOURNAL.read().unwrap_or_else(|e| e.into_inner())
}

fn journal_write() -> std::sync::RwLockWriteGuard<'static, Journal> {
    JOURNAL.write().unwrap_or_else(|e| e.into_inner())
}

const AHEAD_GAP: usize = (8 << 20) / PAGE;
const AHEAD_MAX: usize = (1 << 30) / PAGE;

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
    j.asked += 1;
    j.ahead_asked = j.asked;
    j.ahead.clear();
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
        ask_run(j, &pieces[i..k]);
        i = k;
    }
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
    ask_ahead(ranges);
    let mut j = journal_write();
    let j = &mut *j;
    if j.ahead.is_empty() {
        j.asked += 1;
        j.ahead_asked = j.asked;
    }
    for &(start, end) in ranges {
        if start >= end {
            continue;
        }
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        if !j.ahead.iter().any(|&(a, b)| a <= first && last <= b) && ask_kernel(j, first, last) {
            j.ahead.push((first, last));
        }
    }
    j.kept = !j.ahead.is_empty();
}

pub(crate) fn ask_once_end() {
    let mut j = journal_write();
    if j.kept {
        j.kept = false;
        j.ahead.clear();
    }
}

pub(crate) fn wrote(at: usize, len: usize) {
    let mut j = journal_write();
    if !j.kept || len == 0 {
        return;
    }
    j.asked += 1;
    let now = j.asked;
    j.ahead_asked = now;
    for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        j.written.insert(page, now);
    }
}

fn ask_run(j: &mut Journal, pieces: &[(usize, usize)]) {
    if pieces.len() < 2 {
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if ask_kernel(j, first, last) {
        j.ahead.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run(j, a);
    ask_run(j, b);
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
    for k in 0..count {
        let page = j.found[k] / PAGE;
        j.written.insert(page, now);
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
            j.asked += 1;
            let now = j.asked;
            for k in 0..count {
                let page = j.found[k] / PAGE;
                j.written.insert(page, now);
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
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    #[cfg(feature = "diagnostics")]
    let guarding = std::time::Instant::now();
    let written = guard_with_hot(first * PAGE, last * PAGE);
    #[cfg(feature = "diagnostics")]
    {
        let us = guarding.elapsed().as_micros() as u64;
        crate::copystat::add(crate::copystat::C::GuardUs, us);
        crate::copystat::add(crate::copystat::C::Watches, 1);
        crate::copystat::add(crate::copystat::C::WatchUs, us);
        slow(us);
    }
    let (guarded, hot) = written?;
    if guarded.is_empty() {
        let j = journal_read();
        let mut out: Vec<(usize, u64)> = if since != 0 && j.asked <= since {
            Vec::new()
        } else {
            j.written
                .range(first..last)
                .filter(|&(_, &w)| since == 0 || w > since)
                .map(|(&p, &w)| (p, w))
                .collect()
        };
        merge_hot(&mut out, &hot);
        return Some((j.asked, out));
    }
    let mut j = journal_write();
    j.asked += 1;
    let now = j.asked;
    for page in guarded {
        j.written.insert(page, now);
    }
    let mut out: Vec<(usize, u64)> = j
        .written
        .range(first..last)
        .filter(|&(_, &w)| since == 0 || w > since)
        .map(|(&p, &w)| (p, w))
        .collect();
    merge_hot(&mut out, &hot);
    Some((now, out))
}

const BLOCK: usize = PAGE;

struct Guards {
    protected: IntervalSet,
    clean: IntervalSet,
    absent: IntervalSet,
    aliased: BTreeSet<usize>,
    recent_fault: BTreeMap<usize, u64>,
    hot: BTreeMap<usize, u64>,
}

static GUARDS: RwLock<Guards> = RwLock::new(Guards {
    protected: IntervalSet::new(),
    clean: IntervalSet::new(),
    absent: IntervalSet::new(),
    aliased: BTreeSet::new(),
    recent_fault: BTreeMap::new(),
    hot: BTreeMap::new(),
});

fn guards_read() -> std::sync::RwLockReadGuard<'static, Guards> {
    GUARDS.read().unwrap_or_else(|e| e.into_inner())
}

fn guards() -> std::sync::RwLockWriteGuard<'static, Guards> {
    GUARDS.write().unwrap_or_else(|e| e.into_inner())
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

fn guard_with_hot(start: usize, end: usize) -> Option<(Vec<usize>, Vec<usize>)> {
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ask = *ASK.get_or_init(|| std::env::var_os("OATHKEEP_GUARD_ASK").is_some_and(|v| v == "1"));
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    let frame = crate::frame::current();
    if !ask {
        let g = guards_read();
        if g.clean.contains_range(first, last) {
            return Some((Vec::new(), Vec::new()));
        }
        let has_hot = g.hot.range(first..last).next().is_some();
        let known = has_hot || g.protected.overlaps(first, last) || g.absent.overlaps(first, last);
        if known {
            let gaps = g.clean.gaps_in(first, last);
            if !gaps.is_empty() {
                let mut all_covered = true;
                let mut hot_pages = Vec::new();
                'gaps: for &(a, b) in &gaps {
                    if g.absent.contains_range(a, b) {
                        continue;
                    }
                    if !has_hot {
                        all_covered = false;
                        break;
                    }
                    let has_absent = g.absent.overlaps(a, b);
                    let mut cursor = a;
                    for (&block, &until) in g.hot.range(a..b) {
                        if block > cursor {
                            if !has_absent || !g.absent.contains_range(cursor, block) {
                                all_covered = false;
                                break 'gaps;
                            }
                        }
                        if until > frame {
                            if !has_absent || !g.absent.contains(block) {
                                hot_pages.push(block);
                            }
                            cursor = block + 1;
                        } else if !has_absent || !g.absent.contains(block) {
                            all_covered = false;
                            break 'gaps;
                        } else {
                            cursor = block + 1;
                        }
                    }
                    if cursor < b && (!has_absent || !g.absent.contains_range(cursor, b)) {
                        all_covered = false;
                        break 'gaps;
                    }
                }
                if all_covered {
                    return Some((Vec::new(), hot_pages));
                }
            }
        }
        drop(g);
        if !known && !is_view(start, end) {
            return None;
        }
    } else if !is_view(start, end) {
        return None;
    }
    install();
    let mut g = guards();
    if g.clean.contains_range(first, last) {
        return Some((Vec::new(), Vec::new()));
    }
    let mut written = Vec::new();
    let mut hot_pages = Vec::new();
    let mut protected_runs = Vec::new();
    let mut uncommitted_until = first;
    for (gap_first, gap_last) in g.clean.gaps_in(first, last) {
        let mut run: Option<usize> = None;
        let mut block = gap_first;
        while block < gap_last {
            if let Some(&until) = g.hot.get(&block) {
                if until > frame {
                    if let Some(from) = run.take() {
                        protect_here(&mut g, from, block);
                        protected_runs.push((from, block));
                    }
                    hot_pages.push(block);
                    block += 1;
                    continue;
                }
                g.hot.remove(&block);
            }
            if g.absent.contains(block) {
                if block >= uncommitted_until {
                    uncommitted_until = uncommitted_end(block);
                }
                if block < uncommitted_until {
                    if let Some(from) = run.take() {
                        protect_here(&mut g, from, block);
                        protected_runs.push((from, block));
                    }
                    block = uncommitted_until.min(gap_last);
                    continue;
                }
                g.absent.remove(block);
            }
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            if g.protected.contains(block) {
                if let Some(from) = run.take() {
                    protect_here(&mut g, from, block);
                    protected_runs.push((from, block));
                }
                g.clean.insert(block);
            } else {
                run.get_or_insert(block);
            }
            block += 1;
        }
        if let Some(from) = run.take() {
            protect_here(&mut g, from, gap_last);
            protected_runs.push((from, gap_last));
        }
    }
    if !protected_runs.is_empty() {
        protect_aliases(&mut g, &protected_runs);
    }
    Some((written, hot_pages))
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

fn protect_aliases(g: &mut Guards, runs: &[(usize, usize)]) {
    if !view_guard() || runs.is_empty() {
        return;
    }
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
        for alias in all {
            g.aliased.insert(alias);
        }
        all_other_runs.extend(other_runs);
    }
    if total_count == 0 {
        return;
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
        protect_alias_range(g, a, b);
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
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
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
    for block in a..b {
        // SAFETY: fallback per block if the span crosses a view boundary.
        if unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READONLY, &mut old) }
            != 0
        {
            g.protected.insert(block);
        }
    }
}

fn protect_here(g: &mut Guards, from: usize, to: usize) {
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
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.insert_range(from, to);
        g.clean.insert_range(from, to);
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
                block = raw_end;
            } else {
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
            }
        }
        block = reg_end;
    }
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

fn open(g: &mut Guards, start: usize, end: usize) -> bool {
    open_and_aliases(g, start, end).0
}

fn open_and_aliases(g: &mut Guards, start: usize, end: usize) -> (bool, Vec<usize>) {
    let any = open_here(g, start, end);
    if start >= end || g.aliased.is_empty() {
        return (any, Vec::new());
    }
    let aliased: Vec<usize> = g
        .aliased
        .range(start / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .collect();
    if aliased.is_empty() {
        return (any, Vec::new());
    }
    for &block in &aliased {
        g.aliased.remove(&block);
    }
    let (all, other_runs) = crate::inplace::alias_spans_of_blocks(&aliased);
    for &alias in &all {
        g.aliased.remove(&alias);
    }
    for (a, b) in other_runs {
        #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
        let opened = open_here_count(g, a.saturating_mul(BLOCK), b.saturating_mul(BLOCK));
        #[cfg(feature = "diagnostics")]
        if opened > 0 {
            ALIASES[1].fetch_add(opened, std::sync::atomic::Ordering::Relaxed);
        }
    }
    (any, all)
}

fn open_fault(g: &mut Guards, at: usize) -> bool {
    let (opened, aliases) = open_and_aliases(g, at, at + 1);
    if !opened {
        return false;
    }
    if hot_enabled() {
        let block = at / BLOCK;
        let frame = crate::frame::current();
        let mut become_hot = g
            .recent_fault
            .insert(block, frame)
            .is_some_and(|prev| frame >= prev && frame.saturating_sub(prev) <= HOT_WINDOW_FRAMES);
        for &alias in &aliases {
            if alias != block
                && g.recent_fault
                    .insert(alias, frame)
                    .is_some_and(|prev| {
                        frame >= prev && frame.saturating_sub(prev) <= HOT_WINDOW_FRAMES
                    })
            {
                become_hot = true;
            }
        }
        if become_hot {
            let until = frame.saturating_add(HOT_FRAMES);
            g.hot.insert(block, until);
            for alias in aliases {
                g.hot.insert(alias, until);
            }
        }
    }
    true
}

fn open_here(g: &mut Guards, start: usize, end: usize) -> bool {
    open_here_count(g, start, end) > 0
}

fn open_here_count(g: &mut Guards, start: usize, end: usize) -> u64 {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    let spans = g.protected.remove_range(first, last);
    if spans.is_empty() {
        return 0;
    }
    g.clean.remove_range(first, last);
    let mut blocks = 0u64;
    for (a, b) in spans {
        blocks += (b - a) as u64;
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
    blocks
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
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    if !guards_read().absent.overlaps(first, last) {
        return;
    }
    guards().absent.remove_range(first, last);
}

pub fn writing(at: usize, len: usize) {
    if len > 0
        && let Some(commit) = COMMIT_TITLE_MEMORY.get()
    {
        commit(at, len);
    }
    open(&mut guards(), at, at.saturating_add(len));
    crate::descriptor::host::remapped(at, len);
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
        let mut g = guards();
        let guarded = g.protected.count_in(at / BLOCK, end.div_ceil(BLOCK));
        if pages {
            WRITTEN[2].fetch_add(guarded, Relaxed);
        }
        if mode & 1 != 0 && guarded != 0 {
            open(&mut g, at, end);
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
    let mut g = guards();
    for &(at, n) in &ranges {
        if open_here(&mut g, at, at.saturating_add(n)) {
            #[cfg(feature = "diagnostics")]
            ALIASES[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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
    crate::descriptor::host::remapped(at, len);
    crate::inplace::views_gone(at, len);
    crate::inplace::title_memory_freeing(at, len);
    let mut g = guards();
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    g.absent.remove_range(first, last);
    g.protected.remove_range(first, last);
    g.clean.remove_range(first, last);
    let aliased: Vec<usize> = g.aliased.range(first..last).copied().collect();
    for block in aliased {
        g.aliased.remove(&block);
    }
    let hot_keys: Vec<usize> = g.hot.range(first..last).map(|(&k, _)| k).collect();
    for block in hot_keys {
        g.hot.remove(&block);
    }
    let recent_keys: Vec<usize> = g.recent_fault.range(first..last).map(|(&k, _)| k).collect();
    for block in recent_keys {
        g.recent_fault.remove(&block);
    }
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
        let mut g = guards();
        if open_fault(&mut g, at) {
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
    let mut g = guards();
    if open_fault(&mut g, at) {
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
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        title_memory_remapped(view, 2 * BLOCK);
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
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
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
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
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
        use super::{BLOCK, guard_with_hot, guards_read, title_memory_remapped, writable};
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
        // First write faults once -> re-protected on next guard.
        // SAFETY: committed mapped view memory.
        unsafe { (start as *mut u8).write_volatile(1) };
        let (second_guarded, second_hot) = guard_with_hot(start, end).unwrap();
        assert_eq!(second_guarded, vec![block0]);
        assert!(second_hot.is_empty());
        // Second write within HOT_WINDOW_FRAMES -> enters hot cooldown.
        // SAFETY: committed mapped view memory.
        unsafe { (start as *mut u8).write_volatile(2) };
        let (third_guarded, third_hot) = guard_with_hot(start, end).unwrap();
        assert!(third_guarded.is_empty());
        assert_eq!(third_hot, vec![block0]);
        // Block 0 stays writable (no reprotect) while block 1 stays clean.
        assert!(writable(start));
        // Remap clears hot state cleanly.
        title_memory_remapped(start, 2 * BLOCK);
        assert!(!guards_read().hot.contains_key(&block0));
        // SAFETY: unmap our test view.
        unsafe { UnmapViewOfFile(view) };
    }
}
