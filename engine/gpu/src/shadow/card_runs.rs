use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use crate::watch::PAGE;

use super::card_alloc::CARDED;
use super::loose::LOOSE;
use super::{PIECE, Shadow};
use super::pool::{piece_at, pieces_of};
use super::registry::buffers_read;
use super::relay::{RELAY_FETCHED, outside, relay};
use super::snap::{SNAP_BATCH, SNAP_CHUNK, coalesce_copy_ranges, pack_copy_batches, split_copy_chunks};

/// What went up, by send: its number, and the offsets, in order and joined
/// (a refresh sends thousands of runs of a few words).
#[derive(Default)]
pub(super) struct SentRuns(Vec<(u64, Vec<(usize, usize)>)>);

impl SentRuns {
    #[cfg(test)]
    pub(super) fn note(&mut self, number: u64, ranges: Vec<(usize, usize)>) {
        self.note_joined(number, joined(ranges));
    }

    /// `ranges` are in order and joined.
    pub(super) fn note_joined(&mut self, number: u64, ranges: Vec<(usize, usize)>) {
        if ranges.is_empty() {
            return;
        }
        if let Some((_, all)) = self.0.last_mut().filter(|s| s.0 == number) {
            all.extend(ranges);
            *all = joined(std::mem::take(all));
            return;
        }
        match self.0.iter_mut().find(|s| s.0 == number) {
            Some((_, all)) => {
                all.extend(ranges);
                *all = joined(std::mem::take(all));
            }
            None => self.0.push((number, ranges)),
        }
    }

    /// Sends up to `number` are of no use any more.
    pub(super) fn forget(&mut self, number: u64) {
        self.0.retain(|s| s.0 > number);
    }

    /// Adds to `out` what sends after `number` took of `[from, to)`.
    pub(super) fn near(&self, number: u64, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
        for (_, ranges) in self.0.iter().filter(|s| s.0 > number) {
            let first = ranges.partition_point(|r| r.1 <= from);
            out.extend(ranges[first..].iter().take_while(|r| r.0 < to).copied());
        }
    }

    #[cfg(test)]
    pub(super) fn of(all: &[(usize, usize, u64)]) -> SentRuns {
        let mut sent = SentRuns::default();
        for &(a, b, number) in all {
            sent.note(number, vec![(a, b)]);
        }
        sent
    }
}

/// The copies in the card's memory whose CPU side changed since they were
/// last asked: each with the changed offsets, in order and joined where
/// they touch. The ranges are taken: the caller copies them to the card's
/// side (`Host::card_before`).
///
/// With [`relay`], `sent_as` is the number of the send that takes them
/// (`crate::host`'s order of the queue): each range is noted with it, in
/// the same lock that took it, so that a merge of pieces fetched before
/// that send knows the card did not hold these bytes yet
/// ([`Shadow::card_merge_relay`]). Notes of sends that every fetch still to
/// be merged came after are dropped.
pub(crate) fn card_dirty(sent_as: u64) -> Vec<(Arc<Shadow>, Vec<(usize, usize)>)> {
    let mut out = Vec::new();
    let mut carded = CARDED.lock().unwrap_or_else(|e| e.into_inner());
    let fetched = RELAY_FETCHED.load(Ordering::Acquire);
    carded.retain(|weak| {
        let Some(shadow) = weak.upgrade() else {
            return false;
        };
        let ranges = shadow
            .card
            .as_ref()
            .filter(|c| c.touched.load(Ordering::Acquire) && c.touched.swap(false, Ordering::AcqRel))
            .map(|c| {
                let mut dirty = c.dirty.lock().unwrap_or_else(|e| e.into_inner());
                let ranges = joined(std::mem::take(&mut *dirty));
                if relay() {
                    let mut sent = c.sent.lock().unwrap_or_else(|e| e.into_inner());
                    sent.forget(fetched);
                    sent.note_joined(sent_as, ranges.clone());
                }
                ranges
            })
            .unwrap_or_default();
        if !ranges.is_empty() {
            out.push((shadow, ranges));
        }
        true
    });
    out
}

/// Diagnostics (`crate::host::upcheck`, nothing in the air): copies whose
/// pages the write watch and the guards hold for unchanged since they were
/// taken, compared with title memory, `most` bytes a call in turn. `say` gets
/// the copy, the title address of a page that is not the same, the words that
/// differ and the first of them (its address, the copy's value, title
/// memory's). Returns the bytes compared.
pub(crate) fn stale_pages(most: usize, say: &mut dyn FnMut(&Shadow, usize, usize, (usize, u32, u32))) -> usize {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut all: Vec<Arc<Shadow>> = buffers_read().values().filter_map(|e| e.shadow.clone()).collect();
    all.extend(LOOSE.write().unwrap_or_else(|e| e.into_inner()).values().cloned());
    all.retain(|s| s.place.is_none() && s.watched.load(Ordering::Relaxed) == 1);
    all.sort_by_key(|s| s.id);
    let next = NEXT.load(Ordering::Relaxed);
    let turn = all.partition_point(|s| s.id < next);
    let mut done = 0;
    let mut title = vec![0u8; PAGE];
    for shadow in all[turn..].iter().chain(&all[..turn]) {
        if done >= most {
            NEXT.store(shadow.id, Ordering::Relaxed);
            break;
        }
        let (start, end) = (shadow.start as usize, (shadow.start + shadow.len) as usize);
        // (What it finds written stays noted for the next refresh.)
        let Some((_, written)) = crate::watch::writes(start, end, true) else {
            continue;
        };
        let written: std::collections::HashMap<usize, u64> = written.into_iter().collect();
        let copied = shadow.copied.read().unwrap_or_else(|e| e.into_inner()).pages.clone();
        for (i, &at) in copied.iter().enumerate() {
            let page = start / PAGE + i;
            if at == 0 || written.get(&page).is_some_and(|&w| w > at) {
                continue;
            }
            let (lo, hi) = ((page * PAGE).max(start), ((page + 1) * PAGE).min(end));
            let title = &mut title[..hi - lo];
            if !crate::host::read_into(lo as *const u8, title) {
                continue;
            }
            done += hi - lo;
            // SAFETY: inside the mapping; nothing is in the air and the
            // submission lock is held.
            let held = unsafe { std::slice::from_raw_parts(shadow.data.add(lo - start).cast_const(), hi - lo) };
            if held == title {
                continue;
            }
            let word = |b: &[u8], k: usize| u32::from_le_bytes([b[k], b[k + 1], b[k + 2], b[k + 3]]);
            let differ: Vec<usize> = (0..(hi - lo) & !3).step_by(4).filter(|&k| word(held, k) != word(title, k)).collect();
            // Written meanwhile: not stale, and the next refresh sees it.
            let again = crate::watch::writes(lo, hi, true).is_none_or(|(_, w)| w.iter().any(|&(p, w)| p == page && w > at));
            if let Some(&k) = differ.first()
                && !again
            {
                say(shadow, lo, differ.len(), (lo + k, word(held, k), word(title, k)));
            }
        }
    }
    done
}

/// Copies of memory (from, to, bytes: apart), on several threads if they are
/// many.
#[allow(dead_code)]
pub(crate) fn copy_all(mut copies: Vec<(usize, usize, usize)>) {
    coalesce_copy_ranges(&mut copies);
    let bytes: usize = copies.iter().map(|c| c.2).sum();
    if bytes >= 192 << 10 {
        let chunks = split_copy_chunks(copies, SNAP_CHUNK);
        let batches = pack_copy_batches(&chunks, SNAP_BATCH);
        crate::land::each_under(batches.len(), &|b| {
            for &(src, dst, len) in &chunks[batches[b].clone()] {
                // SAFETY: disjoint source and destination ranges owned by caller.
                unsafe { std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, len) };
            }
        });
        return;
    }
    for (src, dst, len) in copies {
        // SAFETY: the caller's ranges.
        unsafe { std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, len) };
    }
}

/// A title buffer copy was done on the CPU into `[dst, dst + len)`: all of it
/// goes up to the copies in the card's memory that hold it, whether their
/// CPU side held the same before or not. Left out is what shaders in the air
/// may be writing (such a copy waits for them, or is done on the card).
pub(crate) fn send_copied(dst: usize, len: usize) {
    for shadow in carded_copies() {
        let (start, end) = (shadow.start as usize, (shadow.start + shadow.len) as usize);
        let (from, to) = (dst.max(start), (dst + len).min(end));
        if from >= to {
            continue;
        }
        let busy: Vec<(usize, usize)> =
            shadow.pending.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().filter(|&(a, b)| a < to && b > from).collect();
        for (a, b) in outside(from, to, busy) {
            shadow.copy(a, b);
        }
    }
}

/// The copies in the card's memory (diagnostics: `crate::host::upcheck`).
pub(crate) fn carded_copies() -> Vec<Arc<Shadow>> {
    CARDED.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(std::sync::Weak::upgrade).collect()
}

/// Ranges in order, those that overlap or touch as one.
pub(super) fn joined(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    // (The sort that finds what is in order already: they come as a few
    // stretches of tens of thousands in order, put behind one another.)
    ranges.sort();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

/// Runs of pieces to hash again (`Host::card_before`), those that overlap
/// or touch as one, in order: every piece of `runs` is in the result once
/// and no other piece is. Ranges sent up lie close together (514 a frame
/// for 87 MB in run 102), and each had a pass of its own over its pieces,
/// so a piece that held several ranges was hashed once for each.
pub(crate) fn joined_piece_runs(mut runs: Vec<CardSpan>) -> Vec<CardSpan> {
    runs.sort_unstable_by_key(|r| r.first);
    let mut out: Vec<CardSpan> = Vec::new();
    for run in runs {
        match out.last_mut() {
            Some(last) if run.first <= last.first + last.pieces => {
                last.pieces = last.pieces.max(run.first + run.pieces - last.first);
                last.to = last.to.max(run.to);
            }
            _ => out.push(run),
        }
    }
    out
}

/// A declared range of a copy in the card's memory, as the compare on the
/// card takes it: its offsets in the copy, its first 64 KiB piece (pieces
/// are cut at multiples of 64 KiB of title memory; piece 0 holds offset
/// 0), how many pieces hold it, and where the copy starts in piece 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CardSpan {
    pub(crate) from: u32,
    pub(crate) to: u32,
    pub(crate) first: u32,
    pub(crate) pieces: u32,
    pub(crate) skew: u32,
}

/// [`CardSpan`] for offsets `[from, to)` (`from < to`) of a copy of title
/// memory from `start` on.
pub(super) fn card_span_of(start: usize, from: usize, to: usize) -> CardSpan {
    let pieces = pieces_of(start, from, to);
    CardSpan {
        from: from as u32,
        to: to as u32,
        first: pieces.start as u32,
        pieces: pieces.len() as u32,
        skew: (start % PIECE) as u32,
    }
}

/// The offsets of piece `i` of `span` that lie in the span, for a copy of
/// `len` bytes of title memory from `start` on: what is fetched and merged
/// of a piece the compare found differing.
pub(super) fn card_piece_of(start: usize, len: usize, span: &CardSpan, i: u32) -> (usize, usize) {
    let (lo, hi) = piece_at(start, len, (span.first + i) as usize);
    (lo.max(span.from as usize), hi.min(span.to as usize))
}

/// The offsets [`Shadow::card_hashed`] gives, for a card's buffer of `size`
/// bytes for title memory from `start` on.
pub(super) fn card_hashed_of(start: usize, size: usize, piece: usize) -> (usize, usize) {
    let (lo, hi) = piece_at(start, size, piece);
    (lo & !15, hi.next_multiple_of(16))
}
