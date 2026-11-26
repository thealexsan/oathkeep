use std::sync::atomic::Ordering;
use std::sync::Mutex;
use crate::memory::watch::PAGE;

use super::card_runs::joined;
use super::diff::{MERGED, changed_runs, note_stale};
use super::{NOT_WATCHED, Shadow};
use super::pool::stale_pages_in;
use super::put::{under_wide, word_runs};
use super::relay::outside;

impl Shadow {
    /// Whether shaders of a submission not yet copied back may be writing
    /// the copy.
    pub(crate) fn writes_pending(&self) -> bool {
        self.has_pending.load(Ordering::Acquire)
            && !self.pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Whether one of those submissions may be writing in title memory
    /// `[from, to)`.
    pub(crate) fn writes_pending_in(&self, from: usize, to: usize) -> bool {
        self.has_pending.load(Ordering::Acquire)
            && self.pending.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|&(a, e)| a < to && e > from)
    }

    /// Whether [`Shadow::update`] of `[address, address + len)` would leave
    /// title bytes behind: a range shaders may be writing holds a page the
    /// title wrote since the copy last took it. The answer comes from the
    /// write-watch journal alone, as the submission's one asking left it
    /// (`crate::memory::watch::asked`); where the journal cannot say (memory not
    /// write-watched, a range not asked about) it is yes. A page the copy
    /// never took whole counts as written if the title ever wrote it.
    ///
    /// For submissions sent to the card without a wait
    /// (`crate::backend::late`): the ranges are those of flights in the air,
    /// which are then settled before this refresh.
    pub(crate) fn stale_under_writes(&self, address: usize, len: usize) -> bool {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return false;
        }
        let busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|&&(a, e)| a < to && e > from)
            .map(|&(a, e)| (a.max(from), e.min(to)))
            .collect();
        if busy.is_empty() {
            return false;
        }
        if self.watched.load(Ordering::Relaxed) == NOT_WATCHED {
            return true;
        }
        let base = start / PAGE;
        let copied = self.copied.read().unwrap_or_else(|e| e.into_inner());
        busy.into_iter().any(|(a, e)| match crate::memory::watch::asked(a, e) {
            Some((_, written)) => !stale_pages_in(base, &copied.pages, a, e, &written).is_empty(),
            None => true,
        })
    }

    /// Where [`Shadow::stale_under_writes`] says yes: puts the bytes the
    /// title changed into the copy, in the ranges shaders may be writing,
    /// and only those bytes, so that what the shaders write beside them
    /// stays (`crate::backend::late_exact`). For each page of such a range
    /// that the title wrote since the copy last took it, title memory as
    /// it is now is compared with what the copy held when the range was
    /// given to the shaders: the copy's CPU side for a copy in the card's
    /// memory (the changed runs go up before the next submission, each a
    /// copy of its own bytes, `runs_left` of them at most), the snapshot
    /// for the others (the changed bytes go into the copy and into the
    /// snapshot, so the compare after the card ran takes them for the
    /// title's and not the shaders').
    ///
    /// Whether the caller must still settle the flights: the journal
    /// cannot say what was written, a snapshot is missing, or `runs_left`
    /// ran out. Pages done before that stay done.
    pub(crate) fn merge_under_writes(&self, address: usize, len: usize, runs_left: &mut usize) -> bool {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return false;
        }
        let busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|&&(a, e)| a < to && e > from)
            .map(|&(a, e)| (a.max(from), e.min(to)))
            .collect();
        if busy.is_empty() {
            return false;
        }
        if self.watched.load(Ordering::Relaxed) == NOT_WATCHED {
            note_stale(0);
            return true;
        }
        // The declared ranges overlap (views of the title's whole heap from
        // dozens of offsets): as one range each page is asked about and
        // compared once. Every byte of it is in a declared range, and of a
        // copy in the PC's memory in the snapshot.
        let busy = if crate::memory::watch::light() { joined(busy) } else { busy };
        let base = start / PAGE;
        for (a, e) in busy {
            // As the refresh itself asks (`update_free`): the journal if
            // the submission's asking covered the range, else the kernel,
            // else the guards. The journal alone said nothing for copies
            // of view memory, which is guarded and not write-watched, and
            // those were all of run 99's cases: the flights were settled
            // 4-5 times a frame for "0 pages looked at".
            let Some((now, written)) = crate::memory::watch::writes(a, e, true) else {
                note_stale(1);
                return true;
            };
            let stale: Vec<usize> = {
                let copied = self.copied.read().unwrap_or_else(|e| e.into_inner());
                stale_pages_in(base, &copied.pages, a, e, &written)
            };
            if stale.is_empty() {
                continue;
            }
            // The lock of the copy's reference (the snapshot, or the
            // CPU's side of a copy in the card's memory), from before
            // title memory is read until the title's bytes are in: with
            // the copy-back on its own thread ([`relay`]) that thread
            // puts what the card wrote into title memory and into the
            // reference under the same lock, and a page read before
            // such a merge and compared after it would take the card's
            // bytes for the title's.
            let merging = self.merging.write().unwrap_or_else(|e| e.into_inner());
            let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
            let held = if self.card.is_some() {
                self.data as usize
            } else {
                before.as_mut().map_or(0, |snapshot| snapshot.as_mut_ptr() as usize)
            };
            if held == 0 {
                note_stale(3);
                return true;
            }
            // A page's part in the range, compared and put in: the runs it
            // changed; none for a page that is not there (left as it is, and
            // looked at again next time) or that had too many.
            type Done = Option<Vec<(usize, usize)>>;
            let left = std::sync::atomic::AtomicI64::new(*runs_left as i64);
            let over = std::sync::atomic::AtomicBool::new(false);
            let done: Vec<Mutex<Done>> = stale.iter().map(|_| Mutex::new(None)).collect();
            let page_of = |i: usize| -> (usize, usize) { ((stale[i] * PAGE).max(a), ((stale[i] + 1) * PAGE).min(e)) };
            let one = |i: usize| {
                let (lo, hi) = page_of(i);
                if lo < hi && !over.load(Ordering::Relaxed) {
                    // SAFETY: the reference holds the copy's bytes at the
                    // copy's offsets, and these are in a declared range; the
                    // locks held above keep out what else writes them, and
                    // no two pages are the same.
                    let runs = unsafe { self.merge_page_under_writes(lo, hi - lo, held as *mut u8, &left) };
                    match runs {
                        Ok(runs) => *done[i].lock().unwrap_or_else(|e| e.into_inner()) = runs,
                        Err(()) => over.store(true, Ordering::Relaxed),
                    }
                }
            };
            // (Not on the threads of `wide`: the thread that waits there
            // takes whatever work comes, and a landing's takes the lock.)
            if stale.len() > 2 && under_wide() {
                crate::memory::writeback_pool::each_under(stale.len(), &one);
            } else {
                (0..stale.len()).for_each(one);
            }
            *runs_left = left.into_inner().max(0) as usize;
            let mut taken = Vec::new();
            for (i, runs) in done.into_iter().enumerate() {
                let Some(runs) = runs.into_inner().unwrap_or_else(|e| e.into_inner()) else {
                    continue;
                };
                let (lo, hi) = ((stale[i] * PAGE).max(a), ((stale[i] + 1) * PAGE).min(e));
                MERGED[1].fetch_add(runs.len() as u64, Ordering::Relaxed);
                MERGED[2].fetch_add(runs.iter().map(|r| r.1 as u64).sum(), Ordering::Relaxed);
                self.card_changed_runs(runs.into_iter().map(|(r, m)| (lo - start + r, lo - start + r + m)));
                // Taken, if the range holds all of the page that is in the
                // copy; else the rest of it is the refresh's, which notes
                // only whole pages, and the page is looked at again.
                if lo < hi && (stale[i] * PAGE).max(start) >= a && ((stale[i] + 1) * PAGE).min(end) <= e {
                    taken.push(stale[i]);
                }
            }
            drop(before);
            drop(merging);
            {
                let mut copied = self.copied.write().unwrap_or_else(|e| e.into_inner());
                for page in taken {
                    if let Some(at) = copied.pages.get_mut(page - base) {
                        *at = now;
                    }
                }
            }
            if over.into_inner() {
                note_stale(2);
                return true;
            }
        }
        false
    }

    /// Of [`Shadow::merge_under_writes`]: title memory `[lo, lo + n)`, in one
    /// page, compared with the reference, which begins at `held`, and put in.
    /// The runs that changed, none if it cannot be read, and an error if
    /// there were more to copy one by one than `left`.
    ///
    /// # Safety
    /// The reference holds the copy's length, and nothing else writes these
    /// bytes of it during the call.
    pub(super) unsafe fn merge_page_under_writes(
        &self,
        lo: usize,
        n: usize,
        held: *mut u8,
        left: &std::sync::atomic::AtomicI64,
    ) -> Result<Option<Vec<(usize, usize)>>, ()> {
        thread_local! {
            static TITLE: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        TITLE.with_borrow_mut(|title| {
            title.resize(PAGE, 0);
            let title = &mut title[..n];
            if !crate::backend::read_into(lo as *const u8, title) {
                return Ok(None);
            }
            MERGED[0].fetch_add(1, Ordering::Relaxed);
            let at = lo - self.start as usize;
            // SAFETY: the caller's.
            let held = unsafe { std::slice::from_raw_parts_mut(held.add(at), n) };
            if self.card.is_some() {
                // Whole words go up as pairs, all of them in one
                // dispatch: only what is a copy of its own counts.
                let runs = if crate::backend::words_under() { word_runs(lo, title, held) } else { changed_runs(title, held) };
                let own = |&&(r, m): &&(usize, usize)| !crate::backend::words_under() || !crate::backend::goes_as_words(at + r, m);
                let copies = runs.iter().filter(own).count() as i64;
                if copies > 0 && left.fetch_sub(copies, Ordering::Relaxed) < copies {
                    return Err(());
                }
                let was = (!runs.is_empty() && crate::backend::upload_verify::on()).then(|| held.to_vec());
                for &(r, m) in &runs {
                    held[r..r + m].copy_from_slice(&title[r..r + m]);
                }
                if let Some(was) = was {
                    crate::backend::upload_verify::note(self.id, at, &was, held);
                }
                return Ok(Some(runs));
            }
            let runs = changed_runs(title, held);
            for &(r, m) in &runs {
                // SAFETY: inside the copy's mapping. The card may be
                // writing the copy beside these bytes; they are
                // written here and nothing around them.
                unsafe { std::ptr::copy_nonoverlapping(title.as_ptr().add(r), self.data.add(at + r), m) };
                held[r..r + m].copy_from_slice(&title[r..r + m]);
            }
            Ok(Some(runs))
        })
    }

    /// [`Shadow::card_merge_relay`] of several pieces, each within one
    /// 64 KiB block of title memory: from and to (offsets in the copy), and
    /// where the fetched bytes are. On any thread: several may land pieces
    /// of one copy, no two the same bytes. Returns the bytes put into title
    /// memory.
    ///
    /// What is left out is as there, asked once for all of them: the notes
    /// of bytes that went up (or are to go up) behind the fetch only move
    /// from `dirty` to `sent`, with a later number, and grow only by
    /// [`Shadow::merge_under_writes`], which the lock held here keeps out.
    ///
    /// # Safety
    /// The third of each piece is the address of its bytes, which stay as
    /// they are during the call.
    pub(crate) unsafe fn land_card(&self, pieces: &[(usize, usize, usize)], fetched_after: u64, notes: &mut crate::memory::writeback_pool::Notes) -> u64 {
        let Some(card) = &self.card else {
            return 0;
        };
        let waiting = std::time::Instant::now();
        let _merging = self.merging.read().unwrap_or_else(|e| e.into_inner());
        crate::memory::writeback_pool::spent(crate::memory::writeback_pool::stat::LOCK, waiting);
        let (from, to) = pieces.iter().fold((usize::MAX, 0), |(a, b), p| (a.min(p.0), b.max(p.1)));
        let skip: Vec<(usize, usize)> = {
            let dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
            let sent = card.sent.lock().unwrap_or_else(|e| e.into_inner());
            let mut skip: Vec<(usize, usize)> = dirty.iter().copied().filter(|&(a, b)| a < to && b > from).collect();
            sent.near(fetched_after, from, to, &mut skip);
            skip
        };
        let mut put = 0;
        let mut landed_ranges: Vec<(usize, usize)> = Vec::new();
        let mut parts_buf: Vec<(usize, usize)> = Vec::new();
        for &(lo, hi, now) in pieces {
            if lo >= hi || hi > self.len as usize {
                continue;
            }
            let whole = [(0usize, hi - lo)];
            let parts: &[(usize, usize)] = if skip.is_empty() {
                &whole
            } else {
                let near: Vec<(usize, usize)> = skip.iter().copied().filter(|&(a, b)| a < hi && b > lo).collect();
                if near.is_empty() {
                    &whole
                } else {
                    parts_buf.clear();
                    parts_buf.extend(outside(lo, hi, near).into_iter().map(|(a, b)| (a - lo, b - lo)));
                    &parts_buf
                }
            };
            // SAFETY: inside the mapping (checked above), in a range
            // declared and not done with, which no refresh writes; the
            // title's bytes are kept out by the lock; the caller's.
            // Where title memory is not mapped the CPU's side keeps what
            // the card wrote, as before.
            let merged =
                unsafe { crate::memory::writeback_pool::piece(self.start as usize + lo, now as *const u8, self.data.add(lo), parts, true, notes, self) };
            self.walls_merged(hi - lo, merged as usize);
            if merged > 0 {
                landed_ranges.push((lo, hi));
            }
            put += merged;
        }
        if !landed_ranges.is_empty() {
            self.note_landings(&landed_ranges);
        }
        put
    }

    /// [`Shadow::relay_merge`] of a piece `[at, at + n)` (within one 64 KiB
    /// block of title memory) of a copy in the PC's memory; on any thread,
    /// as [`Shadow::land_card`]. The copy is read once and never written.
    /// Returns the bytes put into title memory.
    pub(crate) fn land_back(&self, at: usize, n: usize, notes: &mut crate::memory::writeback_pool::Notes) -> u64 {
        if self.card.is_some() || n == 0 || at + n > self.len as usize {
            return 0;
        }
        let waiting = std::time::Instant::now();
        let _merging = self.merging.read().unwrap_or_else(|e| e.into_inner());
        crate::memory::writeback_pool::spent(crate::memory::writeback_pool::stat::LOCK, waiting);
        // The snapshot's memory stays where it is until the range is done
        // with (`copy_back_done`), which comes after this.
        let Some(held) = self.before.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(Vec::as_mut_ptr) else {
            return 0;
        };
        // SAFETY: inside the copy's mapping (checked above), which only the
        // card writes there, and inside the snapshot's allocation, which
        // holds the copy's length (`writing`): these offsets are in a range
        // declared and not done with, whose bytes it holds, and the lock
        // keeps out what else writes them. Title memory that is not there
        // is looked at again when the next flight that declared the range
        // lands.
        unsafe { crate::memory::writeback_pool::piece(self.start as usize + at, self.data.add(at), held.add(at), &[(0, n)], false, notes, self) }
    }

    /// What shaders wrote in 64 KiB block `block` (of title memory) of this
    /// copy was put into title memory beside the guard, which was noted as a
    /// write with number `now`, `before` the number of the block's last write
    /// before. If the copy held title memory's bytes of the block ahead of
    /// that it holds them still, and is noted to ([`crate::memory::writeback_pool`] says why);
    /// whether it was.
    ///
    /// That it did: it took the whole block after that write, and no other
    /// came that is not noted yet. Looked at after the note, whatever comes
    /// later has a number above it: a write of the title's takes the block's
    /// bit, until an asking has noted it.
    pub(crate) fn landed(&self, block: usize, before: u64, now: u64) -> bool {
        let Some(page) = block.checked_sub(self.start as usize / PAGE) else {
            return false;
        };
        let mut copied = self.copied.write().unwrap_or_else(|e| e.into_inner());
        let Some(at) = copied.pages.get_mut(page) else {
            return false;
        };
        let held = *at != 0 && *at >= before && crate::memory::watch::block_published(block);
        if held {
            *at = (*at).max(now);
        }
        held
    }
}
