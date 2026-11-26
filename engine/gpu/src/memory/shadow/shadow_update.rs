use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use crate::memory::watch::PAGE;

use super::{GUARDED, KERNEL_PAGE, NOT_WATCHED, Shadow};
use super::pool::{by_parts, for_pieces_of};
use super::put::{ECHO, SEND, dword_runs, land_echo, send_changed};
use super::wide::wide_each;

impl Shadow {
    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        // In place: the card reads title memory itself.
        if self.place.is_some() {
            return 0;
        }
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = if self.has_pending.load(Ordering::Acquire) {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .copied()
                .filter(|&(a, e)| a < to && e > from)
                .collect()
        } else {
            Vec::new()
        };
        if !busy.is_empty() {
            // Around ranges shaders are writing: the gaps, each as below
            // (in the world most submissions have such ranges; copying the
            // gaps whole cost ~1 s per frame).
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                if a > at {
                    bytes += self.update_free(at, a);
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_free(at, to);
            }
            return bytes;
        }
        self.update_free(from, to)
    }

    /// Whether [`Shadow::update`] of `[address, address + len)` would copy
    /// nothing, and knows it without asking anything
    /// (`crate::memory::watch::nothing_new`). `false` if it would have to ask.
    pub(crate) fn nothing_new(&self, address: usize, len: usize) -> bool {
        if self.place.is_some() {
            return true;
        }
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return true;
        }
        if !matches!(self.watched.load(Ordering::Relaxed), 1 | GUARDED) {
            return false;
        }
        if !crate::memory::watch::can_answer_without_asking(from, to) {
            return false;
        }
        // Around ranges shaders are writing the refresh asks gap by gap.
        if self.has_pending.load(Ordering::Acquire)
            && self.pending.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|&(a, e)| a < to && e > from)
        {
            return false;
        }
        let copied = self.copied.read().unwrap_or_else(|e| e.into_inner());
        let same = crate::memory::watch::nothing_new(from, to, start / PAGE, &copied.pages, copied.ranged(from, to));
        #[cfg(feature = "diagnostics")]
        if same {
            use crate::diagnostics::copy_stats::{C, add};
            add(C::Checked, (to - from) as u64);
            add(C::Unchanged, (to - from) as u64);
        }
        same
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where no
    /// shader writes.
    pub(super) fn update_free(&self, from: usize, to: usize) -> usize {
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            if let Some((now, written)) = crate::memory::watch::writes(from, to, true) {
                // (A range whose blocks rest after a write has no guard now.)
                let guarded = crate::memory::watch::guarded(from, to) || self.watched.load(Ordering::Relaxed) == GUARDED;
                self.watched.store(if guarded { GUARDED } else { 1 }, Ordering::Relaxed);
                let copied = self.copy_stale(from, to, now, &written);
                self.refresh_check(from, to);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::diagnostics::copy_stats::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::WholeBytes, (to - from) as u64);
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self.copy(from, to);
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::whole(from, to - from, started.elapsed().as_micros() as u64);
        to - from
    }

    /// Diagnostic (`OATHKEEP_REFRESH_CHECK=1`; off by default): after a
    /// refresh of `[from, to)` that trusted the write watch or the guards,
    /// the first 256 bytes of every 64 KiB block of it are read from title
    /// memory and compared with the copy. A block that differs on three
    /// refreshes in a row was changed with nothing to tell of it (a write
    /// through another view of its pages, as DirectStorage's page
    /// destinations make) and the card reads old bytes there: said with
    /// the copy and the address. One that differs once is a write of the
    /// title's that arrived between the refresh and the look.
    pub(super) fn refresh_check(&self, from: usize, to: usize) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_REFRESH_CHECK").is_some_and(|v| v == "1")) {
            return;
        }
        const SAMPLE: usize = 256;
        // Blocks found differing: how many refreshes in a row.
        static DIFFERING: Mutex<Option<std::collections::HashMap<usize, u32>>> = Mutex::new(None);
        static SAID: AtomicU64 = AtomicU64::new(0);
        let start = self.start as usize;
        let mut title = [0u8; SAMPLE];
        let mut at = from;
        while at < to {
            let n = SAMPLE.min(to - at);
            if crate::backend::read_into(at as *const u8, &mut title[..n]) {
                // SAFETY: `[from, to)` lies inside the buffer (`update`),
                // so these bytes are inside the copy's mapping.
                let copy = unsafe { std::slice::from_raw_parts(self.data.add(at - start).cast_const(), n) };
                let mut map = DIFFERING.lock().unwrap_or_else(|e| e.into_inner());
                let map = map.get_or_insert_with(Default::default);
                if copy == &title[..n] {
                    map.remove(&at);
                } else {
                    let times = map.entry(at).or_insert(0);
                    *times += 1;
                    if *times == 3 {
                        let said = SAID.fetch_add(1, Ordering::Relaxed) + 1;
                        if said <= 64 || said.is_power_of_two() {
                            tracing::warn!(
                                said,
                                copy = self.id,
                                copy_start = format_args!("{start:#x}"),
                                copy_bytes = self.len,
                                address = format_args!("{at:#x}"),
                                refreshed = format_args!("{from:#x}..{to:#x}"),
                                on_the_card = self.card.is_some(),
                                window = self.window,
                                "a host copy of title memory is stale after its refresh, three times in a row:                                  the memory changed with nothing to tell of it (OATHKEEP_REFRESH_CHECK=1)"
                            );
                        }
                    }
                }
            }
            // The first bytes of the next 64 KiB block.
            at = (at / PAGE + 1) * PAGE;
        }
    }

    /// Copies the pages of `[from, to)` written after their last copy
    /// (`written`: last writes by page, in page order; `now`: this
    /// asking), whole pages within the buffer.
    pub(super) fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let base = self.start as usize / PAGE;
        let mut copied = self.copied.write().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        // Pages the range holds in part are never noted in `copied`, and
        // were copied again at every refresh of the range, written or not:
        // of the heap's buffer views alone two ends for each of 4,400
        // copies, every frame. A range that was taken whole, and not written
        // since, is as it was.
        let ranged = if crate::memory::watch::light() { copied.ranged(from, to) } else { 0 };
        let (mut runs, mut run) = (Vec::new(), None::<(usize, bool)>);
        // Pages taken before and written since: in the order of their runs.
        let mut again_at: Vec<(usize, u64)> = Vec::new();
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = &mut copied.pages[page - base];
            let taken = (*at).max(ranged);
            if taken == 0 || write > taken {
                // A page only partly in the range is copied in part: it
                // stays stale for the rest.
                let (start, end) = (self.start as usize, (self.start + self.len) as usize);
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    *at = now;
                }
                // Cut where pages taken before meet pages never taken.
                let again = taken != 0;
                if again {
                    again_at.push((page, taken));
                }
                if let Some((first, known)) = run
                    && known != again
                {
                    runs.push(((first * PAGE).max(from), (page * PAGE).min(to), known));
                    run = None;
                }
                run.get_or_insert((page, again));
            } else if let Some((first, known)) = run.take() {
                runs.push(((first * PAGE).max(from), (page * PAGE).min(to), known));
            }
        }
        // Cut to the range: next to it shaders may be writing the copy.
        if let Some((first, known)) = run {
            runs.push(((first * PAGE).max(from), ((last + 1) * PAGE).min(to), known));
        }
        let runs = by_parts(runs, &again_at, &crate::memory::watch::parts_written(&again_at));
        let bytes = self.take_all(runs);
        if crate::memory::watch::light() {
            // All of the range is in the copy as of this asking.
            copied.record_range(from, to, now);
        }
        bytes
    }

    /// [`Shadow::take`] of `runs` (from, to, known), on several threads if
    /// they are long: the big heap's copy has some 4 MB of them in each of
    /// 14 refreshes a frame, on one thread while the others had nothing.
    pub(super) fn take_all(&self, runs: Vec<(usize, usize, bool)>) -> usize {
        const LONG: usize = 1 << 20;
        const PART: usize = 256 << 10;
        let bytes = runs.iter().map(|r| r.1 - r.0).sum();
        if bytes < LONG || !crate::memory::watch::light() {
            for (from, to, known) in runs {
                self.take(from, to, known);
            }
            return bytes;
        }
        let mut parts = Vec::with_capacity(bytes / PART + runs.len());
        for (from, to, known) in runs {
            let mut at = from;
            while at < to {
                // Whole pages to a part.
                let next = ((at / PAGE) * PAGE + PART).min(to);
                parts.push((at, next, known));
                at = next;
            }
        }
        let this = std::ptr::from_ref(self) as usize;
        wide_each(parts.len(), move |i| {
            // SAFETY: `wide_each` returns when every part is done, before
            // the borrow of `self` ends. The parts lie apart.
            let this = unsafe { &*(this as *const Shadow) };
            let (from, to, known) = parts[i];
            this.take(from, to, known);
        });
        bytes
    }

    /// What shaders wrote in `[from, to)` (offsets) was put into title memory.
    #[cfg(test)]
    pub(super) fn note_landing(&self, from: usize, to: usize) {
        self.note_landings(&[(from, to)]);
    }

    /// Batched [`Shadow::note_landing`] across multiple `(from, to)` ranges so
    /// [`Shadow::land_card`] acquires `self.landings` once per job instead of
    /// once per 64 KiB piece.
    pub(super) fn note_landings(&self, ranges: &[(usize, usize)]) {
        if ranges.is_empty() {
            return;
        }
        let first = self.start as usize / KERNEL_PAGE;
        let mut bits = self.landings.lock().unwrap_or_else(|e| e.into_inner());
        if bits.is_empty() {
            let all = (self.start + self.len).div_ceil(KERNEL_PAGE as u64) as usize - first;
            bits.resize(all.div_ceil(64), 0);
        }
        for &(from, to) in ranges {
            let pages = (self.start as usize + from) / KERNEL_PAGE - first..(self.start as usize + to).div_ceil(KERNEL_PAGE) - first;
            for page in pages {
                bits[page / 64] |= 1 << (page % 64);
            }
        }
        self.landing.store(true, Ordering::Release);
    }

    /// [`Shadow::take`] of `[from, to)`, which the copy held before and which
    /// is written since: where it may be only the landing that wrote it, the
    /// 4 KiB that title memory and the CPU's side have alike are left as they
    /// are, and nothing of them is sent ([`land_echo`]).
    pub(super) fn take_landed(&self, from: usize, to: usize) -> usize {
        thread_local! {
            static TITLE: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let start = self.start as usize;
        let first = start / KERNEL_PAGE;
        // Runs of pages noted, and not noted (any more).
        let mut runs: Vec<(usize, usize, bool)> = Vec::new();
        {
            let mut bits = self.landings.lock().unwrap_or_else(|e| e.into_inner());
            let mut at = from;
            while at < to {
                let next = ((at / KERNEL_PAGE + 1) * KERNEL_PAGE).min(to);
                let page = at / KERNEL_PAGE - first;
                let noted = bits.get(page / 64).is_some_and(|word| word >> (page % 64) & 1 != 0);
                // (A page looked at in part is looked at again.)
                if noted && next - at == KERNEL_PAGE {
                    bits[page / 64] &= !(1 << (page % 64));
                }
                match runs.last_mut() {
                    Some(last) if last.2 == noted && next - last.0 <= PAGE => last.1 = next,
                    _ => runs.push((at, next, noted)),
                }
                at = next;
            }
        }
        TITLE.with_borrow_mut(|title| {
            for (from, to, noted) in runs {
                if !noted {
                    self.copy(from, to);
                    continue;
                }
                title.resize(to - from, 0);
                // (Pages that are not there read as zeros.)
                crate::backend::copy_mapped(title.as_mut_ptr(), from as *const u8, to - from, false);
                let mut at = from;
                while at < to {
                    let next = ((at / KERNEL_PAGE + 1) * KERNEL_PAGE).min(to);
                    let title = &title[at - from..next - from];
                    // SAFETY: inside the mapping of the copy's CPU side, which
                    // nothing else reads or writes there: no shader's range
                    // holds it, and the parts of a refresh lie apart.
                    let held = unsafe { std::slice::from_raw_parts_mut(self.data.add(at - start), next - at) };
                    if title == held {
                        ECHO[0].fetch_add((next - at) as u64, Ordering::Relaxed);
                    } else {
                        ECHO[1].fetch_add((next - at) as u64, Ordering::Relaxed);
                        held.copy_from_slice(title);
                        self.changed(at - start, next - start);
                        self.card_changed(at - start, next - start);
                    }
                    at = next;
                }
            }
        });
        to - from
    }

    /// Copies title memory `[from, to)` (inside the buffer) into the copy;
    /// `known`: the copy held it before ([`send_changed`]).
    pub(super) fn take(&self, from: usize, to: usize, known: bool) -> usize {
        thread_local! {
            /// A page of title memory, as read.
            static TITLE: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let Some(card) = self.card.as_ref().filter(|_| self.changes_only.load(Ordering::Relaxed) || send_changed()) else {
            if known && self.card.is_some() && self.landing.load(Ordering::Acquire) && land_echo() && !crate::backend::upload_verify::on() {
                return self.take_landed(from, to);
            }
            return self.copy(from, to);
        };
        let whole = !known && card.before_gpu.load(Ordering::Acquire) == 0;
        let start = self.start as usize;
        TITLE.with_borrow_mut(|title| {
            title.resize(PAGE, 0);
            let mut at = from;
            while at < to {
                let n = (PAGE - at % PAGE).min(to - at);
                let title = &mut title[..n];
                // (Pages that are not there read as zeros.)
                crate::backend::copy_mapped(title.as_mut_ptr(), at as *const u8, n, false);
                // SAFETY: inside the mapping of the copy's CPU side, which
                // nothing else reads or writes there (the card copies from
                // it): no shader's range holds it, and the parts of a
                // refresh lie apart.
                let held = unsafe { std::slice::from_raw_parts_mut(self.data.add(at - start), n) };
                let was = crate::backend::upload_verify::on().then(|| held.to_vec());
                let runs = if whole { vec![(0, n)] } else { dword_runs(at, held, title, 0) };
                for &(a, len) in &runs {
                    held[a..a + len].copy_from_slice(&title[a..a + len]);
                    self.changed(at - start + a, at - start + a + len);
                }
                self.card_changed_runs(runs.iter().map(|&(a, len)| (at - start + a, at - start + a + len)));
                if let Some(was) = was {
                    crate::backend::upload_verify::note(self.id, at - start, &was, held);
                }
                SEND[0].fetch_add(n as u64, Ordering::Relaxed);
                if runs.is_empty() {
                    SEND[1].fetch_add(n as u64, Ordering::Relaxed);
                }
                SEND[2].fetch_add(runs.iter().map(|r| r.1 as u64).sum(), Ordering::Relaxed);
                SEND[3].fetch_add(runs.len() as u64, Ordering::Relaxed);
                at += n;
            }
        });
        to - from
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    pub(super) fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        if self.card.is_some() && from < to && crate::backend::upload_verify::on() {
            for_pieces_of(from - start, to - start, |a, b| {
                let was = self.card_held(a, b - a);
                // SAFETY: inside the mapping, as above.
                crate::backend::copy_mapped(unsafe { self.data.add(a) }, (start + a) as *const u8, b - a, false);
                crate::backend::upload_verify::note(self.id, a, &was, &self.card_held(a, b - a));
            });
        } else if to - from >= (1 << 20) && crate::memory::watch::light() {
            const PART: usize = 256 << 10;
            let len = to - from;
            let parts = len.div_ceil(PART);
            let dst_base = at as usize;
            wide_each(parts, move |i| {
                let off = i * PART;
                let n = PART.min(len - off);
                crate::backend::copy_mapped((dst_base + off) as *mut u8, (from + off) as *const u8, n, false);
            });
        } else {
            crate::backend::copy_mapped(at, from as *const u8, to - from, false);
        }
        // After the bytes are in: a snapshot taken meanwhile is not current.
        self.changed(from - start, to - start);
        self.card_changed(from - start, to - start);
        to - from
    }
}
