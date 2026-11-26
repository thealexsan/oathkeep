use super::*;

#[derive(Default)]
pub(super) struct Batch {
    pub(super) refreshes: Vec<(std::sync::Arc<crate::memory::shadow::Shadow>, usize, usize)>,
    /// Where each copy (by address) is in `refreshes`: the list that makes
    /// the heap's buffers ready adds some 4,100 copies, and searching them
    /// for every refresh of a submission was tens of millions of
    /// comparisons a frame.
    pub(super) index: ByCopy<usize>,
    /// Staging buffers (by address) refreshed since the last title write,
    /// or due to be.
    pub(super) textures: std::collections::HashSet<usize>,
    /// Those due ([`Batch::refresh_textures`]).
    pub(super) due: Vec<std::sync::Arc<crate::textures::texture::Staging>>,
    /// What copies at their place in the submission's lists put into title
    /// memory ([`place`]): the host copy, the title address, the bytes.
    pub(super) placed: Vec<(std::sync::Arc<crate::memory::shadow::Shadow>, usize, Vec<u8>)>,
    /// Fills not done yet (destination, source, bytes): [`wide_fills`].
    pub(super) fills: Vec<(usize, usize, usize)>,
    /// Copies (by address) brought up to date ahead of the steps, and the
    /// range (from, to) ([`Batch::refresh_ahead`]).
    pub(super) ahead: ByCopy<(usize, usize)>,
    /// `ahead` holds everything the submission refreshes
    /// ([`Batch::refresh_front`]).
    pub(super) front: bool,
}

/// Whether the copies that a submission's shaders write are brought up to
/// date together, on several threads, ahead of its steps
/// (`OATHKEEP_REFRESH_AHEAD=0`: each before the step that gives it to them).
///
/// 580 copies a frame, one after the other on the submitting thread, 3.4 to
/// 6.5 ms (session play-1125-0203).
impl Batch {
    pub(super) fn mark_ahead(&mut self, key: usize, from: usize, to: usize) {
        self.ahead
            .entry(key)
            .and_modify(|span| {
                span.0 = span.0.min(from);
                span.1 = span.1.max(to);
            })
            .or_insert((from, to));
    }

    pub(super) fn add(&mut self, shadow: &std::sync::Arc<crate::memory::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        if self.ahead.get(&key).is_some_and(|&(from, to)| from <= address && end <= to) {
            return;
        }
        if let Some(&at) = self.index.get(&key) {
            let r = &mut self.refreshes[at];
            if r.1 <= address && end <= r.2 {
                return;
            }
            if crate::memory::watch::light() && shadow.nothing_new(address, len) {
                return;
            }
            (r.1, r.2) = (r.1.min(address), r.2.max(end));
            return;
        }
        if crate::memory::watch::light() && shadow.nothing_new(address, len) {
            self.mark_ahead(key, address, end);
            return;
        }
        self.index.insert(key, self.refreshes.len());
        self.refreshes.push((shadow.clone(), address, end));
    }

    /// Before shaders are given a range of `shadow`'s copy: its pending
    /// refresh is done, the others stay pending.
    pub(super) fn flush_one(&mut self, shadow: &std::sync::Arc<crate::memory::shadow::Shadow>) {
        // From here on it is refreshed around what the shaders write. (What
        // is in front of all the steps was, up to here.)
        if !self.front {
            self.ahead.remove(&(std::sync::Arc::as_ptr(shadow) as usize));
        }
        let Some(at) = self.index.remove(&(std::sync::Arc::as_ptr(shadow) as usize)) else {
            return;
        };
        let (shadow, from, to) = self.refreshes.swap_remove(at);
        // The last entry took its place.
        if let Some(moved) = self.refreshes.get(at) {
            self.index.insert(std::sync::Arc::as_ptr(&moved.0) as usize, at);
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        if !(crate::memory::watch::light() && shadow.nothing_new(from, to - from)) {
            Self::update(&shadow, from, to);
        }
        #[cfg(feature = "diagnostics")]
        {
            let ns = started.elapsed().as_nanos() as u64;
            SPLIT[6].fetch_add(ns, Ordering::Relaxed);
            SPLIT[9].fetch_add(ns, Ordering::Relaxed);
            SPLIT[10].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Brings `[from, to)` of a copy up to date, timed.
    pub(super) fn update(shadow: &crate::memory::shadow::Shadow, from: usize, to: usize) {
        let started = std::time::Instant::now();
        let _one = crate::diagnostics::timeline::span("a copy's refresh");
        let copied = shadow.update(from, to - from);
        let us = started.elapsed().as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::range(from, to - from, us, 0);
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
        PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
    }

    /// Whether `staging` is still to be refreshed in this submission (and
    /// notes it refreshed).
    pub(super) fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures.insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    pub(super) fn title_written(&mut self) {
        self.flush();
        self.ahead.clear();
        self.textures.clear();
    }

    /// After a step that wrote `[dst, dst + len)` of title memory: drops
    /// `self.ahead` entries overlapping that range (or all of `self.ahead` if
    /// `aliases` is true) and clears `self.textures`.
    pub(super) fn invalidate_written(&mut self, dst: usize, len: usize, aliases: bool) {
        if aliases {
            self.ahead.clear();
        } else if len > 0 && !self.ahead.is_empty() {
            let end = dst.saturating_add(len);
            self.ahead.retain(|_, &mut (from, to)| to <= dst || from >= end);
        }
        self.textures.clear();
    }

    /// Before the steps of `runs` ([`refresh_front`]), a new part of the
    /// submission beginning at each of `parts`.
    pub(super) fn refresh_front(&mut self, runs: &[&HostList], parts: &[usize]) {
        let mut at: ByCopy<usize> = ByCopy::default();
        let mut spans: Vec<(std::sync::Arc<crate::memory::shadow::Shadow>, usize, usize)> = Vec::new();
        let mut given: Vec<u32> = Vec::new();
        let mut due = Vec::new();
        let (mut part, mut last_key, mut last_idx) = (1u32, 0usize, usize::MAX);
        let wide_tex = wide_textures();
        for (n, list) in runs.iter().enumerate() {
            if parts.contains(&n) {
                part += 1;
            }
            for step in &list.deferred().copies {
                match step {
                    Step::Refresh(shadow, addr, len) => {
                        let key = std::sync::Arc::as_ptr(shadow) as usize;
                        let i = if key == last_key {
                            last_idx
                        } else {
                            let idx = *at.entry(key).or_insert_with(|| {
                                spans.push((shadow.clone(), usize::MAX, 0));
                                given.push(0);
                                spans.len() - 1
                            });
                            last_key = key;
                            last_idx = idx;
                            idx
                        };
                        if given[i] != part {
                            let span = &mut spans[i];
                            (span.1, span.2) = (span.1.min(*addr), span.2.max(*addr + *len));
                        }
                    }
                    Step::GpuWrite(shadow, ..) => {
                        let key = std::sync::Arc::as_ptr(shadow) as usize;
                        let i = if key == last_key {
                            last_idx
                        } else {
                            let idx = *at.entry(key).or_insert_with(|| {
                                spans.push((shadow.clone(), usize::MAX, 0));
                                given.push(0);
                                spans.len() - 1
                            });
                            last_key = key;
                            last_idx = idx;
                            idx
                        };
                        given[i] = part;
                    }
                    Step::Texture(staging) if wide_tex => {
                        if self.texture_due(staging) {
                            due.push(staging.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        spans.retain(|s| s.1 < s.2);
        self.front = true;
        self.ahead.reserve(spans.len());
        for (shadow, from, to) in &spans {
            self.ahead.insert(std::sync::Arc::as_ptr(shadow) as usize, (*from, *to));
        }
        #[cfg(feature = "diagnostics")]
        {
            FLUSHES[0].fetch_add(1, Ordering::Relaxed);
            FLUSHES[1].fetch_add(spans.len() as u64, Ordering::Relaxed);
        }
        let _flush = crate::diagnostics::timeline::span("refreshes of copies");
        let started = std::time::Instant::now();
        let before = [0, 2].map(|i| PREPARING[i].load(Ordering::Relaxed));
        // Filter out known-no-op refreshes BEFORE `ask_ahead` and `wide_each`
        // so `ask_ahead` and worker dispatch only see active refreshes.
        if crate::memory::watch::light() {
            let _skipped = filter_active_refreshes(&mut spans);
            #[cfg(feature = "diagnostics")]
            FLUSHES[2].fetch_add(_skipped as u64, Ordering::Relaxed);
        }
        let ahead = !crate::memory::watch::once_kept() && spans.len() > 1;
        if ahead {
            let ranges: Vec<(usize, usize)> = spans.iter().filter(|r| r.0.asked_ahead()).map(|r| (r.1, r.2)).collect();
            crate::memory::watch::ask_ahead(&ranges);
        }
        #[cfg(feature = "diagnostics")]
        FLUSHES[3].fetch_add(spans.len() as u64, Ordering::Relaxed);
        crate::memory::shadow::wide_each(spans.len() + due.len(), move |i| {
            // The textures first: they are the longer pieces.
            let Some(i) = i.checked_sub(due.len()) else {
                due[i].refresh();
                return;
            };
            let (shadow, from, to) = &spans[i];
            Self::update(shadow, *from, *to);
        });
        if ahead {
            crate::memory::watch::ask_ahead_done();
        }
        // The time counted is the time it took, not the threads' sum.
        let took = started.elapsed().as_micros() as u64;
        let summed = [0, 2].map(|i| PREPARING[i].load(Ordering::Relaxed)).iter().zip(before).map(|(now, was)| now.saturating_sub(was)).collect::<Vec<_>>();
        PREPARING[2].fetch_sub(summed[1], Ordering::Relaxed);
        PREPARING[0].fetch_sub(summed[0], Ordering::Relaxed);
        PREPARING[0].fetch_add(took, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        SPLIT[6].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// Before the steps of `runs` ([`ahead_of_writes`]); copies and fills
    /// that `taken` names are not done on the CPU.
    pub(super) fn refresh_ahead(&mut self, runs: &[&HostList], taken: impl Fn(&HostList, usize) -> bool) {
        let mut copies: ByCopy<std::sync::Arc<crate::memory::shadow::Shadow>> = ByCopy::default();
        let mut touches = Vec::new();
        'steps: for list in runs {
            for (i, step) in list.deferred().copies.iter().enumerate() {
                touches.push(match step {
                    Step::Refresh(shadow, at, len) => {
                        let key = std::sync::Arc::as_ptr(shadow) as usize;
                        copies.entry(key).or_insert_with(|| shadow.clone());
                        Touch::Refresh(key, *at, *at + *len)
                    }
                    Step::GpuWrite(shadow, ..) => Touch::Write(std::sync::Arc::as_ptr(shadow) as usize),
                    Step::Copy(..) if taken(list, i) => Touch::Nothing,
                    Step::Copy(..) | Step::Run(_) | Step::Placed(_) => break 'steps,
                    _ => Touch::Nothing,
                });
            }
        }
        let due: Vec<_> = written_spans(touches.into_iter())
            .into_iter()
            .filter_map(|(key, from, to)| Some((copies.get(&key)?.clone(), from, to)))
            .collect();
        for (shadow, from, to) in &due {
            self.ahead.insert(std::sync::Arc::as_ptr(shadow) as usize, (*from, *to));
        }
        #[cfg(feature = "diagnostics")]
        let began = std::time::Instant::now();
        let started = std::time::Instant::now();
        let before = PREPARING[0].load(Ordering::Relaxed);
        crate::memory::shadow::wide_each(due.len(), move |i| {
            let (shadow, from, to) = &due[i];
            if !shadow.nothing_new(*from, *to - *from) {
                Self::update(shadow, *from, *to);
            }
        });
        // The time counted is the time it took, not the threads' sum.
        let summed = PREPARING[0].load(Ordering::Relaxed).saturating_sub(before);
        PREPARING[0].fetch_sub(summed.saturating_sub(started.elapsed().as_micros() as u64), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            let ns = began.elapsed().as_nanos() as u64;
            SPLIT[6].fetch_add(ns, Ordering::Relaxed);
            SPLIT[9].fetch_add(ns, Ordering::Relaxed);
        }
    }

    /// The fills noted are done.
    pub(super) fn fill(&mut self) {
        // Below this many the threads cost more than they save.
        const FEW: usize = 96;
        const PART: usize = 64;
        if self.fills.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        if self.fills.len() < FEW {
            for (dst, src, len) in self.fills.drain(..) {
                copy_mapped(dst as *mut u8, src as *const u8, len, false);
            }
        } else {
            let fills = std::mem::take(&mut self.fills);
            crate::memory::shadow::wide_each(fills.len().div_ceil(PART), move |i| {
                for &(dst, src, len) in fills.iter().skip(i * PART).take(PART) {
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                }
            });
        }
        let took = started.elapsed();
        PREPARING[0].fetch_add(took.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        SPLIT[7].fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
    }

    /// The textures due are brought up to date.
    pub(super) fn refresh_textures(&mut self) {
        if self.due.len() < 2 || !wide_textures() {
            self.due.drain(..).for_each(|staging| staging.refresh());
            return;
        }
        let _textures = crate::diagnostics::timeline::span("textures brought up to date");
        // The time counted is the time it took, not the threads' sum.
        let started = std::time::Instant::now();
        let before = PREPARING[2].load(Ordering::Relaxed);
        let due = std::mem::take(&mut self.due);
        crate::memory::shadow::wide_each(due.len(), move |i| due[i].refresh());
        let summed = PREPARING[2].load(Ordering::Relaxed).saturating_sub(before);
        PREPARING[2].fetch_sub(summed.saturating_sub(started.elapsed().as_micros() as u64), Ordering::Relaxed);
    }

    pub(super) fn flush(&mut self) {
        self.refresh_textures();
        self.index.clear();
        if self.refreshes.is_empty() {
            return;
        }
        #[cfg(feature = "diagnostics")]
        let (began, all) = (std::time::Instant::now(), self.refreshes.len());
        let _flush = crate::diagnostics::timeline::span("refreshes of copies");
        self.flush_all();
        #[cfg(feature = "diagnostics")]
        {
            SPLIT[6].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
            FLUSHES[0].fetch_add(1, Ordering::Relaxed);
            FLUSHES[1].fetch_add(all as u64, Ordering::Relaxed);
        }
    }

    pub(super) fn flush_all(&mut self) {
        let light = crate::memory::watch::light();
        if !self.front {
            for (shadow, from, to) in &self.refreshes {
                let key = std::sync::Arc::as_ptr(shadow) as usize;
                self.ahead
                    .entry(key)
                    .and_modify(|span| {
                        span.0 = span.0.min(*from);
                        span.1 = span.1.max(*to);
                    })
                    .or_insert((*from, *to));
            }
        }
        if light {
            let _skipped = filter_active_refreshes(&mut self.refreshes);
            #[cfg(feature = "diagnostics")]
            FLUSHES[2].fetch_add(_skipped as u64, Ordering::Relaxed);
            if self.refreshes.is_empty() {
                return;
            }
        }
        // One write-watch asking for the copies that lie close together,
        // not one per copy.
        let ahead = !crate::memory::watch::once_kept() && self.refreshes.len() > 1;
        if ahead {
            let started = std::time::Instant::now();
            let ranges: Vec<(usize, usize)> = self
                .refreshes
                .iter()
                .filter(|r| r.0.asked_ahead())
                .map(|r| (r.1, r.2))
                .collect();
            crate::memory::watch::ask_ahead(&ranges);
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        if wide_refresh() && self.refreshes.len() >= 4 {
            // Each copy on its own, several at a time: a copy's refresh
            // writes that copy and reads title memory, nothing of another's.
            // The time counted is the time it took, not the threads' sum.
            let started = std::time::Instant::now();
            let before = PREPARING[0].load(Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            FLUSHES[3].fetch_add(self.refreshes.len() as u64, Ordering::Relaxed);
            if light {
                // A number to a free thread, not a job to each copy.
                let refreshes = std::mem::take(&mut self.refreshes);
                crate::memory::shadow::wide_each(refreshes.len(), move |i| {
                    let (shadow, from, to) = &refreshes[i];
                    if !(ahead && shadow.nothing_new(*from, *to - *from)) {
                        Self::update(shadow, *from, *to);
                    }
                });
            } else {
                let jobs: Vec<Box<dyn FnOnce() + Send>> = self
                    .refreshes
                    .drain(..)
                    .map(|(shadow, from, to)| Box::new(move || Self::update(&shadow, from, to)) as Box<dyn FnOnce() + Send>)
                    .collect();
                crate::memory::shadow::wide_run(jobs);
            }
            let summed = PREPARING[0].load(Ordering::Relaxed).saturating_sub(before);
            PREPARING[0].fetch_sub(summed.saturating_sub(started.elapsed().as_micros() as u64), Ordering::Relaxed);
        } else {
            for (shadow, from, to) in self.refreshes.drain(..) {
                if !(light && ahead && shadow.nothing_new(from, to - from)) {
                    Self::update(&shadow, from, to);
                }
            }
        }
        if ahead {
            crate::memory::watch::ask_ahead_done();
        }
    }
}

/// Drops refreshes known to copy nothing (`Shadow::nothing_new`) and returns
/// how many were skipped. Batches above `FEW` are checked in parallel chunks
/// using one `u64` bitmask per chunk so neither `ask_ahead` nor worker
/// dispatch wastes work on no-ops.
pub(super) fn filter_active_refreshes(refreshes: &mut Vec<(std::sync::Arc<crate::memory::shadow::Shadow>, usize, usize)>) -> usize {
    const FEW: usize = 64;
    let total = refreshes.len();
    if total <= FEW || !wide_refresh() {
        refreshes.retain(|(shadow, from, to)| !shadow.nothing_new(*from, *to - *from));
        return total - refreshes.len();
    }
    let chunk_size = total.div_ceil(8).clamp(8, 64);
    let num_chunks = total.div_ceil(chunk_size);
    let items = std::sync::Arc::new(std::mem::take(refreshes));
    let keep: std::sync::Arc<Vec<std::sync::atomic::AtomicU64>> =
        std::sync::Arc::new((0..num_chunks).map(|_| std::sync::atomic::AtomicU64::new(0)).collect());
    let items_w = items.clone();
    let keep_w = keep.clone();
    crate::memory::shadow::wide_each(num_chunks, move |chunk| {
        let start = chunk * chunk_size;
        let end = (start + chunk_size).min(total);
        let mut mask = 0u64;
        for (bit, i) in (start..end).enumerate() {
            let (shadow, from, to) = &items_w[i];
            if !shadow.nothing_new(*from, *to - *from) {
                mask |= 1u64 << bit;
            }
        }
        keep_w[chunk].store(mask, Ordering::Relaxed);
    });
    let mut items = std::sync::Arc::try_unwrap(items).unwrap_or_else(|arc| (*arc).clone());
    let mut idx = 0;
    items.retain(|_| {
        let chunk = idx / chunk_size;
        let bit = idx % chunk_size;
        idx += 1;
        (keep[chunk].load(Ordering::Relaxed) >> bit) & 1 != 0
    });
    *refreshes = items;
    total - refreshes.len()
}
