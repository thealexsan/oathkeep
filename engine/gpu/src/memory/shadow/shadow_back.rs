use std::sync::atomic::Ordering;

use super::diff::back_piece;
use super::{PIECE, Shadow};
use super::pool::{KEPT_BYTES, PINNED_SNAP_BYTES, first_difference, give_back, keeps, kept_most, last_difference, pinned_snap_most, pooled};
use super::put::put_changed;
use super::relay::{PREPARED, merge_piece, relay};
use super::snap::{BACK_CLASS, SNAPS};
use super::wide::{copy_wide, wide_back};

impl Shadow {
    /// At submission, after the range's refresh: a submitted list's shaders
    /// write this range. Its bytes are kept to tell later what the GPU wrote:
    /// one snapshot per copy and submission, each byte taken once, when the
    /// first range holding it is declared (the world declares overlapping
    /// views of its 174 MB heap dozens of times, from different offsets: a
    /// snapshot grown to cover each was copied whole every time), in reused
    /// memory (fresh snapshots cost ~2.3 million page faults per world
    /// frame).
    pub(crate) fn writing(&self, address: usize, len: usize) {
        if self.place.is_some() {
            return;
        }
        let range = (address, address + len);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // With the copy-back on its own thread: counted for this submission
        // (once), whether or not an earlier one still holds the range.
        if relay() {
            self.flights.lock().unwrap_or_else(|e| e.into_inner()).declare(range, PREPARED.load(Ordering::Relaxed));
        }
        // Dispatches declare the same ranges thousands of times.
        if pending.contains(&range) {
            return;
        }
        // In the card's memory: no snapshot. What shaders wrote is found on
        // the card (`Host::card_before` reads the declared ranges from the
        // lists).
        if self.card.is_some() {
            pending.push(range);
            self.has_pending.store(true, Ordering::Release);
            return;
        }
        // The parts no declared range holds yet: their bytes are taken now,
        // just after the range's refresh (a part declared earlier has not
        // been refreshed since, and keeps the bytes taken then).
        let initial = self.inside(address, len);
        if initial.0 >= initial.1 {
            pending.push(range);
            self.has_pending.store(true, Ordering::Release);
            return;
        }
        if pending.iter().any(|&(a, e)| a <= range.0 && range.1 <= e) {
            pending.push(range);
            self.has_pending.store(true, Ordering::Release);
            return;
        }
        let single_part = [initial];
        let mut multi_parts: Vec<(usize, usize)> = Vec::new();
        let parts: &[(usize, usize)] = if pending.is_empty() {
            &single_part
        } else {
            multi_parts.push(initial);
            let mut rest = Vec::with_capacity(4);
            for &(a, e) in pending.iter() {
                let (a, e) = self.inside(a, e - a);
                if a >= e {
                    continue;
                }
                rest.clear();
                for &(from, to) in &multi_parts {
                    if e <= from || a >= to {
                        rest.push((from, to));
                        continue;
                    }
                    if from < a {
                        rest.push((from, a));
                    }
                    if e < to {
                        rest.push((e, to));
                    }
                }
                std::mem::swap(&mut multi_parts, &mut rest);
                if multi_parts.is_empty() {
                    break;
                }
            }
            &multi_parts
        };
        pending.push(range);
        self.has_pending.store(true, Ordering::Release);
        let taken: usize = parts.iter().map(|&(from, to)| to.saturating_sub(from)).sum();
        if taken == 0 {
            return;
        }
        BACK_CLASS[usize::from(!self.window)].fetch_add(taken as u64, Ordering::Relaxed);
        let total = self.len as usize;
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = before.get_or_insert_with(|| pooled(total));
        if keeps() {
            // Piece by piece, and only the pieces not known current. A
            // piece the part holds whole is current from now on; of a
            // piece it holds partly only the part's bytes are taken, as
            // without the switch, and it is taken again next time (the
            // rest of it may belong to a range declared earlier, whose
            // bytes stay as they were taken).
            let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            if kept.0.is_empty() {
                kept.0 = vec![false; self.pieces(0, total.max(1)).end];
            }
            for &(from, to) in parts {
                if from >= to {
                    continue;
                }
                for piece in self.pieces(from, to) {
                    if kept.0[piece] {
                        continue;
                    }
                    let (lo, hi) = self.piece(piece);
                    let whole = from <= lo && hi <= to;
                    let (lo, hi) = (lo.max(from), hi.min(to));
                    // SAFETY: as below; `hi <= total`.
                    unsafe {
                        std::ptr::copy_nonoverlapping(self.data.add(lo), snapshot.as_mut_ptr().add(lo), hi - lo);
                    }
                    kept.0[piece] = whole;
                }
            }
            return;
        }
        if wide_back() {
            // Noted, and taken with the submission's other snapshots when
            // its refreshes are done (`take_snapshots`): the range is
            // declared from here on, so nothing changes its bytes before
            // then. The snapshot's memory stays where it is until the
            // range is done with (`copy_back_done`).
            let mut snaps = SNAPS.lock().unwrap_or_else(|e| e.into_inner());
            let (data, snapshot) = (self.data as usize, snapshot.as_mut_ptr() as usize);
            snaps.extend(parts.iter().copied().filter(|&(from, to)| from < to).map(|(from, to)| (data + from, snapshot + from, to - from)));
            return;
        }
        for &(from, to) in parts {
            if from < to {
                // SAFETY: the copy's mapping and the snapshot's allocation
                // (`pooled`) both hold `total` bytes, and `to <= total`.
                unsafe {
                    copy_wide(self.data.add(from), snapshot.as_mut_ptr().add(from), to - from);
                }
            }
        }
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written. The range must lie in ranges declared with
    /// [`Shadow::writing`] and not yet done: `Host::execute` passes the
    /// union of a submission's ranges, each byte once, then calls
    /// [`Shadow::copy_back_done`].
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        self.copy_back_with(address, len, None)
    }

    /// [`Shadow::copy_back`]; `found`, if given, is the pieces of the range
    /// that differ from the snapshot, found beforehand ([`find_all`]).
    pub(crate) fn copy_back_with(&self, address: usize, len: usize, found: Option<Vec<(usize, usize)>>) -> usize {
        if self.place.is_some() {
            return 0;
        }
        let snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        self.compared(from, to);
        if let Some(before) = &*snapshot
            && from < to
        {
            // SAFETY: `writing` set the snapshot's bytes of every declared
            // range, at the copy's offsets.
            let before = unsafe { std::slice::from_raw_parts(before.as_ptr().add(from), to - from) };
            // By 64 KiB piece: the copy compared with its bytes before the
            // submission (on several threads: gigabytes a frame, bound by
            // memory speed), title memory read only where the GPU wrote.
            #[cfg(feature = "diagnostics")]
            let scanning = std::time::Instant::now();
            let differing = match found {
                Some(found) => found,
                None => self.differing(from, to, before),
            };
            #[cfg(feature = "diagnostics")]
            let syncing = {
                use crate::diagnostics::copy_stats::{C, add};
                add(C::BackScanUs, scanning.elapsed().as_micros() as u64);
                add(C::BackDiffer, differing.len() as u64);
                std::time::Instant::now()
            };
            // Where the copy changes (the GPU wrote, and below the title's
            // bytes come in), a kept snapshot is no longer current.
            for &(at, n) in &differing {
                self.changed(at, at + n);
            }
            // Sized to each stretch read (`back_piece`): a zeroed 64 KiB per
            // range, most of whose ranges differ nowhere, was most of the
            // work for small ranges.
            let (mut title, mut mask, mut runs, mut was) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for (at, n) in differing {
                // SAFETY: inside the mapping; only the submitting thread
                // writes it.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &before[at - from..at - from + n];
                let read = |first: usize, title: &mut [u8]| crate::backend::read_into((start + at + first) as *const u8, title);
                let Some((first, n)) = back_piece(host, old, &mut title, &mut mask, &mut runs, read) else {
                    continue;
                };
                // Title memory as read, beside it with the card's bytes put
                // in: only the dwords that differ are written.
                if !runs.is_empty() {
                    was.clear();
                    was.extend(title[..n].iter().zip(&mask[..n]).map(|(now, changed)| now ^ changed));
                    written += put_changed(start + at + first, &was, &title[..n]);
                }
            }
            // Our write shows as a title write at the copy's next refresh,
            // which copies those pages once more (some dozens of MB a
            // frame). Asking about the whole range here, to note them
            // current, cost more: 19-31 askings a frame over 110-174 MB of
            // view memory, each a walk of its guards.
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::BackSyncUs, syncing.elapsed().as_micros() as u64);
        }
        written
    }

    /// Offsets `[from, to)` of the copy are compared with the snapshot: for
    /// the frame line's counts.
    pub(super) fn compared(&self, from: usize, to: usize) {
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::BackCompared, (to - from) as u64);
        let (bytes, ranges) = if self.window { (2, 5) } else { (3, 6) };
        BACK_CLASS[bytes].fetch_add((to - from) as u64, Ordering::Relaxed);
        BACK_CLASS[ranges].fetch_add(1, Ordering::Relaxed);
        if !self.window && self.len >= 1 << 20 {
            BACK_CLASS[4].fetch_add((to - from) as u64, Ordering::Relaxed);
        }
    }

    /// [`relay`]: the declared range `[address, address + len)` of a copy
    /// in the PC's memory is about to be looked through
    /// (`crate::memory::shadow::find_all`) by the thread that settles: counted as
    /// [`Shadow::copy_back_with`] counts it.
    pub(crate) fn relay_compared(&self, address: usize, len: usize) {
        let (from, to) = self.inside(address, len);
        self.compared(from, to);
    }

    /// [`relay`], for a copy in the PC's memory: offsets `[at, at + n)`
    /// are a 64 KiB piece of a declared range that differed from the
    /// snapshot when it was looked through. What the card wrote there
    /// since the snapshot was last brought up goes into title memory, from
    /// the first to the last such byte, and into the snapshot, so that the
    /// next flight's merge finds only what the card writes from now on.
    /// The copy is read once and never written: a later flight's shaders
    /// may be writing it at this moment. Under the snapshot's lock, which
    /// [`Shadow::merge_under_writes`] holds while it reads title memory
    /// and puts the title's bytes into the copy and the snapshot: the two
    /// never see each other half done. `now` and `title` are scratch
    /// memory. Returns the bytes written to title memory.
    pub(crate) fn relay_merge(&self, at: usize, n: usize, now: &mut Vec<u8>, title: &mut Vec<u8>) -> usize {
        let start = self.start as usize;
        if self.card.is_some() || n == 0 || at + n > self.len as usize {
            return 0;
        }
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let Some(before) = snapshot.as_mut() else {
            return 0;
        };
        now.clear();
        now.reserve(n);
        // SAFETY: inside the copy's mapping (checked above), read into
        // room just reserved; the card may write the mapping meanwhile, and
        // each byte read is one it held at some moment.
        unsafe {
            std::ptr::copy_nonoverlapping(self.data.add(at), now.as_mut_ptr(), n);
            now.set_len(n);
        }
        // SAFETY: the snapshot's allocation holds the copy's length
        // (`writing`), and these offsets are in a range declared and not
        // done with, whose bytes it holds; nothing else touches them
        // without the lock held here.
        let held = unsafe { std::slice::from_raw_parts_mut(before.as_mut_ptr().add(at), n) };
        let Some(first) = first_difference(now, held) else {
            return 0;
        };
        let last = last_difference(now, held).unwrap_or(first);
        let (at, n) = (at + first, last + 1 - first);
        let (now, held) = (&now[first..=last], &mut held[first..=last]);
        title.clear();
        title.resize(n, 0);
        // Title memory not there: looked at again when the next flight
        // that declared the range lands (the snapshot stays as it is).
        if !crate::backend::read_into((start + at) as *const u8, title) {
            return 0;
        }
        let was = title.clone();
        if !merge_piece(now, held, title) {
            return 0;
        }
        put_changed(start + at, &was, title)
    }

    /// The 64 KiB pieces (cut at multiples of 64 KiB of title memory) of
    /// `[from, to)` of the copy that differ from `before` (the bytes from
    /// `from` on), as (offset, bytes) in order; compared on several threads
    /// when the range is big.
    pub(super) fn differing(&self, from: usize, to: usize, before: &[u8]) -> Vec<(usize, usize)> {
        let (start, data) = (self.start as usize, self.data as usize);
        let scan = |a: usize, b: usize| -> Vec<(usize, usize)> {
            let mut found = Vec::new();
            let mut at = a;
            while at < b {
                let n = (PIECE - (start + at) % PIECE).min(b - at);
                // SAFETY: inside the mapping and snapshot slice.
                if unsafe { crate::memory::safecopy::differ((data + at) as *const u8, before[at - from..].as_ptr(), n) } {
                    found.push((at, n));
                }
                at += n;
            }
            found
        };
        const PER_THREAD: usize = 16 << 20;
        let threads = ((to - from) / PER_THREAD).clamp(1, 8);
        if threads == 1 {
            return scan(from, to);
        }
        // Cut on piece boundaries, so that every thread sees whole pieces.
        let cut = |i: usize| {
            if i == threads {
                return to;
            }
            let at = from + (to - from) / threads * i;
            (at - (start + at) % PIECE).max(from)
        };
        let scan = &scan;
        std::thread::scope(|s| {
            let parts: Vec<_> = (0..threads)
                .map(|i| {
                    let (a, b) = (cut(i), cut(i + 1));
                    s.spawn(move || scan(a, b))
                })
                .collect();
            parts
                .into_iter()
                .flat_map(|part| match part.join() {
                    Ok(found) => found,
                    Err(panic) => std::panic::resume_unwind(panic),
                })
                .collect()
        })
    }

    /// After [`Shadow::copy_back`] of a submission's ranges: every submitted
    /// step that declared one of `ranges` (`(address, end)`, as declared) is
    /// done with it (one submission runs at a time), and the snapshot with
    /// the last range.
    ///
    /// With [`relay`] several submissions may hold a range: `ranges` are
    /// those of one of them (each once), and a range is done with when the
    /// last submission that declared it is ([`Flights`]).
    pub(crate) fn copy_back_done(&self, ranges: &[(usize, usize)]) {
        if self.place.is_some() {
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if relay() {
            match self.flights.lock().unwrap_or_else(|e| e.into_inner()).done_or_clear(ranges) {
                None => pending.clear(),
                Some(free) => pending.retain(|r| free.binary_search(r).is_err()),
            }
        } else {
            pending.retain(|r| !ranges.contains(r));
        }
        let non_empty = !pending.is_empty();
        self.has_pending.store(non_empty, Ordering::Release);
        if non_empty || self.card.is_some() {
            return;
        }
        if keeps() {
            // Kept for the next submission, if it is worth it (a megabyte
            // or more) and there is room.
            let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            if !kept.1 && self.len >= 1 << 20 {
                if KEPT_BYTES.fetch_add(self.len, Ordering::Relaxed) + self.len <= kept_most() {
                    kept.1 = true;
                } else {
                    KEPT_BYTES.fetch_sub(self.len, Ordering::Relaxed);
                }
            }
            if kept.1 {
                return;
            }
            // Given back: nothing of it is current next time. (The lock
            // on the pieces ends here: `copy_back` takes it while it holds
            // the snapshot's.)
            kept.0.clear();
        } else if !crate::memory::residency::pressured() && !crate::memory::residency::critical_commit() {
            if self.before_pinned.load(Ordering::Relaxed) {
                return;
            }
            let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref bytes) = *before {
                let cap = bytes.capacity() as u64;
                if PINNED_SNAP_BYTES.fetch_add(cap, Ordering::Relaxed) + cap <= pinned_snap_most() {
                    self.before_pinned.store(true, Ordering::Relaxed);
                    return;
                }
                PINNED_SNAP_BYTES.fetch_sub(cap, Ordering::Relaxed);
            }
            if let Some(bytes) = before.take() {
                drop(before);
                give_back(bytes);
            }
            return;
        }
        if let Some(bytes) = self.before.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if self.before_pinned.swap(false, Ordering::Relaxed) {
                PINNED_SNAP_BYTES.fetch_sub(bytes.capacity() as u64, Ordering::Relaxed);
            }
            give_back(bytes);
        }
    }
}
