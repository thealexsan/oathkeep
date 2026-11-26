use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::{HostList, host};
use crate::watch::PAGE;

pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    host_width: u64,
    allow_uav: bool,
    data: *mut u8,
    gpu: u64,
    start: u64,
    len: u64,
    copied: Mutex<CopiedState>,
    uncopied: AtomicU64,
    synced_at: AtomicU64,
    clean_guard_epochs: [AtomicU64; crate::watch::CLEAN_EPOCH_SHARDS],
    ever_written: AtomicBool,
    watched: AtomicU8,
    dead: AtomicBool,
    has_pending: AtomicBool,
    pending: Mutex<Vec<(usize, usize)>>,
    flights: Mutex<Flights>,
    before: Mutex<Option<Vec<u8>>>,
    kept: Mutex<(Vec<bool>, bool)>,
    scanned: Mutex<ScannedSubmission>,
    used: AtomicU64,
    asked: [AtomicU64; 2],
    unsized_asks: AtomicU64,
    id: u64,
    card: Option<Card>,
    window: bool,
    place: Option<(crate::inplace::PlaceHeap, u64)>,
}

#[derive(Default)]
struct ScannedSubmission {
    submission: u64,
    stamp: crate::watch::WriteStamp,
    spans: Vec<(usize, usize)>,
}

impl ScannedSubmission {
    fn sync(&mut self, submission: u64, now: crate::watch::WriteStamp) {
        if self.submission != submission || self.stamp.global != now.global {
            self.submission = submission;
            self.stamp = now;
            self.spans.clear();
            return;
        }
        if self.stamp.ranged == now.ranged {
            return;
        }
        match crate::watch::written_ranges_since(self.stamp, now) {
            Some(ranges) => {
                for (wa, wb) in ranges {
                    self.subtract(wa, wb);
                }
                self.stamp = now;
            }
            None => {
                self.stamp = now;
                self.spans.clear();
            }
        }
    }

    fn subtract(&mut self, wa: usize, wb: usize) {
        if wa >= wb || self.spans.is_empty() {
            return;
        }
        let mut i = self.spans.partition_point(|&(_, b)| b <= wa);
        while i < self.spans.len() && self.spans[i].0 < wb {
            let (a, b) = self.spans[i];
            if a < wa && b > wb {
                self.spans[i] = (a, wa);
                self.spans.insert(i + 1, (wb, b));
                break;
            } else if a < wa {
                self.spans[i].1 = wa;
                i += 1;
            } else if b > wb {
                self.spans[i].0 = wb;
                break;
            } else {
                self.spans.remove(i);
            }
        }
    }

    fn uncovered(
        &mut self,
        submission: u64,
        now: crate::watch::WriteStamp,
        from: usize,
        to: usize,
    ) -> Vec<(usize, usize)> {
        if from >= to {
            return Vec::new();
        }
        self.sync(submission, now);
        if self.spans.is_empty() {
            return vec![(from, to)];
        }
        let mut out = Vec::new();
        let mut cur = from;
        let idx = self.spans.partition_point(|&(_, b)| b <= from);
        for &(a, b) in &self.spans[idx..] {
            if a >= to {
                break;
            }
            if a > cur {
                out.push((cur, a));
            }
            cur = cur.max(b);
            if cur >= to {
                return out;
            }
        }
        if cur < to {
            out.push((cur, to));
        }
        out
    }

    fn record(
        &mut self,
        submission: u64,
        now: crate::watch::WriteStamp,
        from: usize,
        to: usize,
    ) {
        if from >= to {
            return;
        }
        self.sync(submission, now);
        let mut lo = from;
        let mut hi = to;
        let mut first = self.spans.len();
        let mut last = 0usize;
        let mut merged = false;
        for (i, &(a, b)) in self.spans.iter().enumerate() {
            if hi >= a && lo <= b {
                lo = lo.min(a);
                hi = hi.max(b);
                first = first.min(i);
                last = i + 1;
                merged = true;
            }
        }
        if merged {
            self.spans.splice(first..last, [(lo, hi)]);
        } else {
            let at = self.spans.partition_point(|&(a, _)| a < lo);
            self.spans.insert(at, (lo, hi));
        }
    }

    fn clear(&mut self) {
        self.submission = 0;
        self.spans.clear();
    }
}

const HOT_HASH_BLOCK: usize = 4096;
const COPIED_CHUNK: usize = 64;

struct CleanSubEntry {
    shards: [usize; crate::watch::CLEAN_EPOCH_SHARDS],
    epochs: [u64; crate::watch::CLEAN_EPOCH_SHARDS],
    count: usize,
    spans: Vec<(usize, usize)>,
}

const MAX_CLEAN_SUB_ENTRIES: usize = 64;

struct CopiedState {
    pages: Vec<u64>,
    /// Per-64-page-chunk minimum timestamp (`0` iff any page in the chunk is uncopied).
    chunk_min: Vec<u64>,
    /// Per-64-page-chunk minimum non-zero timestamp (`u64::MAX` iff all pages are `0`).
    chunk_min_nz: Vec<u64>,
    /// Lazy floor applied to non-zero pages in a fully-copied chunk (`chunk_min[c] > 0`).
    chunk_floor: Vec<u64>,
    /// Lazily allocated per-64 KiB page quiet tracking for non-card `copy_hot` cooling.
    hot_quiet: Vec<HotPageQuiet>,
    /// Multi-shard subrange clean-guard cache entries (up to `MAX_CLEAN_SUB_ENTRIES` shard-sets
    /// per shadow, eliminating ping-pong eviction across shards of large buffers).
    clean_subs: Vec<CleanSubEntry>,
}

impl CopiedState {
    fn new(pages: usize) -> Self {
        let chunks = pages.div_ceil(COPIED_CHUNK);
        Self {
            pages: vec![0; pages],
            chunk_min: vec![0; chunks],
            chunk_min_nz: vec![u64::MAX; chunks],
            chunk_floor: vec![0; chunks],
            hot_quiet: Vec::new(),
            clean_subs: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.pages.clear();
        self.chunk_min.clear();
        self.chunk_min_nz.clear();
        self.chunk_floor.clear();
        self.hot_quiet.clear();
        self.clean_subs.clear();
    }

    fn clean_covers_shards(
        &self,
        shards: [usize; crate::watch::CLEAN_EPOCH_SHARDS],
        count: usize,
        lo: usize,
        hi: usize,
        since: u64,
    ) -> bool {
        if lo >= hi || count == 0 || since == 0 {
            return false;
        }
        let Some(entry) = self
            .clean_subs
            .iter()
            .find(|e| e.count == count && e.shards[..count] == shards[..count])
        else {
            return false;
        };
        if !crate::watch::guarded_shards_epochs_match(
            &entry.shards[..count],
            &entry.epochs[..count],
            since,
        ) {
            return false;
        }
        entry.spans.iter().any(|&(a, b)| a <= lo && hi <= b)
    }

    fn record_clean_shards(
        &mut self,
        shards: [usize; crate::watch::CLEAN_EPOCH_SHARDS],
        count: usize,
        epochs: [u64; crate::watch::CLEAN_EPOCH_SHARDS],
        lo: usize,
        hi: usize,
    ) {
        if lo >= hi || count == 0 || epochs[0] == 0 {
            return;
        }
        if let Some(entry) = self
            .clean_subs
            .iter_mut()
            .find(|e| e.count == count && e.shards[..count] == shards[..count])
        {
            if entry.epochs[..count] != epochs[..count] {
                entry.epochs = epochs;
                entry.spans.clear();
                entry.spans.push((lo, hi));
                return;
            }
            let spans = &mut entry.spans;
            if spans.len() >= 64 {
                spans.clear();
            }
            let mut new_lo = lo;
            let mut new_hi = hi;
            let mut merged = Vec::with_capacity(spans.len() + 1);
            let mut inserted = false;
            for &(a, b) in spans.iter() {
                if b < new_lo {
                    merged.push((a, b));
                } else if new_hi < a {
                    if !inserted {
                        merged.push((new_lo, new_hi));
                        inserted = true;
                    }
                    merged.push((a, b));
                } else {
                    new_lo = new_lo.min(a);
                    new_hi = new_hi.max(b);
                }
            }
            if !inserted {
                merged.push((new_lo, new_hi));
            }
            *spans = merged;
            return;
        }
        if self.clean_subs.len() >= MAX_CLEAN_SUB_ENTRIES {
            self.clean_subs.remove(0);
        }
        self.clean_subs.push(CleanSubEntry {
            shards,
            epochs,
            count,
            spans: vec![(lo, hi)],
        });
    }

    fn record_hot_quiet(
        &mut self,
        idx: usize,
        frame: u64,
        changed: bool,
        full_page: bool,
        cool_after: u32,
    ) -> bool {
        if cool_after == 0 {
            return false;
        }
        if self.hot_quiet.is_empty() {
            self.hot_quiet.resize(self.pages.len(), HotPageQuiet::default());
        }
        let Some(slot) = self.hot_quiet.get_mut(idx) else {
            return false;
        };
        slot.record(frame, changed, true, full_page, cool_after)
    }

    #[inline]
    fn mark_hot_changed(&mut self, idx: usize, frame: u64) {
        if let Some(slot) = self.hot_quiet.get_mut(idx) {
            slot.mark_changed(frame);
        }
    }

    fn len(&self) -> usize {
        self.pages.len()
    }

    #[inline]
    fn get(&self, idx: usize, global_synced: u64) -> u64 {
        let v = self.pages[idx];
        if v == 0 {
            0
        } else {
            v.max(self.chunk_floor[idx / COPIED_CHUNK])
                .max(global_synced)
        }
    }

    /// Sets `pages[idx]` to at least `now` (if `now != 0`) without recomputing chunk summaries yet.
    /// Returns `true` if the page transitioned from `0` to non-zero.
    #[inline]
    fn set_raw(&mut self, idx: usize, now: u64) -> bool {
        if now == 0 {
            return false;
        }
        let slot = &mut self.pages[idx];
        let was_zero = *slot == 0;
        if *slot < now {
            *slot = now;
        }
        was_zero
    }

    fn recompute_chunk(&mut self, c: usize) {
        let lo = c * COPIED_CHUNK;
        let hi = (lo + COPIED_CHUNK).min(self.pages.len());
        if lo >= hi {
            return;
        }
        let floor = self.chunk_floor[c];
        let mut min_all = u64::MAX;
        let mut min_nz = u64::MAX;
        for &raw in &self.pages[lo..hi] {
            if raw == 0 {
                min_all = 0;
            } else {
                let eff = raw.max(floor);
                min_all = min_all.min(eff);
                min_nz = min_nz.min(eff);
            }
        }
        self.chunk_min[c] = if min_all == u64::MAX { 0 } else { min_all };
        self.chunk_min_nz[c] = min_nz;
    }

    fn set_page(&mut self, idx: usize, now: u64) -> bool {
        let was_zero = self.set_raw(idx, now);
        if now != 0 {
            self.recompute_chunk(idx / COPIED_CHUNK);
        }
        was_zero
    }

    /// Returns `(all_copied, since)` for page indices `lo..hi`.
    /// - `all_copied` is `true` iff every page in `lo..hi` has been copied (`!= 0`).
    /// - `since` is the minimum non-zero timestamp in `lo..hi`, or `u64::MAX` if all pages are `0`.
    fn range_since(&self, lo: usize, hi: usize, global_synced: u64) -> (bool, u64) {
        let hi = hi.min(self.pages.len());
        if lo >= hi {
            return (true, global_synced);
        }
        let mut all_copied = true;
        let mut min_nz = u64::MAX;
        let mut i = lo;

        // Head partial chunk.
        let first_chunk = i / COPIED_CHUNK;
        let first_end = ((first_chunk + 1) * COPIED_CHUNK).min(hi);
        if i % COPIED_CHUNK != 0 || first_end < (first_chunk + 1) * COPIED_CHUNK {
            let floor = self.chunk_floor[first_chunk];
            for &raw in &self.pages[i..first_end] {
                if raw == 0 {
                    all_copied = false;
                } else {
                    min_nz = min_nz.min(raw.max(floor));
                }
            }
            i = first_end;
        }

        // Full middle chunks in O(1) per 4 MiB chunk.
        while i + COPIED_CHUNK <= hi {
            let c = i / COPIED_CHUNK;
            let cm = self.chunk_min[c];
            if cm == 0 {
                all_copied = false;
                let nz = self.chunk_min_nz[c];
                if nz < min_nz {
                    min_nz = nz;
                }
            } else if cm < min_nz {
                min_nz = cm;
            }
            i += COPIED_CHUNK;
        }

        // Tail partial chunk.
        if i < hi {
            let c = i / COPIED_CHUNK;
            let floor = self.chunk_floor[c];
            for &raw in &self.pages[i..hi] {
                if raw == 0 {
                    all_copied = false;
                } else {
                    min_nz = min_nz.min(raw.max(floor));
                }
            }
        }

        if min_nz != u64::MAX {
            min_nz = min_nz.max(global_synced);
        }
        (all_copied, min_nz)
    }

    /// Raises every already-copied (non-zero) page in `lo..hi` to at least `now`.
    /// Never turns a `0` (uncopied) page into non-zero.
    fn advance_synced(&mut self, lo: usize, hi: usize, now: u64) {
        let hi = hi.min(self.pages.len());
        if now == 0 || lo >= hi {
            return;
        }
        let mut i = lo;
        let first_chunk = i / COPIED_CHUNK;
        let first_end = ((first_chunk + 1) * COPIED_CHUNK).min(hi);
        if i % COPIED_CHUNK != 0 || first_end < (first_chunk + 1) * COPIED_CHUNK {
            if self.chunk_min_nz[first_chunk] < now {
                let mut touched = false;
                for slot in &mut self.pages[i..first_end] {
                    if *slot != 0 && *slot < now {
                        *slot = now;
                        touched = true;
                    }
                }
                if touched {
                    self.recompute_chunk(first_chunk);
                }
            }
            i = first_end;
        }

        while i + COPIED_CHUNK <= hi {
            let c = i / COPIED_CHUNK;
            if self.chunk_min[c] > 0 {
                if self.chunk_floor[c] < now {
                    self.chunk_floor[c] = now;
                }
                if self.chunk_min[c] < now {
                    self.chunk_min[c] = now;
                }
                if self.chunk_min_nz[c] < now {
                    self.chunk_min_nz[c] = now;
                }
            } else if self.chunk_min_nz[c] < now {
                let end = i + COPIED_CHUNK;
                let mut touched = false;
                for slot in &mut self.pages[i..end] {
                    if *slot != 0 && *slot < now {
                        *slot = now;
                        touched = true;
                    }
                }
                if touched {
                    self.recompute_chunk(c);
                }
            }
            i += COPIED_CHUNK;
        }

        if i < hi {
            let c = i / COPIED_CHUNK;
            if self.chunk_min_nz[c] < now {
                let mut touched = false;
                for slot in &mut self.pages[i..hi] {
                    if *slot != 0 && *slot < now {
                        *slot = now;
                        touched = true;
                    }
                }
                if touched {
                    self.recompute_chunk(c);
                }
            }
        }
    }

    #[cfg(feature = "diagnostics")]
    fn copied_page_count(&self) -> u64 {
        let mut total = 0u64;
        let len = self.pages.len();
        for (c, &cm) in self.chunk_min.iter().enumerate() {
            let lo = c * COPIED_CHUNK;
            let hi = (lo + COPIED_CHUNK).min(len);
            if cm > 0 {
                total += (hi - lo) as u64;
            } else if self.chunk_min_nz[c] != u64::MAX {
                total += self.pages[lo..hi].iter().filter(|&&at| at != 0).count() as u64;
            }
        }
        total
    }

    fn stale_pages_in(
        &self,
        base: usize,
        global_synced: u64,
        a: usize,
        e: usize,
        written: &[(usize, u64)],
    ) -> Vec<usize> {
        (a / PAGE..e.div_ceil(PAGE))
            .filter(|&page| page >= base && page - base < self.pages.len())
            .filter(|&page| match self.get(page - base, global_synced) {
                0 => true,
                at => written
                    .binary_search_by_key(&page, |&(p, _)| p)
                    .is_ok_and(|i| written[i].1 > at),
            })
            .collect()
    }
}

fn full_page_range(
    start: usize,
    end: usize,
    from: usize,
    to: usize,
    base: usize,
    pages_len: usize,
) -> (usize, usize) {
    if from >= to || pages_len == 0 {
        return (0, 0);
    }
    let first_p = from / PAGE;
    let last_p = (to - 1) / PAGE;
    let full_first = if (first_p * PAGE).max(start) >= from {
        first_p
    } else {
        first_p + 1
    };
    let full_end = if ((last_p + 1) * PAGE).min(end) <= to {
        last_p + 1
    } else {
        last_p
    };
    if full_first >= full_end {
        return (0, 0);
    }
    let lo = full_first.saturating_sub(base).min(pages_len);
    let hi = full_end.saturating_sub(base).min(pages_len);
    (lo, hi)
}

const DEFAULT_HOT_COOL_FRAMES: u32 = 4;
const MAX_QUIET_COOL_BACKOFF: u8 = 2;
const PARALLEL_CARD_PAGES: usize = 16;
const PARALLEL_UNWATCHED_BYTES: usize = 1 << 20;
const MAX_CARD_SCAN_CHUNKS: usize = 6;

fn hot_cool_frames() -> u32 {
    static FRAMES: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("OATHKEEP_HOT_COOL_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_HOT_COOL_FRAMES)
    })
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct HotPageQuiet {
    /// `frame + 1` of the last frame on which this 64 KiB page had changed bytes (`0` = untracked).
    last_changed: u64,
    /// Number of distinct frames after `last_changed` with 0 changed bytes on a full-page hot check.
    quiet_frames: u32,
    /// `frame + 1` of the most recent quiet frame counted.
    last_quiet_frame: u64,
    /// Number of times this page was previously cooled; doubles quiet threshold per cool
    /// (up to `1 << MAX_QUIET_COOL_BACKOFF`) so active ring-buffer pages stop thrashing guards.
    cool_count: u8,
}

impl HotPageQuiet {
    #[inline]
    fn reset_quiet(&mut self) {
        self.last_changed = 0;
        self.quiet_frames = 0;
        self.last_quiet_frame = 0;
    }

    #[inline]
    fn mark_changed(&mut self, frame: u64) {
        self.last_changed = frame.saturating_add(1);
        self.quiet_frames = 0;
        self.last_quiet_frame = 0;
    }

    /// Updates quiet-page state for one 64 KiB page on `frame`.
    /// Returns `true` when the page has been unchanged across `cool_after << cool_count`
    /// distinct frames and should be cooled (removed from `watch` hot set).
    #[inline]
    fn record(
        &mut self,
        frame: u64,
        changed: bool,
        is_hot: bool,
        full_page: bool,
        cool_after: u32,
    ) -> bool {
        let f1 = frame.saturating_add(1);
        if changed {
            self.last_changed = f1;
            self.quiet_frames = 0;
            self.last_quiet_frame = 0;
            return false;
        }
        if !is_hot || !full_page || cool_after == 0 {
            return false;
        }
        if self.last_changed == 0 {
            self.last_changed = f1;
            self.quiet_frames = 0;
            self.last_quiet_frame = 0;
            return false;
        }
        if f1 > self.last_changed && f1 != self.last_quiet_frame {
            self.last_quiet_frame = f1;
            self.quiet_frames = self.quiet_frames.saturating_add(1);
        }
        let needed = cool_after.saturating_mul(1u32 << self.cool_count.min(MAX_QUIET_COOL_BACKOFF));
        if self.quiet_frames >= needed
            && f1.saturating_sub(self.last_changed) >= u64::from(needed)
        {
            self.reset_quiet();
            self.cool_count = self.cool_count.saturating_add(1).min(MAX_QUIET_COOL_BACKOFF);
            return true;
        }
        false
    }
}

#[derive(Default)]
struct HotHashes {
    /// Contiguous per-4 KiB sub-block hashes (`0` = cleared/unhashed, `h | 1` = hashed).
    slots: Vec<u128>,
    /// Per-64 KiB page quiet tracking for adaptive hot-page cooling.
    quiet: Vec<HotPageQuiet>,
}

pub(crate) struct CardMasked {
    pub(crate) offset: usize,
    pub(crate) data: Vec<u8>,
    pub(crate) mask: Arc<Vec<u8>>,
}

struct Card {
    before: Mutex<Option<ID3D12Resource>>,
    old: Mutex<BTreeMap<usize, Vec<u8>>>,
    has_old: AtomicBool,
    dirty: Mutex<Vec<(usize, usize)>>,
    sent: Mutex<Vec<(usize, usize, u64)>>,
    masked: Mutex<Vec<CardMasked>>,
    sent_masked: Mutex<Vec<(usize, Arc<Vec<u8>>, u64)>>,
    /// Contiguous per-4 KiB sub-block hashes (`0` = cleared/unhashed, `h | 1` = hashed)
    /// plus per-64 KiB page quiet tracking for adaptive hot-page cooling.
    /// Lazily allocated on first hot/rewrite/unwatched check.
    hot_hashes: Mutex<HotHashes>,
    has_hot_hashes: AtomicBool,
    size: u64,
}

impl Card {
    fn clear_hot_hashes(&self, from: usize, to: usize) {
        if from >= to || !self.has_hot_hashes.load(Ordering::Acquire) {
            return;
        }
        let mut hashes = self.hot_hashes.lock().unwrap_or_else(|e| e.into_inner());
        if hashes.slots.is_empty() {
            return;
        }
        let first = (from / HOT_HASH_BLOCK).min(hashes.slots.len());
        let last = to.div_ceil(HOT_HASH_BLOCK).min(hashes.slots.len());
        if first < last {
            hashes.slots[first..last].fill(0);
            if !hashes.quiet.is_empty() {
                let q_first = (from / PAGE).min(hashes.quiet.len());
                let q_last = to
                    .div_ceil(PAGE)
                    .saturating_add(1)
                    .min(hashes.quiet.len());
                if q_first < q_last {
                    for q in &mut hashes.quiet[q_first..q_last] {
                        q.reset_quiet();
                    }
                }
            }
            if first == 0 && last == hashes.slots.len() {
                self.has_hot_hashes.store(false, Ordering::Release);
            }
        }
    }
}

/// Compares `bytes` (representing buffer-relative `off_from..off_from + bytes.len()`) against
/// a slice `hashes` starting at 4 KiB slot index `slot_base`, updating changed slots and invoking
/// `on_changed(off_a, off_b)` for each changed sub-range. Returns `true` if any slot hash was stored.
fn diff_hot_card_blocks_at(
    hashes: &mut [u128],
    slot_base: usize,
    buf_len: usize,
    off_from: usize,
    bytes: &[u8],
    mut on_changed: impl FnMut(usize, usize),
) -> bool {
    let off_to = off_from + bytes.len();
    if off_from >= off_to {
        return false;
    }
    let first_slot = off_from / HOT_HASH_BLOCK;
    let last_slot = off_to.div_ceil(HOT_HASH_BLOCK).min(slot_base + hashes.len());
    if first_slot < slot_base || first_slot >= last_slot {
        return false;
    }
    let mut stored_any = false;
    for slot in first_slot..last_slot {
        let hash_entry = &mut hashes[slot - slot_base];
        let slot_lo = slot * HOT_HASH_BLOCK;
        let slot_hi = (slot_lo + HOT_HASH_BLOCK).min(buf_len);
        let cov_lo = off_from.max(slot_lo);
        let cov_hi = off_to.min(slot_hi);
        if cov_lo >= cov_hi {
            continue;
        }
        if cov_lo == slot_lo && cov_hi == slot_hi {
            let h = hot_page_hash(&bytes[cov_lo - off_from..cov_hi - off_from]) | 1;
            if *hash_entry == h {
                continue;
            }
            *hash_entry = h;
            stored_any = true;
            on_changed(cov_lo, cov_hi);
        } else {
            // Partial sub-block update: clear the full-block hash so future full-block
            // checks cannot mistakenly match stale bytes, and upload the partial slice.
            *hash_entry = 0;
            on_changed(cov_lo, cov_hi);
        }
    }
    stored_any
}

#[cfg(test)]
fn diff_hot_card_blocks(
    hashes: &mut [u128],
    buf_len: usize,
    off_from: usize,
    bytes: &[u8],
    on_changed: impl FnMut(usize, usize),
) -> bool {
    diff_hot_card_blocks_at(hashes, 0, buf_len, off_from, bytes, on_changed)
}

const HASH_K0: u64 = 0x9e37_79b9_7f4a_7c15;
const HASH_K1: u64 = 0xbf58_476d_1ce4_e5b9;
const HASH_K2: u64 = 0x94d0_49bb_1331_11eb;
const HASH_K3: u64 = 0xd6e8_feb8_6659_fd93;
const HASH_M_LO: [u64; 4] = [0x9e37_79b1, 0x85eb_ca77, 0xc2b2_ae3d, 0xa7d4_eb2f];
const HASH_M_HI: [u64; 4] = [0x94d0_49bb, 0xbf58_476d, 0xd6e8_feb9, 0xb54a_d69b];

#[inline]
fn hot_page_hash_init(len: usize) -> [u64; 16] {
    let l = len as u64;
    let s0 = l.wrapping_mul(HASH_K0) ^ HASH_K1;
    let s1 = l.wrapping_mul(HASH_K2) ^ HASH_K3;
    [
        s0 ^ HASH_K0,
        s1 ^ HASH_K1,
        s0 ^ HASH_K2,
        s1 ^ HASH_K3,
        s0.rotate_left(7) ^ HASH_K1,
        s1.rotate_left(11) ^ HASH_K2,
        s0.rotate_left(13) ^ HASH_K3,
        s1.rotate_left(17) ^ HASH_K0,
        s0.rotate_left(19) ^ HASH_K2,
        s1.rotate_left(23) ^ HASH_K3,
        s0.rotate_left(29) ^ HASH_K0,
        s1.rotate_left(31) ^ HASH_K1,
        s0.rotate_left(37) ^ HASH_K3,
        s1.rotate_left(41) ^ HASH_K0,
        s0.rotate_left(43) ^ HASH_K1,
        s1.rotate_left(47) ^ HASH_K2,
    ]
}

#[inline]
fn hot_page_hash_finish(mut acc: [u64; 16], rem: &[u8]) -> u128 {
    let mut words = rem.chunks_exact(8);
    for (i, chunk) in (&mut words).enumerate() {
        let w = u64::from_le_bytes(chunk.try_into().unwrap());
        let lane = i & 15;
        let t = acc[lane] ^ w;
        let p_lo = (t & 0xffff_ffff).wrapping_mul(HASH_M_LO[lane & 3]);
        let p_hi = (t >> 32).wrapping_mul(HASH_M_HI[lane & 3]);
        acc[lane] = p_lo.wrapping_add(p_hi.rotate_left(32));
    }
    let mut tail_a = acc[0];
    let mut tail_b = acc[1];
    for &byte in words.remainder() {
        tail_a = (tail_a ^ u64::from(byte))
            .wrapping_mul(HASH_K0)
            .rotate_left(11);
        tail_b = tail_b.wrapping_add(tail_a).wrapping_mul(HASH_K1);
    }
    acc[0] = tail_a;
    acc[1] = tail_b;
    let mut lo = HASH_K0;
    let mut hi = HASH_K1;
    for (i, &v) in acc.iter().enumerate() {
        let mul = HASH_K2.wrapping_add((i as u64).wrapping_mul(HASH_K0)) | 1;
        let m = v.wrapping_mul(mul);
        lo = (lo ^ m).wrapping_mul(HASH_K1).rotate_left(27);
        hi = (hi ^ m.rotate_left(19)).wrapping_mul(HASH_K3).rotate_left(31);
    }
    let mix = |mut x: u64| {
        x ^= x >> 30;
        x = x.wrapping_mul(HASH_K1);
        x ^= x >> 27;
        x = x.wrapping_mul(HASH_K2);
        x ^ (x >> 31)
    };
    let lo = mix(lo ^ hi.rotate_left(17));
    let hi = mix(hi ^ lo.rotate_left(23) ^ HASH_K0);
    (u128::from(hi) << 64) | u128::from(lo)
}

fn hot_page_hash_scalar(bytes: &[u8]) -> u128 {
    let mut acc = hot_page_hash_init(bytes.len());
    let mut chunks = bytes.chunks_exact(128);
    for chunk in &mut chunks {
        for lane in 0..16 {
            let off = lane * 8;
            let w = u64::from_le_bytes(chunk[off..off + 8].try_into().unwrap());
            let t = acc[lane] ^ w;
            let p_lo = (t & 0xffff_ffff).wrapping_mul(HASH_M_LO[lane & 3]);
            let p_hi = (t >> 32).wrapping_mul(HASH_M_HI[lane & 3]);
            acc[lane] = p_lo.wrapping_add(p_hi.rotate_left(32));
        }
    }
    hot_page_hash_finish(acc, chunks.remainder())
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hot_page_hash_avx2(bytes: &[u8]) -> u128 {
    use std::arch::x86_64::*;
    let mut acc = hot_page_hash_init(bytes.len());
    // SAFETY: `acc` holds 16 u64s and `HASH_M_LO`/`HASH_M_HI` hold 4 u64s each; unaligned loads are valid.
    let mut a0 = unsafe { _mm256_loadu_si256(acc.as_ptr().cast()) };
    let mut a1 = unsafe { _mm256_loadu_si256(acc.as_ptr().add(4).cast()) };
    let mut a2 = unsafe { _mm256_loadu_si256(acc.as_ptr().add(8).cast()) };
    let mut a3 = unsafe { _mm256_loadu_si256(acc.as_ptr().add(12).cast()) };
    let k_lo = unsafe { _mm256_loadu_si256(HASH_M_LO.as_ptr().cast()) };
    let k_hi = unsafe { _mm256_loadu_si256(HASH_M_HI.as_ptr().cast()) };

    macro_rules! step {
        ($reg:ident, $ptr:expr) => {{
            // SAFETY: `$ptr` points to 32 readable bytes inside the 128-byte chunk.
            let v = unsafe { _mm256_loadu_si256(($ptr).cast()) };
            let t = _mm256_xor_si256($reg, v);
            let t_hi = _mm256_srli_epi64::<32>(t);
            let p_lo = _mm256_mul_epu32(t, k_lo);
            let p_hi = _mm256_mul_epu32(t_hi, k_hi);
            let p_hi_rot = _mm256_shuffle_epi32::<0xB1>(p_hi);
            $reg = _mm256_add_epi64(p_lo, p_hi_rot);
        }};
    }

    let mut chunks = bytes.chunks_exact(128);
    for chunk in &mut chunks {
        let ptr = chunk.as_ptr();
        step!(a0, ptr);
        step!(a1, ptr.add(32));
        step!(a2, ptr.add(64));
        step!(a3, ptr.add(96));
    }
    // SAFETY: `acc` holds 16 u64s; unaligned stores are valid.
    unsafe {
        _mm256_storeu_si256(acc.as_mut_ptr().cast(), a0);
        _mm256_storeu_si256(acc.as_mut_ptr().add(4).cast(), a1);
        _mm256_storeu_si256(acc.as_mut_ptr().add(8).cast(), a2);
        _mm256_storeu_si256(acc.as_mut_ptr().add(12).cast(), a3);
    }
    hot_page_hash_finish(acc, chunks.remainder())
}

fn hot_page_hash(bytes: &[u8]) -> u128 {
    #[cfg(target_arch = "x86_64")]
    {
        static HAS_AVX2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if bytes.len() >= 128 && *HAS_AVX2.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
        {
            // SAFETY: runtime check confirmed AVX2 support on this CPU.
            return unsafe { hot_page_hash_avx2(bytes) };
        }
    }
    hot_page_hash_scalar(bytes)
}

thread_local! {
    static CARD_HOT_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[inline]
fn push_coalesced_span(out: &mut Vec<(usize, usize)>, a: usize, b: usize) {
    if a >= b {
        return;
    }
    match out.last_mut() {
        Some(last) if a == last.1 => last.1 = b,
        _ => out.push((a, b)),
    }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn read_into_zero_unmapped_skip_absent(src_addr: usize, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return true;
    }
    let end = src_addr.saturating_add(dst.len());
    let dst_ptr = dst.as_mut_ptr();
    let all_mapped = std::cell::Cell::new(true);
    crate::watch::split_absent_spans(
        src_addr,
        end,
        |a, b| {
            let off = a - src_addr;
            let n = b - a;
            // SAFETY: `split_absent_spans` invokes `on_present` and `on_absent` sequentially
            // on non-overlapping subranges of `[src_addr, end)`, so `dst_ptr.add(off)..+n`
            // is uniquely borrowed within `dst`.
            let slice = unsafe { std::slice::from_raw_parts_mut(dst_ptr.add(off), n) };
            if !crate::host::read_into_zero_unmapped(a as *const u8, slice) {
                all_mapped.set(false);
            }
        },
        |a, b| {
            let off = a - src_addr;
            let n = b - a;
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: disjoint subrange inside `dst`, invoked sequentially by `split_absent_spans`.
            unsafe { dst_ptr.add(off).write_bytes(0, n) };
            all_mapped.set(false);
        },
    );
    all_mapped.get()
}

#[inline]
fn copy_mapped_skip_absent(dst: *mut u8, src_addr: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = src_addr.saturating_add(len);
    crate::watch::split_absent_spans(
        src_addr,
        end,
        |a, b| {
            // SAFETY: `[a, b)` lies within `[src_addr, end)` so `dst.add(a - src_addr)`
            // covers `b - a` writable bytes inside `dst[..len]`.
            let at = unsafe { dst.add(a - src_addr) };
            crate::host::copy_mapped(at, a as *const u8, b - a, false);
        },
        |a, b| {
            let n = b - a;
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: inside the writable host mapping `dst[..len]`.
            unsafe { dst.add(a - src_addr).write_bytes(0, n) };
        },
    );
}

fn scan_card_pages_chunk(
    pages: &[(usize, bool)],
    start: usize,
    buf_len: usize,
    from: usize,
    to: usize,
    base: usize,
    frame: u64,
    cool_after: u32,
    slot_base: usize,
    slots: &mut [u128],
    quiet_base: usize,
    quiet: &mut [HotPageQuiet],
) -> (Vec<(usize, usize)>, Vec<usize>, bool) {
    let end = start + buf_len;
    let mut changed_spans: Vec<(usize, usize)> = Vec::new();
    let mut cool_blocks: Vec<usize> = Vec::new();
    let mut stored_any = false;
    CARD_HOT_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < PAGE {
            buf.resize(PAGE, 0);
        }
        for &(page, is_hot) in pages {
            let page_lo = (page * PAGE).max(start);
            let page_hi = ((page + 1) * PAGE).min(end);
            let p_from = page_lo.max(from);
            let p_to = page_hi.min(to);
            if p_from >= p_to {
                continue;
            }
            let off_from = p_from - start;
            let len = p_to - p_from;
            let full_page = p_from == page_lo && p_to == page_hi;
            let mut page_changed = false;
            if len > PAGE {
                let s_lo = (off_from / HOT_HASH_BLOCK).saturating_sub(slot_base);
                let s_hi = (off_from + len)
                    .div_ceil(HOT_HASH_BLOCK)
                    .saturating_sub(slot_base)
                    .min(slots.len());
                if s_lo < s_hi {
                    slots[s_lo..s_hi].fill(0);
                }
                push_coalesced_span(&mut changed_spans, p_from, p_to);
                page_changed = true;
            } else {
                #[cfg(feature = "diagnostics")]
                let t_rd = std::time::Instant::now();
                let ok = read_into_zero_unmapped_skip_absent(p_from, &mut buf[..len]);
                #[cfg(feature = "diagnostics")]
                {
                    let rd_us = t_rd.elapsed().as_micros() as u64;
                    crate::copystat::add(crate::copystat::C::CardScanPages, 1);
                    if is_hot {
                        crate::copystat::add(crate::copystat::C::CardScanHotPages, 1);
                    }
                    crate::copystat::add(crate::copystat::C::CardScanReadUs, rd_us);
                    if !ok {
                        crate::copystat::add(crate::copystat::C::CardScanSparseFaults, 1);
                    }
                }
                #[cfg(not(feature = "diagnostics"))]
                let _ = ok;
                #[cfg(feature = "diagnostics")]
                let t_hash = std::time::Instant::now();
                let stored = diff_hot_card_blocks_at(
                    slots,
                    slot_base,
                    buf_len,
                    off_from,
                    &buf[..len],
                    |cov_lo, cov_hi| {
                        page_changed = true;
                        #[cfg(feature = "diagnostics")]
                        crate::copystat::add(
                            crate::copystat::C::CardScanChangedBytes,
                            (cov_hi - cov_lo) as u64,
                        );
                        push_coalesced_span(&mut changed_spans, start + cov_lo, start + cov_hi);
                    },
                );
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(
                    crate::copystat::C::CardScanHashUs,
                    t_hash.elapsed().as_micros() as u64,
                );
                stored_any |= stored;
            }
            let idx = page - base;
            if idx >= quiet_base
                && let Some(q) = quiet.get_mut(idx - quiet_base)
                && q.record(frame, page_changed, is_hot, full_page, cool_after)
            {
                cool_blocks.push(page);
            }
        }
    });
    (changed_spans, cool_blocks, stored_any)
}

#[allow(clippy::too_many_arguments)]
fn scan_card_pages_into(
    card: &Card,
    start: usize,
    buf_len: usize,
    from: usize,
    to: usize,
    pages: &[(usize, bool)],
    frame: u64,
    cool_after: u32,
) -> (Vec<(usize, usize)>, Vec<usize>) {
    let end = start + buf_len;
    let (from, to) = (from.max(start), to.min(end));
    if from >= to || pages.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let base = start / PAGE;
    let pages_len = end.div_ceil(PAGE).saturating_sub(base);
    let owned_pages;
    let pages = if pages.windows(2).all(|w| w[0].0 < w[1].0)
        && pages.iter().all(|&(p, _)| p >= base && p - base < pages_len)
    {
        pages
    } else {
        let mut sorted: Vec<(usize, bool)> = pages
            .iter()
            .copied()
            .filter(|&(p, _)| p >= base && p - base < pages_len)
            .collect();
        sorted.sort_unstable_by_key(|&(p, _)| p);
        let mut deduped: Vec<(usize, bool)> = Vec::with_capacity(sorted.len());
        for (p, is_hot) in sorted {
            match deduped.last_mut() {
                Some(last) if last.0 == p => last.1 |= is_hot,
                _ => deduped.push((p, is_hot)),
            }
        }
        owned_pages = deduped;
        &owned_pages
    };
    if pages.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let slots_needed = buf_len.div_ceil(HOT_HASH_BLOCK);
    let mut guard = card.hot_hashes.lock().unwrap_or_else(|e| e.into_inner());
    let guard = &mut *guard;
    if guard.slots.len() < slots_needed {
        guard.slots.resize(slots_needed, 0);
    }
    if guard.quiet.len() < pages_len {
        guard.quiet.resize(pages_len, HotPageQuiet::default());
    }
    let use_parallel = pages.len() >= PARALLEL_CARD_PAGES && start.is_multiple_of(HOT_HASH_BLOCK);
    if !use_parallel {
        let (changed, cool, stored) = scan_card_pages_chunk(
            pages,
            start,
            buf_len,
            from,
            to,
            base,
            frame,
            cool_after,
            0,
            &mut guard.slots,
            0,
            &mut guard.quiet,
        );
        if stored {
            card.has_hot_hashes.store(true, Ordering::Release);
        }
        return (changed, cool);
    }
    let n_chunks = MAX_CARD_SCAN_CHUNKS.min(pages.len() / 2).max(2);
    let chunk_pages = pages.len().div_ceil(n_chunks);
    let page_chunks: Vec<&[(usize, bool)]> = pages.chunks(chunk_pages).collect();
    let actual_chunks = page_chunks.len();
    let mut results: Vec<(Vec<(usize, usize)>, Vec<usize>, bool)> =
        vec![(Vec::new(), Vec::new(), false); actual_chunks];
    let mut jobs: Vec<Job> = Vec::with_capacity(actual_chunks);
    let mut rem_slots: &mut [u128] = &mut guard.slots[..];
    let mut cur_slot_base = 0usize;
    let mut rem_quiet: &mut [HotPageQuiet] = &mut guard.quiet[..];
    let mut cur_quiet_base = 0usize;
    let results_base = results.as_mut_ptr() as usize;

    for (i, &pchunk) in page_chunks.iter().enumerate() {
        let (my_slots, my_slot_base, my_quiet, my_quiet_base) = if i + 1 < actual_chunks {
            let next_page = page_chunks[i + 1][0].0;
            let split_slot = (((next_page * PAGE).max(start) - start) / HOT_HASH_BLOCK)
                .clamp(cur_slot_base, cur_slot_base + rem_slots.len());
            let (s_left, s_right) = rem_slots.split_at_mut(split_slot - cur_slot_base);
            let sb = cur_slot_base;
            cur_slot_base = split_slot;
            rem_slots = s_right;

            let split_quiet = (next_page - base).clamp(cur_quiet_base, cur_quiet_base + rem_quiet.len());
            let (q_left, q_right) = rem_quiet.split_at_mut(split_quiet - cur_quiet_base);
            let qb = cur_quiet_base;
            cur_quiet_base = split_quiet;
            rem_quiet = q_right;
            (s_left, sb, q_left, qb)
        } else {
            let sb = cur_slot_base;
            let qb = cur_quiet_base;
            (
                std::mem::take(&mut rem_slots),
                sb,
                std::mem::take(&mut rem_quiet),
                qb,
            )
        };
        let pages_ptr = pchunk.as_ptr() as usize;
        let pages_count = pchunk.len();
        let slots_ptr = my_slots.as_mut_ptr() as usize;
        let slots_count = my_slots.len();
        let quiet_ptr = my_quiet.as_mut_ptr() as usize;
        let quiet_count = my_quiet.len();
        jobs.push(Box::new(move || {
            // SAFETY: `wide_leaf` joins all jobs before `pages`, `guard.slots`, `guard.quiet`,
            // and `results` are dropped; `split_at_mut` guarantees disjoint mutable slices per job.
            let (p_slice, s_slice, q_slice, out_slot) = unsafe {
                (
                    std::slice::from_raw_parts(pages_ptr as *const (usize, bool), pages_count),
                    std::slice::from_raw_parts_mut(slots_ptr as *mut u128, slots_count),
                    std::slice::from_raw_parts_mut(quiet_ptr as *mut HotPageQuiet, quiet_count),
                    &mut *(results_base as *mut (Vec<(usize, usize)>, Vec<usize>, bool)).add(i),
                )
            };
            *out_slot = scan_card_pages_chunk(
                p_slice,
                start,
                buf_len,
                from,
                to,
                base,
                frame,
                cool_after,
                my_slot_base,
                s_slice,
                my_quiet_base,
                q_slice,
            );
        }));
    }
    wide_leaf(jobs);

    let mut changed_spans: Vec<(usize, usize)> = Vec::new();
    let mut cool_blocks: Vec<usize> = Vec::new();
    let mut stored_any = false;
    for (spans, cool, stored) in results {
        for (a, b) in spans {
            push_coalesced_span(&mut changed_spans, a, b);
        }
        cool_blocks.extend(cool);
        stored_any |= stored;
    }
    if stored_any {
        card.has_hot_hashes.store(true, Ordering::Release);
    }
    (changed_spans, cool_blocks)
}

fn scan_unwatched_card_chunk(
    start: usize,
    buf_len: usize,
    off_from: usize,
    off_to: usize,
    slot_base: usize,
    slots: &mut [u128],
) -> (Vec<(usize, usize)>, bool) {
    let mut changed_spans: Vec<(usize, usize)> = Vec::new();
    let mut stored_any = false;
    CARD_HOT_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < PAGE {
            buf.resize(PAGE, 0);
        }
        let mut cur = off_from;
        while cur < off_to {
            let next_aligned = ((cur / HOT_HASH_BLOCK) + (PAGE / HOT_HASH_BLOCK)) * HOT_HASH_BLOCK;
            let step_end = next_aligned.min(off_to);
            let len = step_end - cur;
            let _ = crate::host::read_into_zero_unmapped((start + cur) as *const u8, &mut buf[..len]);
            let stored = diff_hot_card_blocks_at(
                slots,
                slot_base,
                buf_len,
                cur,
                &buf[..len],
                |cov_lo, cov_hi| {
                    push_coalesced_span(&mut changed_spans, start + cov_lo, start + cov_hi);
                },
            );
            stored_any |= stored;
            cur = step_end;
        }
    });
    (changed_spans, stored_any)
}

fn scan_unwatched_card_into(
    card: &Card,
    start: usize,
    buf_len: usize,
    from: usize,
    to: usize,
) -> Vec<(usize, usize)> {
    let (from, to) = (from.max(start), to.min(start + buf_len));
    if from >= to {
        return Vec::new();
    }
    let off_from = from - start;
    let off_to = to - start;
    let len = to - from;
    let slots_needed = buf_len.div_ceil(HOT_HASH_BLOCK);
    let mut guard = card.hot_hashes.lock().unwrap_or_else(|e| e.into_inner());
    let guard = &mut *guard;
    if guard.slots.len() < slots_needed {
        guard.slots.resize(slots_needed, 0);
    }
    if len < PARALLEL_UNWATCHED_BYTES {
        let (spans, stored) =
            scan_unwatched_card_chunk(start, buf_len, off_from, off_to, 0, &mut guard.slots);
        if stored {
            card.has_hot_hashes.store(true, Ordering::Release);
        }
        return spans;
    }
    let first_slot = off_from / HOT_HASH_BLOCK;
    let last_slot = off_to.div_ceil(HOT_HASH_BLOCK).min(guard.slots.len());
    let n_slots = last_slot.saturating_sub(first_slot);
    if n_slots < 2 {
        let (spans, stored) =
            scan_unwatched_card_chunk(start, buf_len, off_from, off_to, 0, &mut guard.slots);
        if stored {
            card.has_hot_hashes.store(true, Ordering::Release);
        }
        return spans;
    }
    let n_chunks = MAX_CARD_SCAN_CHUNKS.min(n_slots).max(2);
    let chunk_slots = n_slots.div_ceil(n_chunks);
    let actual_chunks = n_slots.div_ceil(chunk_slots);
    let mut results: Vec<(Vec<(usize, usize)>, bool)> = vec![(Vec::new(), false); actual_chunks];
    let mut jobs: Vec<Job> = Vec::with_capacity(actual_chunks);
    let mut rem_slots = &mut guard.slots[first_slot..last_slot];
    let mut cur_slot = first_slot;
    let results_base = results.as_mut_ptr() as usize;

    for i in 0..actual_chunks {
        let take = chunk_slots.min(rem_slots.len());
        let (my_slots, rest) = rem_slots.split_at_mut(take);
        let s_lo = cur_slot;
        let s_hi = cur_slot + take;
        cur_slot = s_hi;
        rem_slots = rest;
        let c_off_lo = off_from.max(s_lo * HOT_HASH_BLOCK);
        let c_off_hi = off_to.min(s_hi * HOT_HASH_BLOCK);
        let slots_ptr = my_slots.as_mut_ptr() as usize;
        let slots_count = my_slots.len();
        jobs.push(Box::new(move || {
            // SAFETY: `wide_leaf` joins all jobs before `guard.slots` and `results` are dropped;
            // `split_at_mut` guarantees disjoint mutable slices per job.
            let (s_slice, out_slot) = unsafe {
                (
                    std::slice::from_raw_parts_mut(slots_ptr as *mut u128, slots_count),
                    &mut *(results_base as *mut (Vec<(usize, usize)>, bool)).add(i),
                )
            };
            *out_slot = scan_unwatched_card_chunk(start, buf_len, c_off_lo, c_off_hi, s_lo, s_slice);
        }));
    }
    wide_leaf(jobs);

    let mut changed_spans: Vec<(usize, usize)> = Vec::new();
    let mut stored_any = false;
    for (spans, stored) in results {
        for (a, b) in spans {
            push_coalesced_span(&mut changed_spans, a, b);
        }
        stored_any |= stored;
    }
    if stored_any {
        card.has_hot_hashes.store(true, Ordering::Release);
    }
    changed_spans
}

pub(crate) fn merge_refresh_ranges(ranges: &mut Vec<(usize, usize)>, from: usize, to: usize) {
    const MERGE_GAP: usize = 256 << 10;
    const MAX_SPANS: usize = 32;
    if from >= to {
        return;
    }
    let mut lo = from;
    let mut hi = to;
    let mut first = ranges.len();
    let mut last = 0usize;
    let mut merged = false;
    for (i, &(a, b)) in ranges.iter().enumerate() {
        if hi.saturating_add(MERGE_GAP) >= a && lo <= b.saturating_add(MERGE_GAP) {
            lo = lo.min(a);
            hi = hi.max(b);
            first = first.min(i);
            last = i + 1;
            merged = true;
        }
    }
    if merged {
        ranges.splice(first..last, [(lo, hi)]);
        return;
    }
    let at = ranges.partition_point(|&(a, _)| a < lo);
    ranges.insert(at, (lo, hi));
    if ranges.len() > MAX_SPANS {
        let mut best_i = 0usize;
        let mut best_gap = usize::MAX;
        for i in 0..ranges.len() - 1 {
            let gap = ranges[i + 1].0.saturating_sub(ranges[i].1);
            if gap < best_gap {
                best_gap = gap;
                best_i = i;
            }
        }
        ranges[best_i].1 = ranges[best_i + 1].1;
        ranges.remove(best_i + 1);
    }
}

pub(crate) fn cards() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CARD_BUFFERS").is_none_or(|v| v != "0"))
}

fn card_limits() -> (u64, u64) {
    static LIMITS: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *LIMITS.get_or_init(|| {
        let mb = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default)
                << 20
        };
        let min_bytes = std::env::var("OATHKEEP_CARD_BUFFERS_MIN_KB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|kb| (kb << 10).max(64 << 10))
            .or_else(|| {
                std::env::var("OATHKEEP_CARD_BUFFERS_MIN_MB")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(|m| (m << 20).max(64 << 10))
            })
            .unwrap_or(512 << 10);
        (min_bytes, mb("OATHKEEP_CARD_BUFFERS_MB", 1024))
    })
}

static CARD_BYTES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

fn card_room_kept_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CARD_ROOM_FOR_BUFFERS").is_some_and(|v| v == "1"))
}

pub(crate) fn card_room_kept() -> u64 {
    if !card_room_kept_on() || !cards() {
        return 0;
    }
    card_limits()
        .1
        .saturating_sub(CARD_BYTES[0].load(Ordering::Relaxed))
}

static CARDED: Mutex<Vec<std::sync::Weak<Shadow>>> = Mutex::new(Vec::new());

pub(crate) static CARD_STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(crate) fn card_stats() -> [u64; 10] {
    let mut out = [0u64; 10];
    for (i, stat) in CARD_STATS.iter().enumerate() {
        out[i] = stat.swap(0, Ordering::Relaxed);
    }
    out[7] = CARDED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|w| w.strong_count() > 0)
        .count() as u64;
    out[8] = CARD_BYTES[0].load(Ordering::Relaxed) + WINDOW_CARD_BYTES.load(Ordering::Relaxed);
    out[9] = CARD_BYTES[1].load(Ordering::Relaxed);
    out
}

pub(crate) fn card_hash_bytes(size: u64) -> u64 {
    (size / PIECE as u64 + 2) * 16
}

fn card_for(size: u64) -> Option<ID3D12Resource> {
    if !cards() {
        return None;
    }
    let (least, most) = card_limits();
    if size < least || size >= 1 << 30 {
        return None;
    }
    let host = host()?;
    if !host.card_ready() {
        return None;
    }
    let headroom = crate::resource::commit_headroom();
    if headroom > 0
        && let Some(avail) = crate::host::available_commit()
        && avail < headroom.saturating_add(1024 << 20)
    {
        return None;
    }
    let tenths = if card_room_kept_on() { 9 } else { 8 };
    if let Some([budget, used, ..]) = host.video_memory()
        && used + size > budget / 10 * tenths
    {
        return None;
    }
    if CARD_BYTES[0].fetch_add(size, Ordering::Relaxed) + size > most {
        CARD_BYTES[0].fetch_sub(size, Ordering::Relaxed);
        return None;
    }
    let buffer = host.uav_buffer(size);
    if buffer.is_none() {
        CARD_BYTES[0].fetch_sub(size, Ordering::Relaxed);
    }
    buffer
}

fn window_card_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_CARD_WINDOWS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
            << 20
    })
}

static WINDOW_CARD_BYTES: AtomicU64 = AtomicU64::new(0);

fn window_card_for(size: u64) -> Option<ID3D12Resource> {
    let most = window_card_most();
    if most == 0 || !cards() {
        return None;
    }
    let host = host()?;
    if !host.card_ready() {
        return None;
    }
    let headroom = crate::resource::commit_headroom();
    if headroom > 0
        && let Some(avail) = crate::host::available_commit()
        && avail < headroom.saturating_add(1024 << 20)
    {
        return None;
    }
    if let Some([budget, used, ..]) = host.video_memory()
        && used + size > budget / 10 * 8
    {
        return None;
    }
    if WINDOW_CARD_BYTES.fetch_add(size, Ordering::Relaxed) + size > most {
        WINDOW_CARD_BYTES.fetch_sub(size, Ordering::Relaxed);
        return None;
    }
    let buffer = host.uav_buffer(size);
    if buffer.is_none() {
        WINDOW_CARD_BYTES.fetch_sub(size, Ordering::Relaxed);
    }
    buffer
}

pub(crate) fn card_dirty(
    sent_as: u64,
) -> (
    Vec<(Arc<Shadow>, Vec<(usize, usize)>)>,
    Vec<(Arc<Shadow>, Vec<CardMasked>)>,
) {
    let mut out = Vec::new();
    let mut masked_out = Vec::new();
    let mut carded = CARDED.lock().unwrap_or_else(|e| e.into_inner());
    let fetched = RELAY_FETCHED.load(Ordering::Acquire);
    carded.retain(|weak| {
        let Some(shadow) = weak.upgrade() else {
            return false;
        };
        let (ranges, masked) = shadow
            .card
            .as_ref()
            .map(|c| {
                let mut dirty = c.dirty.lock().unwrap_or_else(|e| e.into_inner());
                let ranges = std::mem::take(&mut *dirty);
                if relay() {
                    let mut sent = c.sent.lock().unwrap_or_else(|e| e.into_inner());
                    sent.retain(|s| s.2 > fetched);
                    sent.extend(ranges.iter().map(|&(a, b)| (a, b, sent_as)));
                }
                drop(dirty);
                let mut m_lock = c.masked.lock().unwrap_or_else(|e| e.into_inner());
                let masked = std::mem::take(&mut *m_lock);
                if relay() {
                    let mut sent_m = c.sent_masked.lock().unwrap_or_else(|e| e.into_inner());
                    sent_m.retain(|s| s.2 > fetched);
                    sent_m.extend(
                        masked
                            .iter()
                            .map(|m| (m.offset, m.mask.clone(), sent_as)),
                    );
                }
                (ranges, masked)
            })
            .unwrap_or_default();
        if !ranges.is_empty() {
            out.push((shadow.clone(), joined(ranges)));
        }
        if !masked.is_empty() {
            masked_out.push((shadow, masked));
        }
        true
    });
    (out, masked_out)
}

fn joined(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

pub(crate) fn joined_piece_runs(mut runs: Vec<CardSpan>) -> Vec<CardSpan> {
    runs.sort_unstable_by_key(|r| r.first);
    let mut out: Vec<CardSpan> = Vec::with_capacity(runs.len());
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CardSpan {
    pub(crate) from: u32,
    pub(crate) to: u32,
    pub(crate) first: u32,
    pub(crate) pieces: u32,
    pub(crate) skew: u32,
}

fn card_span_of(start: usize, from: usize, to: usize) -> CardSpan {
    let pieces = pieces_of(start, from, to);
    CardSpan {
        from: from as u32,
        to: to as u32,
        first: pieces.start as u32,
        pieces: pieces.len() as u32,
        skew: (start % PIECE) as u32,
    }
}

fn card_piece_of(start: usize, len: usize, span: &CardSpan, i: u32) -> (usize, usize) {
    let (lo, hi) = piece_at(start, len, (span.first + i) as usize);
    (lo.max(span.from as usize), hi.min(span.to as usize))
}

#[cfg(test)]
mod card_tests {
    use super::{CardSpan, PIECE, card_piece_of, card_span_of, joined, joined_piece_runs};

    #[test]
    fn piece_runs_join_without_losing_or_adding_pieces() {
        let run = |first: u32, pieces: u32| CardSpan {
            from: first * 65536,
            to: (first + pieces) * 65536,
            first,
            pieces,
            skew: 7,
        };
        let runs = vec![
            run(40, 1),
            run(3, 2),
            run(4, 1),
            run(5, 3),
            run(40, 1),
            run(9, 1),
            run(41, 2),
            run(100, 1),
        ];
        let mut wanted: Vec<u32> = runs
            .iter()
            .flat_map(|r| r.first..r.first + r.pieces)
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        let joined = joined_piece_runs(runs);
        let got: Vec<u32> = joined
            .iter()
            .flat_map(|r| r.first..r.first + r.pieces)
            .collect();
        assert_eq!(got, wanted);
        assert_eq!(
            joined
                .iter()
                .map(|r| (r.first, r.pieces))
                .collect::<Vec<_>>(),
            [(3, 5), (9, 1), (40, 3), (100, 1)]
        );
        assert!(joined.iter().all(|r| r.skew == 7));
        assert!(joined_piece_runs(Vec::new()).is_empty());
    }

    fn shader_piece(span: &CardSpan, i: u32) -> (u32, u32) {
        let piece = span.first + i;
        let lo = piece * 65536;
        let lo = if lo > span.skew { lo - span.skew } else { 0 };
        let hi = (piece + 1) * 65536 - span.skew;
        (lo.max(span.from), hi.min(span.to))
    }

    #[test]
    fn the_shader_and_the_merge_cut_the_same_pieces() {
        for start in [
            0usize,
            4096,
            PIECE - 16,
            PIECE,
            3 * PIECE + 0x1f00,
            0xd_b790_df00,
        ] {
            let len = 5 * PIECE + 4096;
            for (from, to) in [
                (0, len),
                (0, 1),
                (16, PIECE),
                (PIECE - 4, PIECE + 4),
                (2 * PIECE, len),
                (12345, 4 * PIECE + 7),
            ] {
                let span = card_span_of(start, from, to);
                let mut at = from;
                for i in 0..span.pieces {
                    let (lo, hi) = card_piece_of(start, len, &span, i);
                    assert_eq!(lo, at, "{start:#x} {from} {to} piece {i}");
                    assert!(hi > lo && hi - lo <= PIECE);
                    assert_eq!(
                        shader_piece(&span, i),
                        (lo as u32, hi as u32),
                        "{start:#x} {from} {to} piece {i}"
                    );
                    at = hi;
                }
                assert_eq!(at, to, "{start:#x} {from} {to}");
            }
        }
    }

    #[test]
    fn ranges_join_where_they_touch() {
        assert_eq!(
            joined(vec![(8, 12), (0, 4), (4, 6), (10, 20), (30, 31)]),
            [(0, 6), (8, 20), (30, 31)]
        );
        assert!(joined(Vec::new()).is_empty());
    }
}

pub(crate) fn relay() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("OATHKEEP_RELAY").map_or(!cfg!(test), |v| v == "1" || v == "2") && !keeps()
    })
}

static PREPARED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn preparing(number: u64) {
    PREPARED.store(number, Ordering::Relaxed);
}

static RELAY_FETCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn relay_fetched(number: u64) {
    RELAY_FETCHED.fetch_max(number, Ordering::AcqRel);
}

#[derive(Default)]
struct Flights(Vec<((usize, usize), u32, u64)>);

impl Flights {
    fn declare(&mut self, range: (usize, usize), number: u64) {
        match self.0.iter_mut().find(|f| f.0 == range) {
            Some(f) => {
                if f.2 != number {
                    f.1 += 1;
                    f.2 = number;
                }
            }
            None => self.0.push((range, 1, number)),
        }
    }

    fn done(&mut self, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let mut free = Vec::new();
        for range in ranges {
            match self.0.iter().position(|f| f.0 == *range) {
                Some(at) => {
                    self.0[at].1 = self.0[at].1.saturating_sub(1);
                    if self.0[at].1 == 0 {
                        self.0.swap_remove(at);
                        free.push(*range);
                    }
                }
                None => free.push(*range),
            }
        }
        free
    }
}

fn merge_piece(now: &[u8], held: &mut [u8], title: &mut [u8]) -> bool {
    if title == held {
        if now == held {
            return false;
        }
        held.copy_from_slice(now);
        title.copy_from_slice(now);
        return true;
    }
    if title == now {
        held.copy_from_slice(now);
        return false;
    }
    let mut differs = 0u8;
    for ((n, h), t) in now.iter().zip(held.iter_mut()).zip(title.iter_mut()) {
        let wrote = if *n != *h { 0xffu8 } else { 0 };
        let merged = (*t & !wrote) | (*n & wrote);
        differs |= merged ^ *t;
        *t = merged;
        *h = (*h & !wrote) | (*n & wrote);
    }
    differs != 0
}

fn outside(from: usize, to: usize, mut skip: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    skip.sort_unstable();
    let mut out = Vec::new();
    let mut at = from;
    for (a, b) in skip {
        if b <= at {
            continue;
        }
        if a >= to {
            break;
        }
        if a > at {
            out.push((at, a));
        }
        at = at.max(b);
    }
    if at < to {
        out.push((at, to));
    }
    out
}

fn word_runs(address: usize, read: &[u8], merged: &[u8], mut put: impl FnMut(usize, usize)) {
    let n = read.len().min(merged.len());
    let mut i = 0;
    while i < n {
        match first_difference(&read[i..n], &merged[i..n]) {
            Some(d) => i += d,
            None => break,
        }
        let first = i - ((address + i) & 3).min(i);
        let mut end = first;
        while end < n {
            let word = (((address + end) | 3) + 1 - address).min(n);
            if read[end..word] == merged[end..word] {
                break;
            }
            end = word;
        }
        put(first, end - first);
        i = end;
    }
}

fn merge_race_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_MERGE_RACE").is_some_and(|v| v == "1"))
}

struct MergeRace {
    frame: u64,
    now: [u64; 7],
    said: Vec<String>,
    total: [u64; 7],
    quiet: u64,
}

static MERGE_RACE: Mutex<MergeRace> = Mutex::new(MergeRace {
    frame: 0,
    now: [0; 7],
    said: Vec::new(),
    total: [0; 7],
    quiet: 0,
});

fn merge_race(
    read: Option<&[u8]>,
    copy: (u64, u64),
    address: usize,
    merged: &[u8],
    which: &'static str,
) {
    if !merge_race_on() {
        return;
    }
    let Some(read) = read else {
        return;
    };
    let n = read.len().min(merged.len());
    let (read, merged) = (&read[..n], &merged[..n]);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    word_runs(address, read, merged, |off, len| runs.push((off, len)));
    let whole = [(0, n)];
    let looked: &[(usize, usize)] = &whole;
    let mut again = vec![0u8; n];
    let (mut put_back, mut both, mut first) = (0u64, 0u64, None);
    if crate::host::read_into(address as *const u8, &mut again) {
        for &(off, len) in looked {
            for i in off..off + len {
                if again[i] == read[i] || again[i] == merged[i] {
                    continue;
                }
                if merged[i] == read[i] {
                    put_back += 1;
                    first.get_or_insert(i);
                } else {
                    both += 1;
                }
            }
        }
    }
    let frame = crate::frame::current();
    let mut race = MERGE_RACE.lock().unwrap_or_else(|e| e.into_inner());
    if race.frame != frame {
        race.quiet += 1;
        if race.now[4] > 0 || race.quiet >= 100 {
            tracing::info!(
                frame = race.frame,
                stretches = race.now[0],
                stretch_bytes = race.now[1],
                put_back = race.now[2],
                both_changed = race.now[3],
                stretches_hit = race.now[4],
                word_runs = race.now[5],
                word_run_bytes = race.now[6],
                stretches_since_start = race.total[0],
                stretch_bytes_since_start = race.total[1],
                put_back_since_start = race.total[2],
                both_changed_since_start = race.total[3],
                stretches_hit_since_start = race.total[4],
                word_runs_since_start = race.total[5],
                word_run_bytes_since_start = race.total[6],
                first = ?race.said,
                "merge race: title bytes that changed between a merge's read of title memory and its write-back of what shaders wrote, in this frame (OATHKEEP_MERGE_RACE=1)"
            );
            race.quiet = 0;
        }
        race.frame = frame;
        race.now = [0; 7];
        race.said.clear();
    }
    let hit = u64::from(put_back + both > 0);
    let run_bytes: u64 = runs.iter().map(|r| r.1 as u64).sum();
    let add = [
        1,
        n as u64,
        put_back,
        both,
        hit,
        runs.len() as u64,
        run_bytes,
    ];
    for (i, v) in add.iter().enumerate() {
        race.now[i] += v;
        race.total[i] += v;
    }
    if hit > 0 && race.said.len() < 8 {
        race.said.push(format!(
            "{which}: copy {:#x}+{:#x} stretch {address:#x}+{n:#x}: {put_back} put back (first at {:#x}), {both} both changed",
            copy.0,
            copy.1,
            address + first.unwrap_or(0)
        ));
    }
}

#[cfg(test)]
mod relay_tests {
    use super::{Flights, changed_runs, merge_piece, outside};

    #[test]
    fn a_range_stays_closed_until_every_flight_that_declared_it_is_done() {
        let (a, b) = ((0x1000, 0x2000), (0x1800, 0x3000));
        let mut flights = Flights::default();
        for _ in 0..3 {
            flights.declare(a, 7);
        }
        flights.declare(b, 7);
        flights.declare(a, 8);
        assert_eq!(flights.done(&[a, b]), vec![b]);
        flights.declare(a, 9);
        assert_eq!(flights.done(&[a]), vec![]);
        assert_eq!(flights.done(&[a]), vec![a]);
        assert!(flights.0.is_empty());
        flights.declare(a, 1);
        flights.declare(a, 2);
        assert_eq!(flights.done(&[a]), vec![]);
        assert_eq!(flights.done(&[a]), vec![a]);
        assert_eq!(flights.done(&[b]), vec![b]);
    }

    #[test]
    fn the_merge_stores_only_what_the_card_changed() {
        let held0: Vec<u8> = (0..200u32).map(|i| (i * 5 + 1) as u8).collect();
        let mut now = held0.clone();
        for at in [0usize, 3, 4, 5, 64, 199] {
            now[at] ^= 0x33;
        }
        let mut title = held0.clone();
        for at in [1usize, 5, 100] {
            title[at] = 0xee;
        }
        let title0 = title.clone();
        let mut held = held0.clone();
        assert!(merge_piece(&now, &mut held, &mut title));
        for at in 0..200 {
            if now[at] != held0[at] {
                assert_eq!(
                    (title[at], held[at]),
                    (now[at], now[at]),
                    "the card's byte at {at}"
                );
            } else {
                assert_eq!(
                    (title[at], held[at]),
                    (title0[at], held0[at]),
                    "untouched at {at}"
                );
            }
        }
        let (title1, held1) = (title.clone(), held.clone());
        assert!(!merge_piece(&now, &mut held, &mut title));
        assert_eq!((title, held), (title1, held1));
    }

    #[test]
    fn ranges_are_cut_out() {
        assert_eq!(outside(10, 50, vec![]), vec![(10, 50)]);
        assert_eq!(outside(10, 50, vec![(0, 10), (50, 60)]), vec![(10, 50)]);
        assert_eq!(
            outside(10, 50, vec![(30, 35), (0, 12), (34, 40), (48, 99)]),
            vec![(12, 30), (40, 48)]
        );
        assert_eq!(outside(10, 50, vec![(0, 99)]), vec![]);
    }

    #[test]
    fn two_flights_overlapping_ranges_and_a_cpu_write_in_between() {
        const N: usize = 64;
        let base: Vec<u8> = (0..N as u32).map(|i| i as u8).collect();
        let (mut copy, mut title) = (base.clone(), base.clone());
        let (a, b) = ((0usize, 40usize), (24usize, 64usize));
        let mut snap = base.clone();
        for at in (4..12).chain(30..34) {
            copy[at] = 0xa0;
        }
        for at in [8usize, 9, 20, 50] {
            title[at] = 0x77;
        }
        for (at, n) in changed_runs(&title, &snap) {
            copy[at..at + n].copy_from_slice(&title[at..at + n]);
            snap[at..at + n].copy_from_slice(&title[at..at + n]);
        }
        for at in 32..36 {
            copy[at] = 0xb0;
        }
        let seen = copy.clone();
        {
            let mut t = title[a.0..a.1].to_vec();
            assert!(merge_piece(&seen[a.0..a.1], &mut snap[a.0..a.1], &mut t));
            title[a.0..a.1].copy_from_slice(&t);
        }
        assert_eq!(copy, seen, "the merge stores nothing into the copy");
        for at in (4..8).chain(10..12).chain(30..32) {
            assert_eq!(title[at], 0xa0, "A's byte at {at}");
        }
        assert_eq!(
            (title[8], title[9], title[20], title[50]),
            (0x77, 0x77, 0x77, 0x77)
        );
        for at in 32..36 {
            assert_eq!(title[at], 0xb0, "B's byte so far at {at}");
        }
        for at in 36..44 {
            copy[at] = 0xb1;
        }
        title[40] = 0x55;
        for (at, n) in changed_runs(&title[b.0..b.1], &snap[b.0..b.1]) {
            let at = at + b.0;
            copy[at..at + n].copy_from_slice(&title[at..at + n]);
            snap[at..at + n].copy_from_slice(&title[at..at + n]);
        }
        let seen = copy.clone();
        {
            let mut t = title[b.0..b.1].to_vec();
            assert!(merge_piece(&seen[b.0..b.1], &mut snap[b.0..b.1], &mut t));
            title[b.0..b.1].copy_from_slice(&t);
        }
        for at in 32..36 {
            assert_eq!(title[at], 0xb0);
        }
        for at in (36..40).chain(41..44) {
            assert_eq!(title[at], 0xb1, "B's byte at {at}");
        }
        assert_eq!(title[40], 0x55, "the title wrote it last");
        assert_eq!(copy[a.0..b.1], title[a.0..b.1]);
        assert_eq!(snap[a.0..b.1], title[a.0..b.1]);
        let mut t = title.clone();
        assert!(!merge_piece(&copy, &mut snap, &mut t));
    }

    #[test]
    fn title_bytes_not_yet_on_the_card_are_not_taken_for_shader_writes() {
        const N: usize = 48;
        let base: Vec<u8> = (0..N as u32).map(|i| (i + 100) as u8).collect();
        let (mut card, mut cpu, mut title) = (base.clone(), base.clone(), base.clone());
        for at in 10..14 {
            card[at] = 0xa0;
        }
        title[12] = 0x77;
        title[30] = 0x78;
        let mut noted = Vec::new();
        for (at, n) in changed_runs(&title, &cpu) {
            cpu[at..at + n].copy_from_slice(&title[at..at + n]);
            noted.push((at, at + n, 5u64));
        }
        let fetched = card.clone();
        let fetched_after = 4u64;
        for &(a, b, _) in &noted {
            card[a..b].copy_from_slice(&cpu[a..b]);
        }
        let skip: Vec<(usize, usize)> = noted
            .iter()
            .filter(|s| s.2 > fetched_after)
            .map(|s| (s.0, s.1))
            .collect();
        let mut changed = false;
        for (a, b) in outside(0, N, skip) {
            let mut t = title[a..b].to_vec();
            changed |= merge_piece(&fetched[a..b], &mut cpu[a..b], &mut t);
            title[a..b].copy_from_slice(&t);
        }
        assert!(changed);
        for at in [10usize, 11, 13] {
            assert_eq!((title[at], cpu[at]), (0xa0, 0xa0));
        }
        assert_eq!((title[12], cpu[12], card[12]), (0x77, 0x77, 0x77));
        assert_eq!((title[30], cpu[30], card[30]), (0x78, 0x78, 0x78));
        assert_eq!(card, cpu);
        assert_eq!(card, title);
        let mut wrong_title = title.clone();
        let mut wrong_cpu = cpu.clone();
        merge_piece(&fetched, &mut wrong_cpu, &mut wrong_title);
        assert_eq!(wrong_title[30], base[30]);
    }
}

static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let most = n.saturating_mul(4).max(1 << 20);
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n && v.capacity() <= most)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

fn give_back(v: Vec<u8>) {
    const KEPT: usize = 64 << 20;
    if v.capacity() > KEPT {
        return;
    }
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
    pool.truncate(8);
    let mut total = 0usize;
    pool.retain(|v| {
        total = total.saturating_add(v.capacity());
        total <= KEPT
    });
}

const WATCHED_KERNEL: u8 = 1;
const NOT_WATCHED: u8 = 2;
const WATCHED_GUARDED: u8 = 3;

const DIFF_CHUNK: usize = 32;

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    let len = a.len().min(b.len());
    let (a_chunks, a_tail) = a[..len].as_chunks::<DIFF_CHUNK>();
    let (b_chunks, b_tail) = b[..len].as_chunks::<DIFF_CHUNK>();
    let (offset, a, b) = match a_chunks.iter().zip(b_chunks).position(|(x, y)| x != y) {
        Some(i) => (i * DIFF_CHUNK, &a_chunks[i][..], &b_chunks[i][..]),
        None => (len - a_tail.len(), a_tail, b_tail),
    };
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .map(|i| offset + i)
}

fn last_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    let len = a.len().min(b.len());
    let (a_chunks, a_tail) = a[..len].as_chunks::<DIFF_CHUNK>();
    let (b_chunks, b_tail) = b[..len].as_chunks::<DIFF_CHUNK>();
    if let Some(i) = a_tail.iter().zip(b_tail).rposition(|(x, y)| x != y) {
        return Some(len - a_tail.len() + i);
    }
    let chunk = a_chunks.iter().zip(b_chunks).rposition(|(x, y)| x != y)?;
    a_chunks[chunk]
        .iter()
        .zip(&b_chunks[chunk])
        .rposition(|(x, y)| x != y)
        .map(|i| chunk * DIFF_CHUNK + i)
}

#[cfg(test)]
mod difference_tests {
    use super::{first_difference, last_difference};

    #[test]
    fn differences_are_found_as_byte_by_byte() {
        let base: Vec<u8> = (0..98u32).map(|i| (i * 7 + 3) as u8).collect();
        for len in [0, 1, 31, 32, 33, 63, 64, 65, 97] {
            let a = &base[1..1 + len];
            let same = a.to_vec();
            assert_eq!(first_difference(&same, a), None);
            assert_eq!(last_difference(&same, a), None);
            for at in 0..len {
                for other in at..len {
                    let mut b = a.to_vec();
                    b[at] ^= 0x80;
                    b[other] ^= 0x01;
                    let first = a.iter().zip(&b).position(|(x, y)| x != y);
                    let last = a.iter().zip(&b).rposition(|(x, y)| x != y);
                    assert_eq!(first_difference(a, &b), first, "{len} {at} {other}");
                    assert_eq!(last_difference(a, &b), last, "{len} {at} {other}");
                }
            }
        }
    }

    #[test]
    fn differences_ignore_bytes_past_the_shorter_slice() {
        let a = [0; 65];
        let mut b = [0; 67];
        b[65] = 1;
        assert_eq!(first_difference(&a, &b), None);
        assert_eq!(last_difference(&b, &a), None);
        b[32] = 1;
        b[64] = 1;
        assert_eq!(first_difference(&b, &a), Some(32));
        assert_eq!(last_difference(&a, &b), Some(64));
    }
}

const PIECE: usize = 64 << 10;

unsafe fn copy_wide(src: *const u8, dst: *mut u8, len: usize) {
    const PER_THREAD: usize = 16 << 20;
    let threads = (len / PER_THREAD).clamp(1, 6);
    if threads == 1 {
        // SAFETY: the caller's.
        unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
        return;
    }
    let (src, dst, each) = (src as usize, dst as usize, len.div_ceil(threads));
    std::thread::scope(|s| {
        for i in 0..threads {
            let at = (i * each).min(len);
            let n = each.min(len - at);
            s.spawn(move || {
                // SAFETY: a part of the caller's ranges.
                unsafe {
                    std::ptr::copy_nonoverlapping((src + at) as *const u8, (dst + at) as *mut u8, n)
                };
            });
        }
    });
}

pub(crate) fn wide_back() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WIDE_BACK").is_none_or(|v| v != "0"))
}

type Job = Box<dyn FnOnce() + Send>;

struct Workers {
    jobs: Mutex<std::collections::VecDeque<Job>>,
    more: std::sync::Condvar,
}

fn workers() -> &'static Workers {
    static WORKERS: std::sync::OnceLock<Workers> = std::sync::OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let workers = WORKERS.get_or_init(|| Workers {
        jobs: Mutex::new(std::collections::VecDeque::new()),
        more: std::sync::Condvar::new(),
    });
    STARTED.call_once(|| {
        let n = std::thread::available_parallelism()
            .map_or(4, |n| n.get().saturating_sub(2))
            .clamp(2, 8);
        for i in 0..n {
            let _ = std::thread::Builder::new()
                .name(format!("oathkeep-wide-{i}"))
                .spawn(move || {
                    let workers = WORKERS.get().expect("made above");
                    loop {
                        let job = {
                            let mut jobs = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if let Some(job) = jobs.pop_front() {
                                    break job;
                                }
                                jobs = workers.more.wait(jobs).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        job();
                    }
                });
        }
    });
    workers
}

fn wide(jobs: Vec<Job>) {
    wide_leaf_on(workers(), jobs);
}

pub(crate) fn wide_run(jobs: Vec<Box<dyn FnOnce() + Send>>) {
    wide_leaf_on(workers(), jobs);
}

fn relay_workers() -> &'static Workers {
    static WORKERS: std::sync::OnceLock<Workers> = std::sync::OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let workers = WORKERS.get_or_init(|| Workers {
        jobs: Mutex::new(std::collections::VecDeque::new()),
        more: std::sync::Condvar::new(),
    });
    STARTED.call_once(|| {
        let count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4)
            .clamp(2, 4);
        for i in 0..count {
            let _ = std::thread::Builder::new()
                .name(format!("oathkeep-relay-w-{i}"))
                .spawn(move || {
                    let workers = relay_workers();
                    loop {
                        let job = {
                            let mut jobs = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if let Some(job) = jobs.pop_front() {
                                    break job;
                                }
                                jobs = workers.more.wait(jobs).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        job();
                    }
                });
        }
    });
    workers
}

/// Runs relay background diff/merge/writeback jobs on a dedicated non-stealing pool
/// isolated from `workers()`, so `Batch::flush`, `record_prelude`, and `take_snapshots`
/// never queue behind or steal relay background work.
pub(crate) fn wide_relay_run(jobs: Vec<Job>) {
    wide_leaf_on(relay_workers(), jobs);
}

fn heap_workers() -> &'static Workers {
    static WORKERS: std::sync::OnceLock<Workers> = std::sync::OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let workers = WORKERS.get_or_init(|| Workers {
        jobs: Mutex::new(std::collections::VecDeque::new()),
        more: std::sync::Condvar::new(),
    });
    STARTED.call_once(|| {
        let count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4)
            .clamp(2, 8);
        for i in 0..count {
            let _ = std::thread::Builder::new()
                .name(format!("oathkeep-heap-{i}"))
                .spawn(move || {
                    let workers = heap_workers();
                    loop {
                        let job = {
                            let mut jobs = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if let Some(job) = jobs.pop_front() {
                                    break job;
                                }
                                jobs = workers.more.wait(jobs).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        job();
                    }
                });
        }
    });
    workers
}

/// Runs descriptor-heap mirroring jobs on a dedicated pool isolated from `workers()`,
/// so `heap_buffers` never queues behind or steals heavy `batch_flush` / `relay` jobs.
pub(crate) fn wide_heap_run(jobs: Vec<Job>) {
    wide_leaf_on(heap_workers(), jobs);
}

fn stale_worker_count() -> usize {
    static COUNT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *COUNT.get_or_init(|| {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4)
            .clamp(2, 8)
    })
}

fn stale_workers() -> &'static Workers {
    static WORKERS: std::sync::OnceLock<Workers> = std::sync::OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let workers = WORKERS.get_or_init(|| Workers {
        jobs: Mutex::new(std::collections::VecDeque::new()),
        more: std::sync::Condvar::new(),
    });
    STARTED.call_once(|| {
        let count = stale_worker_count();
        for i in 0..count {
            let _ = std::thread::Builder::new()
                .name(format!("oathkeep-stale-{i}"))
                .spawn(move || {
                    let workers = stale_workers();
                    loop {
                        let job = {
                            let mut jobs = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if let Some(job) = jobs.pop_front() {
                                    break job;
                                }
                                jobs = workers.more.wait(jobs).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        job();
                    }
                });
        }
    });
    workers
}

/// Runs `stale_under_flights` checks on a dedicated non-stealing pool isolated from
/// `workers()`, preventing thread-pool starvation deadlocks when the relay thread holds
/// `Shadow::before`/`card.old` while waiting on `workers()`.
pub(crate) fn wide_stale_run(jobs: Vec<Job>) {
    wide_leaf_on(stale_workers(), jobs);
}

/// Runs pure leaf jobs in parallel on the shared `workers()` pool without ever popping
/// unrelated jobs on the calling thread, preventing re-entrant lock deadlocks when called
/// from inside an outer `wide_run` or while holding per-shadow locks.
fn wide_leaf(jobs: Vec<Job>) {
    wide_leaf_on(workers(), jobs);
}

fn wide_leaf_on(workers: &'static Workers, jobs: Vec<Job>) {
    if jobs.len() < 2 {
        for job in jobs {
            job();
        }
        return;
    }
    struct LeafBatch {
        queue: Mutex<std::collections::VecDeque<Job>>,
        rem: (Mutex<usize>, std::sync::Condvar),
    }
    let count = jobs.len();
    let batch = Arc::new(LeafBatch {
        queue: Mutex::new(std::collections::VecDeque::from(jobs)),
        rem: (Mutex::new(count), std::sync::Condvar::new()),
    });
    {
        let mut queue = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..count - 1 {
            let batch = batch.clone();
            queue.push_back(Box::new(move || {
                loop {
                    let job = batch
                        .queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pop_front();
                    let Some(job) = job else {
                        break;
                    };
                    job();
                    let mut rem = batch.rem.0.lock().unwrap_or_else(|e| e.into_inner());
                    *rem -= 1;
                    if *rem == 0 {
                        batch.rem.1.notify_all();
                    }
                }
            }));
        }
    }
    workers.more.notify_all();
    loop {
        let job = batch
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        let Some(job) = job else {
            break;
        };
        job();
        let mut rem = batch.rem.0.lock().unwrap_or_else(|e| e.into_inner());
        *rem -= 1;
        if *rem == 0 {
            batch.rem.1.notify_all();
        }
    }
    let mut rem = batch.rem.0.lock().unwrap_or_else(|e| e.into_inner());
    while *rem > 0 {
        rem = batch.rem.1.wait(rem).unwrap_or_else(|e| e.into_inner());
    }
}

const WIDE_JOB: usize = 1 << 20;

static SNAPS: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());

pub(crate) fn take_snapshots() {
    let snaps = std::mem::take(&mut *SNAPS.lock().unwrap_or_else(|e| e.into_inner()));
    if snaps.is_empty() {
        return;
    }
    wide(copy_jobs(snaps));
}

fn copy_jobs(copies: Vec<(usize, usize, usize)>) -> Vec<Job> {
    let mut jobs: Vec<Job> = Vec::new();
    let mut group: Vec<(usize, usize, usize)> = Vec::new();
    let mut bytes = 0;
    let flush = |group: &mut Vec<(usize, usize, usize)>, jobs: &mut Vec<Job>| {
        let group = std::mem::take(group);
        if !group.is_empty() {
            jobs.push(Box::new(move || {
                for (src, dst, len) in group {
                    // SAFETY: the caller's ranges: a host copy's mapping and
                    // its snapshot, which do not overlap.
                    unsafe { std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, len) };
                }
            }));
        }
    };
    for (src, dst, len) in copies {
        let mut at = 0;
        while at < len {
            let n = (WIDE_JOB - bytes).min(len - at);
            group.push((src + at, dst + at, n));
            at += n;
            bytes += n;
            if bytes >= WIDE_JOB {
                flush(&mut group, &mut jobs);
                bytes = 0;
            }
        }
    }
    flush(&mut group, &mut jobs);
    jobs
}

pub(crate) fn find_all(work: &[(Arc<Shadow>, usize, usize)]) -> Vec<Vec<(usize, usize)>> {
    let scans = work
        .iter()
        .map(|(shadow, address, end)| {
            let (from, to) = shadow.inside(*address, end.saturating_sub(*address));
            match shadow
                .before
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                Some(before) => (
                    shadow.data as usize,
                    before.as_ptr() as usize,
                    shadow.start as usize,
                    from,
                    to,
                ),
                None => (0, 0, 0, 0, 0),
            }
        })
        .collect();
    find_pieces(scans)
}

fn find_pieces(scans: Vec<(usize, usize, usize, usize, usize)>) -> Vec<Vec<(usize, usize)>> {
    type Found = Vec<(usize, usize, Vec<(usize, usize)>)>;
    type Part = (usize, usize, usize, usize, usize, usize, usize);
    let found: Arc<Mutex<Found>> = Arc::default();
    let mut jobs: Vec<Job> = Vec::new();
    let mut group: Vec<Part> = Vec::new();
    let mut bytes = 0;
    let flush = |group: &mut Vec<Part>, jobs: &mut Vec<Job>| {
        let group = std::mem::take(group);
        if group.is_empty() {
            return;
        }
        let found = found.clone();
        jobs.push(Box::new(move || {
            let mut mine: Found = Vec::new();
            for (data, before, start, from, to, which, place) in group {
                let mut pieces = Vec::new();
                let mut at = from;
                while at < to {
                    let n = (PIECE - (start + at) % PIECE).min(to - at);
                    // SAFETY: inside the copy's mapping and its snapshot
                    // (which holds the copy's bytes at the copy's offsets);
                    // nothing writes either while a submission's results
                    // are compared.
                    let (now, old) = unsafe {
                        (
                            std::slice::from_raw_parts((data + at) as *const u8, n),
                            std::slice::from_raw_parts((before + at) as *const u8, n),
                        )
                    };
                    if now != old {
                        pieces.push((at, n));
                    }
                    at += n;
                }
                if !pieces.is_empty() {
                    mine.push((which, place, pieces));
                }
            }
            if !mine.is_empty() {
                found.lock().unwrap_or_else(|e| e.into_inner()).extend(mine);
            }
        }));
    };
    let count = scans.len();
    for (which, (data, before, start, from, to)) in scans.into_iter().enumerate() {
        let (mut at, mut place) = (from, 0);
        while at < to {
            let room = (WIDE_JOB - bytes).max(PIECE);
            let mut next = (at + room).min(to);
            if next < to {
                next -= (start + next) % PIECE;
                if next <= at {
                    next = (at + PIECE - (start + at) % PIECE).min(to);
                }
            }
            group.push((data, before, start, at, next, which, place));
            bytes += next - at;
            place += 1;
            at = next;
            if bytes >= WIDE_JOB {
                flush(&mut group, &mut jobs);
                bytes = 0;
            }
        }
    }
    flush(&mut group, &mut jobs);
    wide_relay_run(jobs);
    let mut found = std::mem::take(&mut *found.lock().unwrap_or_else(|e| e.into_inner()));
    let mut out = vec![Vec::new(); count];
    if !found.is_empty() {
        found.sort_unstable_by_key(|f| (f.0, f.1));
        for (which, _, pieces) in found {
            out[which].extend(pieces);
        }
    }
    out
}

static BACK_CLASS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(crate) fn back_line(frames: f64) {
    let c: Vec<f64> = BACK_CLASS
        .iter()
        .map(|c| c.swap(0, Ordering::Relaxed) as f64 / frames)
        .collect();
    if c.iter().all(|&v| v == 0.0) {
        return;
    }
    let mb = |v: f64| v / (1 << 20) as f64;
    tracing::info!(
        "shader writes looked for on the CPU per frame: {:.1} MB compared in {:.0} ranges of windows outside buffers ({:.1} MB \
         snapshotted), {:.1} MB in {:.0} ranges of title buffers' copies ({:.1} MB snapshotted), of them {:.1} MB in copies \
         of 1 MiB or more (OATHKEEP_CARD_BUFFERS_MIN_MB=1 would have the card look there)",
        mb(c[2]),
        c[5],
        mb(c[0]),
        mb(c[3]),
        c[6],
        mb(c[1]),
        mb(c[4]),
    );
}

#[cfg(test)]
mod wide_tests {
    use super::{Job, PIECE, WIDE_JOB, copy_jobs, find_pieces, wide};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn all_jobs_run_before_wide_returns() {
        for n in [0usize, 1, 2, 7, 300] {
            let ran = Arc::new(AtomicUsize::new(0));
            let jobs: Vec<Job> = (0..n)
                .map(|i| {
                    let ran = ran.clone();
                    Box::new(move || {
                        ran.fetch_add(i + 1, Ordering::Relaxed);
                    }) as Job
                })
                .collect();
            wide(jobs);
            assert_eq!(ran.load(Ordering::Relaxed), n * (n + 1) / 2, "{n} jobs");
        }
    }

    #[test]
    fn copies_cut_into_jobs_copy_the_same_bytes() {
        let len = 3 * WIDE_JOB + 12345;
        let src: Vec<u8> = (0..len).map(|i| (i * 31 + i / 977) as u8).collect();
        let mut dst = vec![0u8; len];
        let (s, d) = (src.as_ptr() as usize, dst.as_mut_ptr() as usize);
        let cuts = [
            0,
            1,
            1,
            4096,
            70_000,
            WIDE_JOB + 3,
            2 * WIDE_JOB + 3,
            2 * WIDE_JOB + 9,
            len,
        ];
        let copies: Vec<(usize, usize, usize)> = cuts
            .windows(2)
            .map(|w| (s + w[0], d + w[0], w[1] - w[0]))
            .collect();
        let jobs = copy_jobs(copies);
        assert!(jobs.len() >= 3);
        wide(jobs);
        assert!(src == dst);
    }

    #[test]
    fn pieces_found_wide_are_those_found_one_by_one() {
        let len = 5 * WIDE_JOB / 2 + 777;
        let old: Vec<u8> = (0..len).map(|i| (i * 7 + i / 513) as u8).collect();
        let mut now = old.clone();
        for at in [
            0usize,
            1,
            PIECE - 1,
            PIECE,
            3 * PIECE + 5,
            WIDE_JOB - 1,
            WIDE_JOB,
            WIDE_JOB + PIECE / 2,
            2 * WIDE_JOB + 9,
            len - 1,
        ] {
            now[at] ^= 0x5a;
        }
        for byte in &mut now[9 * PIECE - 40..9 * PIECE + 40] {
            *byte ^= 0x01;
        }
        let (data, before) = (now.as_ptr() as usize, old.as_ptr() as usize);
        let mut scans = Vec::new();
        let mut wanted = Vec::new();
        for start in [0usize, 4096, PIECE - 16, 0xd_b790_df00] {
            for (from, to) in [
                (0, len),
                (0, 1),
                (1, 2),
                (PIECE - 1, PIECE + 1),
                (3 * PIECE, 3 * PIECE),
                (12345, len - 1),
                (WIDE_JOB - 3, 2 * WIDE_JOB + 10),
            ] {
                scans.push((data, before, start, from, to));
                let mut pieces = Vec::new();
                let mut at = from;
                while at < to {
                    let n = (PIECE - (start + at) % PIECE).min(to - at);
                    if now[at..at + n] != old[at..at + n] {
                        pieces.push((at, n));
                    }
                    at += n;
                }
                wanted.push(pieces);
            }
        }
        scans.push((0, 0, 0, 0, 0));
        wanted.push(Vec::new());
        assert_eq!(find_pieces(scans), wanted);
    }
}

fn back_piece(
    host: &mut [u8],
    old: &[u8],
    title: &mut Vec<u8>,
    mask: &mut Vec<u8>,
    runs: &mut Vec<(usize, usize)>,
    read: impl FnOnce(usize, &mut [u8]) -> bool,
) -> Option<(usize, usize)> {
    runs.clear();
    let first = first_difference(host, old)?;
    let last = last_difference(host, old).unwrap_or(first);
    let n = last + 1 - first;
    let (host, old) = (&mut host[first..=last], &old[first..=last]);
    title.clear();
    title.resize(n, 0);
    if !read(first, title) {
        return None;
    }
    mask.clear();
    mask.resize(n, 0);
    let mut differs = 0u8;
    for (((t, h), o), m) in title
        .iter_mut()
        .zip(host.iter_mut())
        .zip(old)
        .zip(mask.iter_mut())
    {
        let gpu = if (*h != *o) & (*t == *o) { 0xffu8 } else { 0 };
        let merged = (*t & !gpu) | (*h & gpu);
        *m = merged ^ *t;
        differs |= *m;
        *t = merged;
        *h = merged;
    }
    if differs == 0 {
        return None;
    }
    let mut i = 0;
    while i < n {
        while i < n && mask[i] == 0 {
            i += 1;
        }
        let start = i;
        while i < n && mask[i] != 0 {
            i += 1;
        }
        if i > start {
            runs.push((start, i - start));
        }
    }
    Some((first, n))
}

static MERGED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(crate) fn merged_stats() -> [u64; 3] {
    [0, 1, 2].map(|i| MERGED[i].swap(0, Ordering::Relaxed))
}

enum MergeDiff {
    Empty,
    Runs(Vec<(usize, usize)>),
    Masked {
        offset: usize,
        data: Vec<u8>,
        mask: Vec<u8>,
        changed_runs: usize,
        changed_bytes: usize,
        first_off: usize,
        last_end: usize,
    },
}

#[inline(always)]
fn load_u64_le(bytes: &[u8], offset: usize) -> u64 {
    // SAFETY: callers always guarantee `offset + 8 <= bytes.len()`.
    unsafe { (bytes.as_ptr().add(offset) as *const u64).read_unaligned() }.to_le()
}

/// Returns a bitmask with bit 7 set in every byte lane of `x` that is zero (SWAR).
#[inline(always)]
fn zero_byte_mask(x: u64) -> u64 {
    x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080
}

/// Advances `i` past equal bytes in `a[i..n]` vs `b[i..n]` using 64-bit XOR scanning.
#[inline(always)]
fn skip_equal_bytes(a: &[u8], b: &[u8], mut i: usize, n: usize) -> usize {
    while i + 32 <= n {
        let d0 = load_u64_le(a, i) ^ load_u64_le(b, i);
        let d1 = load_u64_le(a, i + 8) ^ load_u64_le(b, i + 8);
        let d2 = load_u64_le(a, i + 16) ^ load_u64_le(b, i + 16);
        let d3 = load_u64_le(a, i + 24) ^ load_u64_le(b, i + 24);
        if (d0 | d1 | d2 | d3) != 0 {
            if d0 != 0 {
                return i + (d0.trailing_zeros() >> 3) as usize;
            }
            if d1 != 0 {
                return i + 8 + (d1.trailing_zeros() >> 3) as usize;
            }
            if d2 != 0 {
                return i + 16 + (d2.trailing_zeros() >> 3) as usize;
            }
            return i + 24 + (d3.trailing_zeros() >> 3) as usize;
        }
        i += 32;
    }
    while i + 8 <= n {
        let d = load_u64_le(a, i) ^ load_u64_le(b, i);
        if d != 0 {
            return i + (d.trailing_zeros() >> 3) as usize;
        }
        i += 8;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Advances `i` past differing bytes in `a[i..n]` vs `b[i..n]` using 64-bit XOR + SWAR.
#[inline(always)]
fn skip_diff_bytes(a: &[u8], b: &[u8], mut i: usize, n: usize) -> usize {
    while i + 8 <= n {
        let d = load_u64_le(a, i) ^ load_u64_le(b, i);
        let eq = zero_byte_mask(d);
        if eq != 0 {
            return i + (eq.trailing_zeros() >> 3) as usize;
        }
        i += 8;
    }
    while i < n && a[i] != b[i] {
        i += 1;
    }
    i
}

/// Compares `title` against `held` (at buffer-relative offset `at` inside a card buffer of
/// 4-byte aligned `buf_len` bytes) and updates `held` in place for all changed bytes.
/// When at most `max_runs` contiguous byte runs differ, returns `MergeDiff::Runs`; otherwise
/// produces a 4-byte aligned `(data, mask)` payload for GPU `CARD_MASKED_MERGE` so dense
/// CPU writes under pending shader writes never fail with `runs_limit` or flood `CopyBufferRegion`.
fn diff_runs_or_masked(
    at: usize,
    buf_len: usize,
    title: &[u8],
    held: &mut [u8],
    max_runs: usize,
) -> MergeDiff {
    let n = title.len().min(held.len());
    let mut i = skip_equal_bytes(title, held, 0, n);
    if i >= n {
        return MergeDiff::Empty;
    }
    let mut runs: Vec<(usize, usize)> = Vec::new();
    while i < n {
        let first = i;
        i = skip_diff_bytes(title, held, first + 1, n);
        if runs.len() < max_runs {
            runs.push((first, i - first));
            i = skip_equal_bytes(title, held, i, n);
            continue;
        }
        // Dense diff (> max_runs): switch to a single 4-byte aligned GPU masked merge.
        let first_diff = runs.first().map_or(first, |r| r.0);
        let last_diff = last_difference(&title[first_diff..n], &held[first_diff..n])
            .map_or(i - 1, |off| first_diff + off);
        let abs_lo = (at + first_diff) & !3;
        let abs_hi = (at + last_diff + 1).next_multiple_of(4).min(buf_len);
        let payload_len = abs_hi.saturating_sub(abs_lo);
        let mut data = vec![0u8; payload_len];
        let mut mask = vec![0u8; payload_len];
        let mut changed_runs = 0usize;
        let mut changed_bytes = 0usize;
        let end_excl = last_diff + 1;
        let mut cur = skip_equal_bytes(title, held, first_diff, end_excl);
        while cur < end_excl {
            let r_start = cur;
            cur = skip_diff_bytes(title, held, r_start + 1, end_excl);
            let r_len = cur - r_start;
            held[r_start..cur].copy_from_slice(&title[r_start..cur]);
            let dst_off = (at + r_start) - abs_lo;
            data[dst_off..dst_off + r_len].copy_from_slice(&title[r_start..cur]);
            mask[dst_off..dst_off + r_len].fill(0xFF);
            changed_runs += 1;
            changed_bytes += r_len;
            cur = skip_equal_bytes(title, held, cur, end_excl);
        }
        return MergeDiff::Masked {
            offset: abs_lo,
            data,
            mask,
            changed_runs,
            changed_bytes,
            first_off: at + first_diff,
            last_end: at + last_diff + 1,
        };
    }
    if runs.is_empty() {
        return MergeDiff::Empty;
    }
    for &(r, m) in &runs {
        held[r..r + m].copy_from_slice(&title[r..r + m]);
    }
    MergeDiff::Runs(runs)
}

#[inline]
fn for_each_changed_run(now: &[u8], was: &[u8], mut f: impl FnMut(usize, usize)) {
    let n = now.len().min(was.len());
    let mut i = skip_equal_bytes(now, was, 0, n);
    while i < n {
        let first = i;
        i = skip_diff_bytes(now, was, first + 1, n);
        f(first, i - first);
        i = skip_equal_bytes(now, was, i, n);
    }
}

#[cfg(test)]
fn changed_runs(now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    for_each_changed_run(now, was, |r, m| runs.push((r, m)));
    runs
}

#[cfg(test)]
mod merge_tests {
    use super::{PIECE, back_piece, changed_runs};

    #[test]
    fn a_piece_put_back_reads_only_its_stretch() {
        let old: Vec<u8> = (0..PIECE).map(|i| (i * 7 + i / 251) as u8).collect();
        let mut title_memory = old.clone();
        title_memory[7] ^= 0x11;
        title_memory[40_000] ^= 0x22;
        let mut host = old.clone();
        for at in [5, 9] {
            host[at] ^= 0x33;
        }
        let gpu = host.clone();
        let mut scratch = Vec::new();
        let (mut mask, mut runs) = (Vec::new(), Vec::new());
        let mut asked = None;
        let read = |first: usize, into: &mut [u8]| {
            asked = Some((first, into.len()));
            into.copy_from_slice(&title_memory[first..first + into.len()]);
            true
        };
        assert_eq!(
            back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read),
            Some((5, 5))
        );
        assert_eq!(asked, Some((5, 5)));
        let mut wanted = title_memory[5..10].to_vec();
        wanted[0] = gpu[5];
        wanted[4] = gpu[9];
        assert_eq!(scratch, wanted);
        assert_eq!(host[5..10], wanted);
        assert_eq!(runs, [(0, 1), (4, 1)]);
        assert!(host[..5] == gpu[..5] && host[10..] == gpu[10..]);
        assert!(
            scratch.capacity() < PIECE / 2,
            "scratch of {} bytes for a stretch of 5",
            scratch.capacity()
        );

        let mut same = old.clone();
        assert_eq!(
            back_piece(
                &mut same,
                &old,
                &mut scratch,
                &mut mask,
                &mut runs,
                |_, _| unreachable!()
            ),
            None
        );
        let mut host = gpu.clone();
        assert_eq!(
            back_piece(
                &mut host,
                &old,
                &mut scratch,
                &mut mask,
                &mut runs,
                |_, _| false
            ),
            None
        );
        assert!(host == gpu);
        let mut title_memory = old.clone();
        title_memory[5] = gpu[5];
        title_memory[9] = gpu[9];
        let mut host = gpu.clone();
        let read = |first: usize, into: &mut [u8]| {
            into.copy_from_slice(&title_memory[first..first + into.len()]);
            true
        };
        assert_eq!(
            back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read),
            None
        );
        let mut host: Vec<u8> = old.iter().map(|b| !b).collect();
        assert_eq!(
            back_piece(
                &mut host,
                &old,
                &mut scratch,
                &mut mask,
                &mut runs,
                |_, into| into.len() == PIECE
            ),
            Some((0, PIECE))
        );
        assert_eq!(scratch.len(), PIECE);
    }

    #[test]
    fn a_byte_both_changed_keeps_the_titles_and_only_gpu_bytes_are_written() {
        let old = vec![1u8, 2, 3, 4, 5, 6];
        let mut host = old.clone();
        host[1] = 20;
        host[3] = 40;
        host[4] = 50;
        let mut title_memory = old.clone();
        title_memory[3] = 44;
        title_memory[5] = 66;
        let (mut scratch, mut mask, mut runs) = (Vec::new(), Vec::new(), Vec::new());
        let read = |first: usize, into: &mut [u8]| {
            into.copy_from_slice(&title_memory[first..first + into.len()]);
            true
        };
        assert_eq!(
            back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read),
            Some((1, 4))
        );
        assert_eq!(
            scratch,
            [20, 3, 44, 50],
            "the title's byte where both changed"
        );
        assert_eq!(host[1..5], [20, 3, 44, 50], "the copy takes the result");
        assert_eq!(runs, [(0, 1), (3, 1)], "the GPU's bytes only");
    }

    #[test]
    fn changed_runs_are_exactly_the_bytes_that_differ() {
        let was: Vec<u8> = (0..300u32).map(|i| (i * 13 + 5) as u8).collect();
        for changes in [
            vec![],
            vec![0],
            vec![299],
            vec![7, 8, 9],
            vec![7, 9],
            vec![
                15, 16, 17, 23, 24, 64, 65, 66, 67, 68, 69, 70, 71, 72, 298, 299,
            ],
        ] {
            let mut now = was.clone();
            for &at in &changes {
                now[at] ^= 0x41;
            }
            let runs = changed_runs(&now, &was);
            let mut put = was.clone();
            let mut held = Vec::new();
            for &(at, n) in &runs {
                assert!(n > 0);
                put[at..at + n].copy_from_slice(&now[at..at + n]);
                held.extend(at..at + n);
            }
            assert_eq!(put, now, "{changes:?}");
            assert_eq!(held, changes, "{changes:?}");
            assert!(
                runs.windows(2).all(|w| w[0].0 + w[0].1 < w[1].0),
                "{changes:?}"
            );
        }
        assert!(changed_runs(&[], &[]).is_empty());
    }
}

static LIVE: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Shadow {
    fn drop(&mut self) {
        if self.place.is_some() {
            crate::inplace::gone(self.window, self.len);
            return;
        }
        LIVE[0].fetch_sub(1, Ordering::Relaxed);
        LIVE[1].fetch_sub(self.len, Ordering::Relaxed);
        if self.kept.get_mut().unwrap_or_else(|e| e.into_inner()).1 {
            KEPT_BYTES.fetch_sub(self.len, Ordering::Relaxed);
        }
        let window = self.window;
        if let Some(card) = &mut self.card {
            if window {
                WINDOW_CARD_BYTES.fetch_sub(card.size, Ordering::Relaxed);
            } else {
                CARD_BYTES[0].fetch_sub(card.size, Ordering::Relaxed);
            }
            if card
                .before
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
            {
                CARD_BYTES[1].fetch_sub(card_hash_bytes(card.size), Ordering::Relaxed);
            }
        }
    }
}

fn keeps() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_KEEP_SNAPSHOTS").is_some_and(|v| v == "1"))
}

static KEPT_BYTES: AtomicU64 = AtomicU64::new(0);
fn kept_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_KEEP_SNAPSHOTS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(256)
            << 20
    })
}

fn pieces_of(start: usize, from: usize, to: usize) -> std::ops::Range<usize> {
    let first = start / PIECE;
    (start + from) / PIECE - first..(start + to - 1) / PIECE - first + 1
}

fn piece_at(start: usize, len: usize, piece: usize) -> (usize, usize) {
    let first = start / PIECE;
    let lo = ((first + piece) * PIECE).saturating_sub(start);
    let hi = ((first + piece + 1) * PIECE - start).min(len);
    (lo, hi)
}

#[cfg(test)]
mod piece_tests {
    use super::{PIECE, piece_at, pieces_of};

    #[test]
    fn pieces_cover_the_copy_as_the_compare_cuts_it() {
        for start in [
            0usize,
            1,
            4096,
            PIECE - 1,
            PIECE,
            PIECE + 77,
            5 * PIECE + 12345,
        ] {
            for len in [1usize, 100, PIECE - 1, PIECE, PIECE + 1, 3 * PIECE + 500] {
                let count = pieces_of(start, 0, len).end;
                let mut at = 0;
                for piece in 0..count {
                    let (lo, hi) = piece_at(start, len, piece);
                    assert_eq!(lo, at, "{start} {len} {piece}");
                    let n = (PIECE - (start + at) % PIECE).min(len - at);
                    assert_eq!(hi, at + n, "{start} {len} {piece}");
                    assert_eq!(pieces_of(start, lo, hi), piece..piece + 1);
                    assert_eq!(pieces_of(start, hi - 1, hi), piece..piece + 1);
                    at = hi;
                }
                assert_eq!(at, len, "{start} {len}");
                assert_eq!(pieces_of(start, 0, len), 0..count);
            }
        }
    }
}

#[cfg(test)]
fn stale_pages_in(
    base: usize,
    copied: &[u64],
    a: usize,
    e: usize,
    written: &[(usize, u64)],
) -> Vec<usize> {
    (a / PAGE..e.div_ceil(PAGE))
        .filter(|&page| page >= base)
        .filter(|&page| match copied.get(page - base) {
            None => false,
            Some(0) => true,
            Some(&at) => written
                .binary_search_by_key(&page, |&(p, _)| p)
                .is_ok_and(|i| written[i].1 > at),
        })
        .collect()
}

#[cfg(test)]
mod stale_tests {
    use super::*;

    #[test]
    fn pages_never_taken_are_stale_written_or_not() {
        let base = 0x100;
        let copied = [5, 0, 5, 5];
        let at = |page: usize| page * PAGE;
        let written = [(0x102, 6), (0x103, 4)];
        assert_eq!(
            stale_pages_in(base, &copied, at(0x100), at(0x104), &written),
            [0x101, 0x102]
        );
        assert_eq!(
            stale_pages_in(base, &copied, at(0x101) + 8, at(0x101) + 16, &[]),
            [0x101]
        );
        assert!(stale_pages_in(base, &copied, at(0x104), at(0x106), &[(0x104, 9)]).is_empty());
    }
}

static LET_GO: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

#[cfg(feature = "diagnostics")]
pub(crate) fn windows() -> [u64; 3] {
    let asked: u64 = {
        let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        loose
            .values()
            .map(|s| {
                let low = s.asked[0].load(Ordering::Relaxed);
                let high = s.asked[1].load(Ordering::Relaxed);
                high.saturating_sub(low)
            })
            .sum()
    };
    [
        LET_GO[0].load(Ordering::Relaxed),
        LET_GO[1].load(Ordering::Relaxed),
        asked,
    ]
}

#[cfg(feature = "diagnostics")]
pub(crate) fn window_use() -> [u64; 8] {
    let mut out = [0u64; 8];
    let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
    for s in loose.values() {
        let copied = s.copied.lock().unwrap_or_else(|e| e.into_inner());
        let pages = copied.copied_page_count();
        out[0] += (pages * PAGE as u64).min(s.len);
        if crate::host::references(&s.buffer) > 1 {
            out[1] += 1;
            out[2] += s.len;
        }
        if Arc::strong_count(s) > 1 {
            out[3] += 1;
            out[4] += s.len;
        }
        if s.unsized_asks.load(Ordering::Relaxed) > 0 {
            out[5] += 1;
            out[6] += s.len;
        }
    }
    out[7] = window();
    out
}

pub(crate) fn let_go_idle() {
    let_go_idle_with(false);
}

pub(crate) fn let_go_idle_urgent() {
    let_go_idle_with(true);
}

fn let_go_idle_with(urgent: bool) {
    let avail_phys = crate::host::available_physical_ram();
    let avail_commit = crate::host::available_commit();
    let (idle_ms, idle_frames, drain_pool) = if urgent {
        (0, 1, true)
    } else {
        (
            crate::host::ram_idle_ms(avail_phys).min(crate::host::commit_idle_ms(avail_commit)),
            crate::host::commit_idle_frames(avail_commit),
            avail_commit.is_some_and(|c| c < (3584u64 << 20)),
        )
    };
    if drain_pool {
        let drained = if urgent {
            POOL.try_lock().ok().map(|mut p| std::mem::take(&mut *p))
        } else {
            let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            Some(std::mem::take(&mut *pool))
        };
        drop(drained);
    }
    let gone_loose: Vec<Arc<Shadow>> = {
        let map = if urgent {
            LOOSE.try_write().ok()
        } else {
            Some(LOOSE.write().unwrap_or_else(|e| e.into_inner()))
        };
        if let Some(mut map) = map
            && !map.is_empty()
        {
            let idle: Vec<(u64, u64)> = map
                .iter()
                .filter(|(_, s)| {
                    Arc::strong_count(s) == 1
                        && !s.writes_pending()
                        && crate::host::long_ago(
                            s.used.load(Ordering::Relaxed),
                            idle_ms,
                            idle_frames,
                        )
                        && crate::host::references(&s.buffer) == 1
                })
                .map(|(key, _)| *key)
                .collect();
            idle.into_iter()
                .filter_map(|key| map.remove(&key))
                .collect()
        } else {
            Vec::new()
        }
    };
    let gone_buffers: Vec<Arc<Shadow>> = {
        let map = if urgent {
            BUFFERS.try_write().ok()
        } else {
            Some(buffers_write())
        };
        if let Some(mut map) = map {
            let mut out = Vec::new();
            for entry in map.values_mut() {
                let evict = entry.shadow.as_ref().is_some_and(|s| {
                    Arc::strong_count(s) == 1
                        && !s.writes_pending()
                        && crate::host::long_ago(
                            s.used.load(Ordering::Relaxed),
                            idle_ms,
                            idle_frames,
                        )
                        && crate::host::references(&s.buffer) == 1
                });
                if evict && let Some(s) = entry.shadow.take() {
                    out.push(s);
                }
            }
            out
        } else {
            Vec::new()
        }
    };
    if !gone_loose.is_empty() || !gone_buffers.is_empty() {
        for s in gone_loose.iter().chain(gone_buffers.iter()) {
            s.mark_dead();
        }
        SHADOW_GEN.fetch_add(1, Ordering::Release);
        let went = gone_loose.len() + gone_buffers.len();
        let bytes: u64 = gone_loose
            .iter()
            .chain(gone_buffers.iter())
            .map(|s| s.len)
            .sum();
        crate::host::trail(|| {
            format!(
                "sweep: {went} host copies of title memory let go ({} KiB; {} windows, {} buffers), held by no list and no slot",
                bytes >> 10,
                gone_loose.len(),
                gone_buffers.len()
            )
        });
    }
    for shadow in gone_loose.into_iter().chain(gone_buffers) {
        LET_GO[0].fetch_add(1, Ordering::Relaxed);
        LET_GO[1].fetch_add(shadow.len, Ordering::Relaxed);
        crate::host::retire(shadow);
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 6] {
    let (windows, window_bytes) = {
        let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        let mut seen = std::collections::HashSet::new();
        let bytes: u64 = loose
            .values()
            .filter(|s| seen.insert(Arc::as_ptr(s) as usize))
            .map(|s| s.len)
            .sum();
        (seen.len() as u64, bytes)
    };
    let (kept, kept_bytes) = {
        let pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        (
            pool.len() as u64,
            pool.iter().map(|v| v.capacity() as u64).sum::<u64>(),
        )
    };
    [
        LIVE[0].load(Ordering::Relaxed),
        LIVE[1].load(Ordering::Relaxed),
        windows,
        window_bytes,
        kept,
        kept_bytes,
    ]
}

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    pub(crate) fn span(&self) -> (u64, u64) {
        (self.start, self.len)
    }

    pub(crate) fn offset_in(&self, address: u64) -> Option<u64> {
        address.checked_sub(self.start).filter(|&at| at < self.len)
    }

    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    pub(crate) fn unwatched(&self) -> bool {
        self.watched.load(Ordering::Relaxed) == NOT_WATCHED
    }

    pub(crate) fn guarded(&self) -> bool {
        self.watched.load(Ordering::Relaxed) == WATCHED_GUARDED
    }

    pub(crate) fn ever_written(&self) -> bool {
        self.ever_written.load(Ordering::Relaxed)
    }

    pub(crate) fn is_gpu_active(&self, _list: &HostList, _address: u64, len: u64) -> bool {
        len > 0 && self.ever_written()
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    pub(crate) fn mark_dead(&self) {
        self.dead.store(true, Ordering::Release);
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start + self.place.as_ref().map_or(0, |place| place.1)
    }

    pub(crate) fn is_in_place(&self) -> bool {
        self.place.is_some()
    }

    pub(crate) fn host_width(&self) -> u64 {
        self.host_width
    }

    /// Whether `address..address+size` can be bound directly as a D3D12 CBV:
    /// 256-byte aligned GPU VA, the title's requested `size` fits in the shadow's
    /// logical span, and the 256-byte-rounded D3D12 CBV size fits in the host buffer.
    pub(crate) fn can_bind_cbv(&self, address: u64, size: u32) -> bool {
        let size_u64 = u64::from(size);
        let aligned = u64::from(size.next_multiple_of(256));
        self.gpu(address).is_multiple_of(256)
            && self.left(address) >= size_u64
            && self.host_width.saturating_sub(self.offset(address)) >= aligned
    }

    pub(crate) fn allow_uav(&self) -> bool {
        self.allow_uav
    }

    fn placed(placed: crate::inplace::Placed, start: u64, len: u64, window: bool) -> Shadow {
        crate::host::name(&placed.buffer, || {
            format!("title memory {start:#x}+{len:#x} in place")
        });
        let mut shadow = Shadow::new(placed.buffer, start as *mut u8, placed.gpu, start, len);
        LIVE[0].fetch_sub(1, Ordering::Relaxed);
        LIVE[1].fetch_sub(len, Ordering::Relaxed);
        shadow.copied.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        *shadow.watched.get_mut() = NOT_WATCHED;
        shadow.window = window;
        shadow.place = Some((placed.heap, placed.bias));
        shadow
    }

    #[inline]
    pub(crate) fn touch_used(&self, now: u64) {
        self.used.store(now, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn is_window(&self) -> bool {
        self.window
    }

    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        self.used.store(crate::host::moment(), Ordering::Relaxed);
        if self.place.is_some() {
            list.keep(&self.buffer);
            return;
        }
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Shadow {
        // SAFETY: `buffer` is a live D3D12 resource whose width and flags never change.
        let desc = unsafe { buffer.GetDesc() };
        let host_width = desc.Width;
        let allow_uav = desc.Flags.0
            & windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0
            != 0;
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        LIVE[0].fetch_add(1, Ordering::Relaxed);
        LIVE[1].fetch_add(len, Ordering::Relaxed);
        Shadow {
            buffer,
            host_width,
            allow_uav,
            data,
            gpu,
            start,
            len,
            copied: Mutex::new(CopiedState::new(pages as usize)),
            uncopied: AtomicU64::new(pages),
            synced_at: AtomicU64::new(0),
            clean_guard_epochs: [const { AtomicU64::new(0) }; crate::watch::CLEAN_EPOCH_SHARDS],
            ever_written: AtomicBool::new(false),
            watched: AtomicU8::new(0),
            dead: AtomicBool::new(false),
            has_pending: AtomicBool::new(false),
            pending: Mutex::default(),
            flights: Mutex::default(),
            before: Mutex::default(),
            kept: Mutex::default(),
            scanned: Mutex::default(),
            used: AtomicU64::new(crate::host::moment()),
            asked: [AtomicU64::new(u64::MAX), AtomicU64::new(0)],
            unsized_asks: AtomicU64::new(0),
            id: {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                NEXT.fetch_add(1, Ordering::Relaxed)
            },
            card: None,
            window: false,
            place: None,
        }
    }

    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        self.used.store(crate::host::moment(), Ordering::Relaxed);
        list.note_shadow_gpu_write(self.id);
        if self.place.is_some() {
            return;
        }
        self.ever_written.store(true, Ordering::Relaxed);
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    pub(crate) fn writing(&self, address: usize, len: usize) {
        if self.place.is_some() {
            return;
        }
        let range = (address, address + len);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if relay() {
            self.flights
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .declare(range, PREPARED.load(Ordering::Relaxed));
        }
        if pending.contains(&range) {
            return;
        }
        if self.card.is_some() {
            pending.push(range);
            self.has_pending.store(true, Ordering::Release);
            return;
        }
        let mut parts = vec![self.inside(address, len)];
        for &(a, e) in pending.iter() {
            let (a, e) = self.inside(a, e - a);
            let mut rest = Vec::with_capacity(parts.len() + 1);
            for (from, to) in parts {
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
            parts = rest;
        }
        pending.push(range);
        self.has_pending.store(true, Ordering::Release);
        let taken: usize = parts
            .iter()
            .map(|&(from, to)| to.saturating_sub(from))
            .sum();
        BACK_CLASS[usize::from(!self.window)].fetch_add(taken as u64, Ordering::Relaxed);
        let total = self.len as usize;
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = before.get_or_insert_with(|| pooled(total));
        if keeps() {
            let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            if kept.0.is_empty() {
                kept.0 = vec![false; self.pieces(0, total.max(1)).end];
            }
            for (from, to) in parts {
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
                        std::ptr::copy_nonoverlapping(
                            self.data.add(lo),
                            snapshot.as_mut_ptr().add(lo),
                            hi - lo,
                        );
                    }
                    kept.0[piece] = whole;
                }
            }
            return;
        }
        if wide_back() {
            let mut snaps = SNAPS.lock().unwrap_or_else(|e| e.into_inner());
            let (data, snapshot) = (self.data as usize, snapshot.as_mut_ptr() as usize);
            snaps.extend(
                parts
                    .into_iter()
                    .filter(|&(from, to)| from < to)
                    .map(|(from, to)| (data + from, snapshot + from, to - from)),
            );
            return;
        }
        for (from, to) in parts {
            if from < to {
                // SAFETY: the copy's mapping and the snapshot's allocation
                // (`pooled`) both hold `total` bytes, and `to <= total`.
                unsafe {
                    copy_wide(
                        self.data.add(from),
                        snapshot.as_mut_ptr().add(from),
                        to - from,
                    );
                }
            }
        }
    }

    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        self.copy_back_with(address, len, None)
    }

    pub(crate) fn copy_back_with(
        &self,
        address: usize,
        len: usize,
        found: Option<Vec<(usize, usize)>>,
    ) -> usize {
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
            let before =
                unsafe { std::slice::from_raw_parts(before.as_ptr().add(from), to - from) };
            #[cfg(feature = "diagnostics")]
            let scanning = std::time::Instant::now();
            let differing = match found {
                Some(found) => found,
                None => self.differing(from, to, before),
            };
            #[cfg(feature = "diagnostics")]
            let syncing = {
                use crate::copystat::{C, add};
                add(C::BackScanUs, scanning.elapsed().as_micros() as u64);
                add(C::BackDiffer, differing.len() as u64);
                std::time::Instant::now()
            };
            for &(at, n) in &differing {
                self.changed(at, at + n);
            }
            let (mut title, mut mask, mut runs) = (Vec::new(), Vec::new(), Vec::new());
            for (at, n) in differing {
                // SAFETY: inside the mapping; only the submitting thread
                // writes it.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &before[at - from..at - from + n];
                let read = |first: usize, title: &mut [u8]| {
                    crate::host::read_into((start + at + first) as *const u8, title)
                };
                let Some((first, n)) =
                    back_piece(host, old, &mut title, &mut mask, &mut runs, read)
                else {
                    continue;
                };
                let at = at + first;
                crate::watch::writing(start + at, n);
                for &(off, len) in &runs {
                    crate::host::write_mapped(
                        (start + at + off) as *mut u8,
                        title[off..].as_ptr(),
                        len,
                    );
                    written += len;
                }
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(
                crate::copystat::C::BackSyncUs,
                syncing.elapsed().as_micros() as u64,
            );
        }
        written
    }

    fn compared(&self, from: usize, to: usize) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackCompared, (to - from) as u64);
        let (bytes, ranges) = if self.window { (2, 5) } else { (3, 6) };
        BACK_CLASS[bytes].fetch_add((to - from) as u64, Ordering::Relaxed);
        BACK_CLASS[ranges].fetch_add(1, Ordering::Relaxed);
        if !self.window && self.len >= 1 << 20 {
            BACK_CLASS[4].fetch_add((to - from) as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn relay_compared(&self, address: usize, len: usize) {
        let (from, to) = self.inside(address, len);
        self.compared(from, to);
    }

    pub(crate) fn relay_merge(
        &self,
        at: usize,
        n: usize,
        now: &mut Vec<u8>,
        title: &mut Vec<u8>,
    ) -> usize {
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
        if !crate::host::read_into((start + at) as *const u8, title) {
            return 0;
        }
        let as_read = merge_race_on().then(|| title.clone());
        if !merge_piece(now, held, title) {
            return 0;
        }
        crate::watch::writing(start + at, n);
        merge_race(
            as_read.as_deref(),
            (self.start, self.len),
            start + at,
            &title[..],
            "pc relay",
        );
        crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
        n
    }

    fn differing(&self, from: usize, to: usize, before: &[u8]) -> Vec<(usize, usize)> {
        let (start, data) = (self.start as usize, self.data as usize);
        let scan = |a: usize, b: usize| -> Vec<(usize, usize)> {
            let mut found = Vec::new();
            let mut at = a;
            while at < b {
                let n = (PIECE - (start + at) % PIECE).min(b - at);
                // SAFETY: inside the mapping, which nothing writes while a
                // submission's results are compared.
                let host = unsafe { std::slice::from_raw_parts((data + at) as *const u8, n) };
                if host != &before[at - from..at - from + n] {
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

    pub(crate) fn copy_back_done(&self, ranges: &[(usize, usize)]) {
        if self.place.is_some() {
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if relay() {
            let free = self
                .flights
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .done(ranges);
            pending.retain(|r| !free.contains(r));
        } else {
            pending.retain(|r| !ranges.contains(r));
        }
        self.has_pending.store(!pending.is_empty(), Ordering::Release);
        if !pending.is_empty() || self.card.is_some() {
            return;
        }
        if keeps() {
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
            kept.0.clear();
        }
        if let Some(bytes) = self.before.lock().unwrap_or_else(|e| e.into_inner()).take() {
            give_back(bytes);
        }
    }

    fn pieces(&self, from: usize, to: usize) -> std::ops::Range<usize> {
        pieces_of(self.start as usize, from, to)
    }

    fn piece(&self, piece: usize) -> (usize, usize) {
        piece_at(self.start as usize, self.len as usize, piece)
    }

    fn changed(&self, from: usize, to: usize) {
        if !keeps() || from >= to {
            return;
        }
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if kept.0.is_empty() {
            return;
        }
        for piece in self.pieces(from, to) {
            kept.0[piece] = false;
        }
    }

    #[inline]
    pub(crate) fn writes_pending(&self) -> bool {
        self.has_pending.load(Ordering::Acquire)
    }

    pub(crate) fn byte_len(&self) -> u64 {
        self.len
    }

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
        let global_synced = if self.uncopied.load(Ordering::Acquire) == 0 {
            self.synced_at.load(Ordering::Acquire)
        } else {
            0
        };
        let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        busy.into_iter()
            .any(|(a, e)| match crate::watch::asked(a, e) {
                Some((_, written)) => !copied
                    .stale_pages_in(base, global_synced, a, e, &written)
                    .is_empty(),
                None => true,
            })
    }

    #[cfg(feature = "diagnostics")]
    fn log_why_stale(reason: &'static str, shadow: &Self, detail: usize, a: usize, e: usize) {
        static HITS: AtomicU64 = AtomicU64::new(0);
        let hit = HITS.fetch_add(1, Ordering::Relaxed);
        if hit < 16 {
            tracing::info!(
                hit,
                reason,
                shadow = format_args!("{:#x}+{:#x}", shadow.start, shadow.len),
                range = format_args!("{a:#x}..{e:#x}"),
                carded = shadow.card.is_some(),
                window = shadow.window,
                detail,
                "WHY_STALE merge_under_writes fallback"
            );
        }
    }

    pub(crate) fn merge_under_writes(
        &self,
        address: usize,
        len: usize,
        runs_left: &mut usize,
    ) -> bool {
        self.merge_under_writes_wide(address, len, runs_left, false)
    }

    pub(crate) fn merge_under_writes_wide(
        &self,
        address: usize,
        len: usize,
        runs_left: &mut usize,
        allow_wide: bool,
    ) -> bool {
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
        let mode = self.watched.load(Ordering::Relaxed);
        if mode == NOT_WATCHED {
            return self.merge_unwatched(start, &busy, runs_left);
        }
        let base = start / PAGE;
        thread_local! {
            static MERGE_TITLE_BUF: std::cell::RefCell<Vec<u8>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        let title = MERGE_TITLE_BUF.with(|cell| {
            let mut v = cell.take();
            if v.len() < PAGE {
                v.resize(PAGE, 0);
            }
            v
        });
        struct ReturnBuf(Vec<u8>);
        impl Drop for ReturnBuf {
            fn drop(&mut self) {
                let v = std::mem::take(&mut self.0);
                MERGE_TITLE_BUF.with(|cell| {
                    *cell.borrow_mut() = v;
                });
            }
        }
        let mut title_guard = ReturnBuf(title);
        let title = &mut title_guard.0;
        for (a, e) in busy {
            let full = a <= start && e >= end;
            let uncopied_zero = self.uncopied.load(Ordering::Acquire) == 0;
            let global_synced = if uncopied_zero {
                self.synced_at.load(Ordering::Acquire)
            } else {
                0
            };
            let lo_page = (a / PAGE).saturating_sub(base);
            let hi_page = e.div_ceil(PAGE).saturating_sub(base);
            let sub_shards = if mode == WATCHED_GUARDED && !full {
                crate::watch::shards_for_range(a, e)
            } else {
                None
            };
            let mut sub_clean_hit = false;
            let (all_copied, since) = if uncopied_zero && full {
                (true, global_synced)
            } else if mode == WATCHED_GUARDED {
                let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                let (all, s) = copied.range_since(lo_page, hi_page, global_synced);
                if all
                    && s != 0
                    && let Some((shards, count)) = sub_shards
                {
                    sub_clean_hit = copied.clean_covers_shards(shards, count, lo_page, hi_page, s);
                }
                (all, s)
            } else {
                (uncopied_zero, 0)
            };
            if mode == WATCHED_GUARDED && all_copied && since != 0 {
                let full_epochs = std::array::from_fn(|i| {
                    self.clean_guard_epochs[i].load(Ordering::Acquire)
                });
                if sub_clean_hit
                    || crate::watch::guarded_clean_epochs_match(start, end, full_epochs, since)
                {
                    continue;
                }
            }
            let (found, clean_epochs) = match mode {
                WATCHED_GUARDED => {
                    match crate::watch::writes_guarded_since_with_clean(a, e, since) {
                        Some((now, written, epochs)) => (Some((now, written)), epochs),
                        None => (None, None),
                    }
                }
                WATCHED_KERNEL => (crate::watch::writes(a, e, true), None),
                _ => match crate::watch::writes_kind(a, e, true) {
                    Some((now, written, kind)) => {
                        self.watched.store(
                            match kind {
                                crate::watch::WatchKind::WriteWatch => WATCHED_KERNEL,
                                crate::watch::WatchKind::Guarded => WATCHED_GUARDED,
                            },
                            Ordering::Relaxed,
                        );
                        (Some((now, written)), None)
                    }
                    None => (None, None),
                },
            };
            let Some((now, written)) = found else {
                #[cfg(feature = "diagnostics")]
                Self::log_why_stale("writes_none", self, 0, a, e);
                return true;
            };
            if let Some(ep) = clean_epochs {
                if full {
                    self.clean_guard_epochs[0].store(0, Ordering::Release);
                    for i in (1..crate::watch::CLEAN_EPOCH_SHARDS).rev() {
                        self.clean_guard_epochs[i].store(ep[i], Ordering::Release);
                    }
                    self.clean_guard_epochs[0].store(ep[0], Ordering::Release);
                } else if let Some((shards, count)) = sub_shards {
                    self.copied
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .record_clean_shards(shards, count, ep, lo_page, hi_page);
                }
            }
            if all_copied && written.is_empty() {
                continue;
            }
            let stale: Vec<usize> = {
                let global_synced = if self.uncopied.load(Ordering::Acquire) == 0 {
                    self.synced_at.load(Ordering::Acquire)
                } else {
                    0
                };
                let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                copied.stale_pages_in(base, global_synced, a, e, &written)
            };
            if stale.is_empty() {
                continue;
            }
            if self.card.is_none() {
                let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
                let Some(snapshot) = before.as_mut() else {
                    #[cfg(feature = "diagnostics")]
                    Self::log_why_stale("no_before", self, 0, a, e);
                    return true;
                };
                let mut mark_pages: Vec<usize> = Vec::with_capacity(stale.len());
                if allow_wide && stale.len() >= 4 {
                    let snap_ptr = snapshot.as_mut_ptr() as usize;
                    let data_ptr = self.data as usize;
                    let chunk_n = stale.len().div_ceil(stale_worker_count()).max(2);
                    let pages_out =
                        std::sync::Arc::new(Mutex::new(Vec::<usize>::with_capacity(stale.len())));
                    let jobs: Vec<Job> = stale
                        .chunks(chunk_n)
                        .map(|chunk| {
                            let pages = chunk.to_vec();
                            let pages_out = pages_out.clone();
                            Box::new(move || {
                                let mut local_title = vec![0u8; PAGE];
                                let mut local_done = Vec::with_capacity(pages.len());
                                let mut local_pages = 0u64;
                                let mut local_runs = 0u64;
                                let mut local_bytes = 0u64;
                                for page in pages {
                                    let (lo, hi) =
                                        ((page * PAGE).max(a), ((page + 1) * PAGE).min(e));
                                    if lo >= hi {
                                        continue;
                                    }
                                    let n = hi - lo;
                                    let t = &mut local_title[..n];
                                    if !crate::host::read_into(lo as *const u8, t) {
                                        continue;
                                    }
                                    local_pages += 1;
                                    let at = lo - start;
                                    // SAFETY: `stale` pages are unique so every worker operates
                                    // on disjoint `[at, at + n)` slices of `snapshot` and `self.data`,
                                    // while `self.before` is locked on the calling thread.
                                    let held = unsafe {
                                        std::slice::from_raw_parts_mut(
                                            (snap_ptr as *mut u8).add(at),
                                            n,
                                        )
                                    };
                                    let mut runs_n = 0u64;
                                    let mut bytes_n = 0u64;
                                    for_each_changed_run(t, held, |r, m| {
                                        // SAFETY: disjoint bytes inside the copy's mapping.
                                        unsafe {
                                            std::ptr::copy_nonoverlapping(
                                                t.as_ptr().add(r),
                                                (data_ptr as *mut u8).add(at + r),
                                                m,
                                            );
                                        }
                                        runs_n += 1;
                                        bytes_n += m as u64;
                                    });
                                    if runs_n > 0 {
                                        held.copy_from_slice(t);
                                        local_runs += runs_n;
                                        local_bytes += bytes_n;
                                    }
                                    if (page * PAGE).max(start) >= a
                                        && ((page + 1) * PAGE).min(end) <= e
                                    {
                                        local_done.push(page);
                                    }
                                }
                                if local_pages > 0 {
                                    MERGED[0].fetch_add(local_pages, Ordering::Relaxed);
                                }
                                if local_runs > 0 {
                                    MERGED[1].fetch_add(local_runs, Ordering::Relaxed);
                                    MERGED[2].fetch_add(local_bytes, Ordering::Relaxed);
                                }
                                if !local_done.is_empty() {
                                    pages_out
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .extend_from_slice(&local_done);
                                }
                            }) as Job
                        })
                        .collect();
                    wide_stale_run(jobs);
                    mark_pages = std::mem::take(
                        &mut *pages_out.lock().unwrap_or_else(|e| e.into_inner()),
                    );
                } else {
                    let mut total_pages = 0u64;
                    let mut total_runs = 0u64;
                    let mut total_bytes = 0u64;
                    for page in stale {
                        let (lo, hi) = ((page * PAGE).max(a), ((page + 1) * PAGE).min(e));
                        if lo >= hi {
                            continue;
                        }
                        let n = hi - lo;
                        let title = &mut title[..n];
                        if !crate::host::read_into(lo as *const u8, title) {
                            continue;
                        }
                        total_pages += 1;
                        let at = lo - start;
                        // SAFETY: the snapshot's allocation holds the copy's
                        // bytes at the copy's offsets (`writing`), and these
                        // are in a declared range.
                        let held = unsafe {
                            std::slice::from_raw_parts_mut(snapshot.as_mut_ptr().add(at), n)
                        };
                        let mut runs_n = 0u64;
                        let mut bytes_n = 0u64;
                        for_each_changed_run(title, held, |r, m| {
                            // SAFETY: inside the copy's mapping. The card may be
                            // writing the copy beside these bytes; they are
                            // written here and nothing around them.
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    title.as_ptr().add(r),
                                    self.data.add(at + r),
                                    m,
                                )
                            };
                            runs_n += 1;
                            bytes_n += m as u64;
                        });
                        if runs_n > 0 {
                            held.copy_from_slice(title);
                            total_runs += runs_n;
                            total_bytes += bytes_n;
                        }
                        if (page * PAGE).max(start) >= a && ((page + 1) * PAGE).min(end) <= e {
                            mark_pages.push(page);
                        }
                    }
                    if total_pages > 0 {
                        MERGED[0].fetch_add(total_pages, Ordering::Relaxed);
                    }
                    if total_runs > 0 {
                        MERGED[1].fetch_add(total_runs, Ordering::Relaxed);
                        MERGED[2].fetch_add(total_bytes, Ordering::Relaxed);
                    }
                }
                drop(before);
                if !mark_pages.is_empty() {
                    let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                    let mut newly_copied = 0u64;
                    for page in mark_pages {
                        if page >= base
                            && page - base < copied.len()
                            && copied.set_page(page - base, now)
                        {
                            newly_copied += 1;
                        }
                    }
                    if newly_copied > 0 {
                        self.uncopied.fetch_sub(newly_copied, Ordering::Release);
                    }
                }
                continue;
            }
            let card = self.card.as_ref().unwrap();
            let mut mark_pages: Vec<usize> = Vec::with_capacity(stale.len());
            let mut clear_spans: Vec<(usize, usize)> = Vec::new();
            let mut dirty_spans: Vec<(usize, usize)> = Vec::new();
            let mut masked_pushes: Vec<(usize, Vec<u8>, Vec<u8>)> = Vec::new();
            let mut total_pages = 0u64;
            let mut total_runs = 0u64;
            let mut total_bytes = 0u64;
            {
                let _before = self.before.lock().unwrap_or_else(|e| e.into_inner());
                let mut old = card.old.lock().unwrap_or_else(|e| e.into_inner());
                if allow_wide && stale.len() >= 4 {
                    let mut work: Vec<(usize, usize, usize, usize, usize, usize)> =
                        Vec::with_capacity(stale.len());
                    for page in stale {
                        let (lo, hi) = ((page * PAGE).max(a), ((page + 1) * PAGE).min(e));
                        if lo >= hi {
                            continue;
                        }
                        let n = hi - lo;
                        let at = lo - start;
                        let p = self.pieces(at, at + n).start;
                        let (p_lo, _) = self.piece(p);
                        let Some(piece_buf) = old.get_mut(&p) else {
                            #[cfg(feature = "diagnostics")]
                            Self::log_why_stale("no_card_old", self, p, lo, hi);
                            return true;
                        };
                        let off = at - p_lo;
                        let held_ptr = piece_buf[off..off + n].as_mut_ptr() as usize;
                        work.push((page, lo, hi, at, held_ptr, n));
                    }
                    if !work.is_empty() {
                        #[derive(Default)]
                        struct CardMergeChunk {
                            pages: u64,
                            runs: u64,
                            bytes: u64,
                            runs_spent: usize,
                            clear_spans: Vec<(usize, usize)>,
                            dirty_spans: Vec<(usize, usize)>,
                            masked_pushes: Vec<(usize, Vec<u8>, Vec<u8>)>,
                            mark_pages: Vec<usize>,
                        }
                        let card_size = card.size as usize;
                        let chunk_n = work.len().div_ceil(stale_worker_count()).max(2);
                        let n_chunks = work.len().div_ceil(chunk_n);
                        let per_chunk_runs = (*runs_left).div_ceil(n_chunks);
                        let mut results: Vec<CardMergeChunk> =
                            (0..n_chunks).map(|_| CardMergeChunk::default()).collect();
                        let results_base = results.as_mut_ptr() as usize;
                        let jobs: Vec<Job> = work
                            .chunks(chunk_n)
                            .enumerate()
                            .map(|(ci, chunk)| {
                                let items = chunk.to_vec();
                                Box::new(move || {
                                    let mut local_title = vec![0u8; PAGE];
                                    let mut out = CardMergeChunk {
                                        mark_pages: Vec::with_capacity(items.len()),
                                        ..CardMergeChunk::default()
                                    };
                                    let mut local_runs_left = per_chunk_runs;
                                    for (page, lo, _hi, at, held_ptr, n) in items {
                                        let t = &mut local_title[..n];
                                        if !crate::host::read_into(lo as *const u8, t) {
                                            continue;
                                        }
                                        out.pages += 1;
                                        let max_runs = local_runs_left.min(16);
                                        // SAFETY: `stale` pages are strictly unique, so every
                                        // `(held_ptr, n)` slice refers to a disjoint byte range
                                        // inside `card.old` while `self.before` and `card.old`
                                        // remain locked on the calling thread until `wide_stale_run`
                                        // joins all workers.
                                        let held = unsafe {
                                            std::slice::from_raw_parts_mut(held_ptr as *mut u8, n)
                                        };
                                        match diff_runs_or_masked(at, card_size, t, held, max_runs)
                                        {
                                            MergeDiff::Empty => {}
                                            MergeDiff::Runs(runs) => {
                                                let spent = runs.len();
                                                local_runs_left =
                                                    local_runs_left.saturating_sub(spent);
                                                out.runs_spent += spent;
                                                for (r, m) in runs {
                                                    out.runs += 1;
                                                    out.bytes += m as u64;
                                                    let (c_from, c_to) = (at + r, at + r + m);
                                                    out.clear_spans.push((c_from, c_to));
                                                    out.dirty_spans.push((c_from, c_to));
                                                }
                                            }
                                            MergeDiff::Masked {
                                                offset,
                                                data,
                                                mask,
                                                changed_runs,
                                                changed_bytes,
                                                first_off,
                                                last_end,
                                            } => {
                                                out.runs += changed_runs as u64;
                                                out.bytes += changed_bytes as u64;
                                                out.clear_spans.push((first_off, last_end));
                                                out.masked_pushes.push((offset, data, mask));
                                            }
                                        }
                                        if (page * PAGE).max(start) >= a
                                            && ((page + 1) * PAGE).min(end) <= e
                                        {
                                            out.mark_pages.push(page);
                                        }
                                    }
                                    // SAFETY: `ci` is unique in `0..n_chunks` and `results`
                                    // outlives `wide_stale_run`.
                                    unsafe {
                                        *(results_base as *mut CardMergeChunk).add(ci) = out;
                                    }
                                }) as Job
                            })
                            .collect();
                        wide_stale_run(jobs);
                        for chunk_out in results {
                            total_pages += chunk_out.pages;
                            total_runs += chunk_out.runs;
                            total_bytes += chunk_out.bytes;
                            *runs_left = runs_left.saturating_sub(chunk_out.runs_spent);
                            clear_spans.extend(chunk_out.clear_spans);
                            dirty_spans.extend(chunk_out.dirty_spans);
                            masked_pushes.extend(chunk_out.masked_pushes);
                            mark_pages.extend(chunk_out.mark_pages);
                        }
                    }
                } else {
                    for page in stale {
                        let (lo, hi) = ((page * PAGE).max(a), ((page + 1) * PAGE).min(e));
                        if lo >= hi {
                            continue;
                        }
                        let n = hi - lo;
                        let title = &mut title[..n];
                        if !crate::host::read_into(lo as *const u8, title) {
                            continue;
                        }
                        total_pages += 1;
                        let at = lo - start;
                        let p = self.pieces(at, at + n).start;
                        let (p_lo, _) = self.piece(p);
                        let max_runs = (*runs_left).min(16);
                        let Some(piece_buf) = old.get_mut(&p) else {
                            #[cfg(feature = "diagnostics")]
                            Self::log_why_stale("no_card_old", self, p, lo, hi);
                            return true;
                        };
                        let held = &mut piece_buf[at - p_lo..at - p_lo + n];
                        let diff = diff_runs_or_masked(at, card.size as usize, title, held, max_runs);
                        match diff {
                            MergeDiff::Empty => {}
                            MergeDiff::Runs(runs) => {
                                *runs_left = runs_left.saturating_sub(runs.len());
                                for (r, m) in runs {
                                    total_runs += 1;
                                    total_bytes += m as u64;
                                    let (c_from, c_to) = (at + r, at + r + m);
                                    clear_spans.push((c_from, c_to));
                                    dirty_spans.push((c_from, c_to));
                                }
                            }
                            MergeDiff::Masked {
                                offset,
                                data,
                                mask,
                                changed_runs,
                                changed_bytes,
                                first_off,
                                last_end,
                            } => {
                                total_runs += changed_runs as u64;
                                total_bytes += changed_bytes as u64;
                                clear_spans.push((first_off, last_end));
                                masked_pushes.push((offset, data, mask));
                            }
                        }
                        if (page * PAGE).max(start) >= a && ((page + 1) * PAGE).min(end) <= e {
                            mark_pages.push(page);
                        }
                    }
                }
            }
            if total_pages > 0 {
                MERGED[0].fetch_add(total_pages, Ordering::Relaxed);
            }
            if total_runs > 0 {
                MERGED[1].fetch_add(total_runs, Ordering::Relaxed);
                MERGED[2].fetch_add(total_bytes, Ordering::Relaxed);
            }
            if !clear_spans.is_empty() && card.has_hot_hashes.load(Ordering::Acquire) {
                for (ca, cb) in coalesce_hash_spans(&clear_spans) {
                    card.clear_hot_hashes(ca, cb);
                }
            }
            if !dirty_spans.is_empty() {
                let mut dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
                for (c_from, c_to) in dirty_spans {
                    match dirty.last_mut() {
                        Some(last) if c_from <= last.1 && c_to >= last.0 => {
                            *last = (last.0.min(c_from), last.1.max(c_to))
                        }
                        _ => dirty.push((c_from, c_to)),
                    }
                }
            }
            for (offset, data, mask) in masked_pushes {
                self.card_masked_push(offset, data, mask);
            }
            if !mark_pages.is_empty() {
                let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                let mut newly_copied = 0u64;
                for page in mark_pages {
                    if page >= base
                        && page - base < copied.len()
                        && copied.set_page(page - base, now)
                    {
                        newly_copied += 1;
                    }
                }
                if newly_copied > 0 {
                    self.uncopied.fetch_sub(newly_copied, Ordering::Release);
                }
            }
        }
        false
    }

    fn merge_unwatched(
        &self,
        start: usize,
        busy: &[(usize, usize)],
        runs_left: &mut usize,
    ) -> bool {
        if let Some(card) = &self.card {
            if !card.has_old.load(Ordering::Acquire) {
                #[cfg(feature = "diagnostics")]
                if let Some(&(a, e)) = busy.first() {
                    Self::log_why_stale("no_card_old", self, 0, a, e);
                }
                return true;
            }
            let mut piece_title = vec![0u8; PIECE];
            let mut diffs: Vec<(usize, MergeDiff)> = Vec::new();
            for &(a, e) in busy {
                let first_piece = self.pieces(a - start, e - start);
                for p in first_piece {
                    let (p_lo, p_hi) = self.piece(p);
                    let c_lo = (a - start).max(p_lo);
                    let c_hi = (e - start).min(p_hi);
                    if c_lo >= c_hi {
                        continue;
                    }
                    let n = c_hi - c_lo;
                    let title = &mut piece_title[..n];
                    diffs.clear();
                    {
                        let _before = self.before.lock().unwrap_or_else(|e| e.into_inner());
                        if !crate::host::read_into((start + c_lo) as *const u8, title) {
                            continue;
                        }
                        let mut old = card.old.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(piece_buf) = old.get_mut(&p) else {
                            #[cfg(feature = "diagnostics")]
                            Self::log_why_stale("no_card_old", self, p, start + c_lo, start + c_hi);
                            return true;
                        };
                        let held_piece = &mut piece_buf[c_lo - p_lo..c_hi - p_lo];
                        if title == held_piece {
                            continue;
                        }
                        let lo_page = (start + c_lo) / PAGE;
                        let hi_page = (start + c_hi).div_ceil(PAGE);
                        for page in lo_page..hi_page {
                            let pg_lo = (page * PAGE).max(start + c_lo) - start;
                            let pg_hi = ((page + 1) * PAGE).min(start + c_hi) - start;
                            if pg_lo >= pg_hi {
                                continue;
                            }
                            let t_pg = &title[pg_lo - c_lo..pg_hi - c_lo];
                            let h_pg = &mut held_piece[pg_lo - c_lo..pg_hi - c_lo];
                            if t_pg == h_pg {
                                continue;
                            }
                            MERGED[0].fetch_add(1, Ordering::Relaxed);
                            let max_runs = (*runs_left).min(16);
                            let diff = diff_runs_or_masked(
                                pg_lo,
                                card.size as usize,
                                t_pg,
                                h_pg,
                                max_runs,
                            );
                            if let MergeDiff::Runs(ref runs) = diff {
                                *runs_left = runs_left.saturating_sub(runs.len());
                            }
                            if !matches!(diff, MergeDiff::Empty) {
                                diffs.push((pg_lo, diff));
                            }
                        }
                    }
                    for (pg_lo, diff) in diffs.drain(..) {
                        match diff {
                            MergeDiff::Empty => {}
                            MergeDiff::Runs(runs) => {
                                for (r, m) in runs {
                                    MERGED[1].fetch_add(1, Ordering::Relaxed);
                                    MERGED[2].fetch_add(m as u64, Ordering::Relaxed);
                                    let (c_from, c_to) = (pg_lo + r, pg_lo + r + m);
                                    card.clear_hot_hashes(c_from, c_to);
                                    self.card_changed(c_from, c_to);
                                }
                            }
                            MergeDiff::Masked {
                                offset,
                                data,
                                mask,
                                changed_runs,
                                changed_bytes,
                                first_off,
                                last_end,
                            } => {
                                MERGED[1].fetch_add(changed_runs as u64, Ordering::Relaxed);
                                MERGED[2].fetch_add(changed_bytes as u64, Ordering::Relaxed);
                                card.clear_hot_hashes(first_off, last_end);
                                self.card_masked_push(offset, data, mask);
                            }
                        }
                    }
                }
            }
            return false;
        }
        const MAX_UNWATCHED_NONCARDED: usize = 1 << 20;
        let mut title = vec![0u8; PAGE];
        for &(a, e) in busy {
            if e - a > MAX_UNWATCHED_NONCARDED {
                #[cfg(feature = "diagnostics")]
                Self::log_why_stale("not_watched_big", self, e - a, a, e);
                return true;
            }
            let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
            let Some(snapshot) = before.as_mut() else {
                #[cfg(feature = "diagnostics")]
                Self::log_why_stale("no_before", self, 0, a, e);
                return true;
            };
            for page in (a / PAGE)..e.div_ceil(PAGE) {
                let (lo, hi) = ((page * PAGE).max(a), ((page + 1) * PAGE).min(e));
                if lo >= hi {
                    continue;
                }
                let n = hi - lo;
                let t = &mut title[..n];
                if !crate::host::read_into(lo as *const u8, t) {
                    continue;
                }
                let at = lo - start;
                // SAFETY: the snapshot's allocation holds the copy's bytes at
                // the copy's offsets (`writing`), and these are in a declared range.
                let held =
                    unsafe { std::slice::from_raw_parts_mut(snapshot.as_mut_ptr().add(at), n) };
                if t == held {
                    continue;
                }
                MERGED[0].fetch_add(1, Ordering::Relaxed);
                let mut runs_n = 0u64;
                let mut bytes_n = 0u64;
                for_each_changed_run(t, held, |r, m| {
                    // SAFETY: inside the copy's mapping. The card may be writing
                    // the copy beside these bytes; they are written here and
                    // nothing around them.
                    unsafe {
                        std::ptr::copy_nonoverlapping(t.as_ptr().add(r), self.data.add(at + r), m)
                    };
                    runs_n += 1;
                    bytes_n += m as u64;
                });
                if runs_n > 0 {
                    held.copy_from_slice(t);
                    MERGED[1].fetch_add(runs_n, Ordering::Relaxed);
                    MERGED[2].fetch_add(bytes_n, Ordering::Relaxed);
                }
            }
        }
        false
    }

    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        if self.place.is_some() {
            return 0;
        }
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        if !busy.is_empty() {
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
        // When no GPU writes are pending on this shadow and some 64 KiB pages have never been
        // copied yet (`uncopied > 0`), align `(from, to)` outward to 64 KiB page boundaries
        // (clamped to `[start, end)`). Otherwise unaligned edge refreshes leave `copied[page] == 0`
        // forever, preventing `all_copied` / `clean_guard_epochs` from ever engaging and causing
        // repeated re-copies on every subsequent submission.
        let (from, to) = if self.uncopied.load(Ordering::Acquire) > 0
            && !self.writes_pending()
            && self.watched.load(Ordering::Relaxed) != NOT_WATCHED
        {
            (
                (from & !(PAGE - 1)).max(start),
                to.next_multiple_of(PAGE).min(end),
            )
        } else {
            (from, to)
        };
        self.update_free(from, to)
    }

    fn update_free(&self, from: usize, to: usize) -> usize {
        let sub = PREPARED.load(Ordering::Relaxed);
        if sub == 0 {
            return self.update_free_span(from, to);
        }
        let stamp = crate::watch::write_stamp();
        let todo = self
            .scanned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .uncovered(sub, stamp, from, to);
        if todo.is_empty() {
            return 0;
        }
        let mut copied = 0usize;
        for &(a, b) in &todo {
            copied += self.update_free_span(a, b);
        }
        if crate::watch::write_stamp() == stamp {
            let mut scanned = self.scanned.lock().unwrap_or_else(|e| e.into_inner());
            for (a, b) in todo {
                scanned.record(sub, stamp, a, b);
            }
        }
        copied
    }

    fn update_free_span(&self, from: usize, to: usize) -> usize {
        let mode = self.watched.load(Ordering::Relaxed);
        if mode != NOT_WATCHED {
            let (start, end) = (self.start as usize, (self.start + self.len) as usize);
            let base = start / PAGE;
            let full = from <= start && to >= end;
            let uncopied_zero = self.uncopied.load(Ordering::Acquire) == 0;
            let global_synced = if uncopied_zero {
                self.synced_at.load(Ordering::Acquire)
            } else {
                0
            };
            let lo_page = (from / PAGE).saturating_sub(base);
            let hi_page = to.div_ceil(PAGE).saturating_sub(base);
            let sub_shards = if mode == WATCHED_GUARDED && !full {
                crate::watch::shards_for_range(from, to)
            } else {
                None
            };
            let mut sub_clean_hit = false;
            let (all_copied, since) = if uncopied_zero && full {
                (true, global_synced)
            } else if mode == WATCHED_GUARDED {
                let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                let (all, s) = copied.range_since(lo_page, hi_page, global_synced);
                if all
                    && s != 0
                    && let Some((shards, count)) = sub_shards
                {
                    sub_clean_hit = copied.clean_covers_shards(shards, count, lo_page, hi_page, s);
                }
                (all, s)
            } else {
                (uncopied_zero, 0)
            };
            if mode == WATCHED_GUARDED && all_copied && since != 0 {
                let full_epochs = std::array::from_fn(|i| {
                    self.clean_guard_epochs[i].load(Ordering::Acquire)
                });
                if sub_clean_hit
                    || crate::watch::guarded_clean_epochs_match(start, end, full_epochs, since)
                {
                    self.refresh_check(from, to);
                    #[cfg(feature = "diagnostics")]
                    {
                        use crate::copystat::{C, add};
                        add(C::Checked, (to - from) as u64);
                        add(C::Unchanged, (to - from) as u64);
                    }
                    return 0;
                }
            }
            let (found, clean_epochs) = match mode {
                WATCHED_GUARDED => match crate::watch::writes_guarded_since_with_clean(
                    from, to, since,
                ) {
                    Some((now, written, epochs)) => (Some((now, written)), epochs),
                    None => (None, None),
                },
                WATCHED_KERNEL => (crate::watch::writes(from, to, true), None),
                _ => match crate::watch::writes_kind(from, to, true) {
                    Some((now, written, kind)) => {
                        self.watched.store(
                            match kind {
                                crate::watch::WatchKind::WriteWatch => WATCHED_KERNEL,
                                crate::watch::WatchKind::Guarded => WATCHED_GUARDED,
                            },
                            Ordering::Relaxed,
                        );
                        (Some((now, written)), None)
                    }
                    None => (None, None),
                },
            };
            if let Some((now, written)) = found {
                let copied = self.copy_stale(from, to, now, &written, all_copied, since);
                if let Some(ep) = clean_epochs {
                    if full {
                        self.clean_guard_epochs[0].store(0, Ordering::Release);
                        for i in (1..crate::watch::CLEAN_EPOCH_SHARDS).rev() {
                            self.clean_guard_epochs[i].store(ep[i], Ordering::Release);
                        }
                        self.clean_guard_epochs[0].store(ep[0], Ordering::Release);
                    } else if let Some((shards, count)) = sub_shards {
                        self.copied
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .record_clean_shards(shards, count, ep, lo_page, hi_page);
                    }
                }
                self.refresh_check(from, to);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        if let Some(card) = &self.card {
            let copied = self.diff_unwatched_card(card, from, to);
            self.refresh_check(from, to);
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::Checked, (to - from) as u64);
                add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
            }
            return copied;
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::WholeBytes, (to - from) as u64);
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self.copy(from, to);
        #[cfg(feature = "diagnostics")]
        crate::copystat::whole(from, to - from, started.elapsed().as_micros() as u64);
        to - from
    }

    fn refresh_check(&self, from: usize, to: usize) {
        if self.data.is_null() {
            return;
        }
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_REFRESH_CHECK").is_some_and(|v| v == "1"))
        {
            return;
        }
        const SAMPLE: usize = 256;
        static DIFFERING: Mutex<Option<std::collections::HashMap<usize, u32>>> = Mutex::new(None);
        static SAID: AtomicU64 = AtomicU64::new(0);
        let start = self.start as usize;
        let mut title = [0u8; SAMPLE];
        let mut at = from;
        while at < to {
            let n = SAMPLE.min(to - at);
            if crate::host::read_into(at as *const u8, &mut title[..n]) {
                // SAFETY: `[from, to)` lies inside the buffer (`update`),
                // so these bytes are inside the copy's mapping.
                let copy = unsafe {
                    std::slice::from_raw_parts(self.data.add(at - start).cast_const(), n)
                };
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
            at = (at / PAGE + 1) * PAGE;
        }
    }

    fn scan_card_pages(
        &self,
        card: &Card,
        from: usize,
        to: usize,
        pages: &[(usize, bool)],
    ) -> (Vec<(usize, usize)>, Vec<usize>) {
        scan_card_pages_into(
            card,
            self.start as usize,
            self.len as usize,
            from,
            to,
            pages,
            crate::frame::current(),
            hot_cool_frames(),
        )
    }

    fn diff_unwatched_card(&self, card: &Card, from: usize, to: usize) -> usize {
        let spans = scan_unwatched_card_into(card, self.start as usize, self.len as usize, from, to);
        self.copy_card_spans(card, &spans, false)
    }

    fn copy_stale(
        &self,
        from: usize,
        to: usize,
        now: u64,
        written: &[(usize, u64)],
        all_copied_hint: bool,
        since_hint: u64,
    ) -> usize {
        let base = self.start as usize / PAGE;
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let full = from <= start && to >= end;
        let uncopied_zero = self.uncopied.load(Ordering::Acquire) == 0;
        let all_copied = all_copied_hint || uncopied_zero;
        if all_copied && written.is_empty() {
            if now == 0 || now <= since_hint {
                return 0;
            }
            if full && uncopied_zero {
                self.synced_at.fetch_max(now, Ordering::Release);
                return 0;
            }
            let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            let (full_lo, full_hi) = full_page_range(start, end, from, to, base, copied.len());
            copied.advance_synced(full_lo, full_hi, now);
            return 0;
        }
        let global_synced = if uncopied_zero {
            self.synced_at.load(Ordering::Acquire)
        } else {
            0
        };
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let pages_len = copied.len();
        let (full_lo, full_hi) = full_page_range(start, end, from, to, base, pages_len);
        let lo_idx = (from / PAGE).saturating_sub(base).min(pages_len);
        let hi_idx = to.div_ceil(PAGE).saturating_sub(base).min(pages_len);
        let range_all_copied = all_copied || copied.range_since(lo_idx, hi_idx, global_synced).0;
        if range_all_copied {
            if written.is_empty() {
                if now != 0 {
                    if full && uncopied_zero {
                        drop(copied);
                        self.synced_at.fetch_max(now, Ordering::Release);
                    } else {
                        copied.advance_synced(full_lo, full_hi, now);
                    }
                }
                return 0;
            }
            if let Some(card) = &self.card {
                let mut recheck_pages: Vec<(usize, bool)> = Vec::new();
                let mut dirty_chunk: Option<usize> = None;
                for &(page, write) in written {
                    if page < base || page - base >= pages_len {
                        continue;
                    }
                    let idx = page - base;
                    let at = copied.get(idx, global_synced);
                    let is_hot = write == crate::watch::HOT_WRITE && at != 0;
                    let is_written = !is_hot && write > at;
                    if !is_hot && !is_written {
                        continue;
                    }
                    if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                        copied.set_raw(idx, now);
                        let c = idx / COPIED_CHUNK;
                        if let Some(prev_c) = dirty_chunk
                            && prev_c != c
                        {
                            copied.recompute_chunk(prev_c);
                        }
                        dirty_chunk = Some(c);
                    }
                    recheck_pages.push((page, is_hot));
                }
                if let Some(c) = dirty_chunk {
                    copied.recompute_chunk(c);
                }
                let mut bytes = 0;
                let mut cool_blocks = Vec::new();
                if !recheck_pages.is_empty() {
                    let (changed_spans, cool) = self.scan_card_pages(card, from, to, &recheck_pages);
                    cool_blocks = cool;
                    bytes += self.copy_card_spans(card, &changed_spans, false);
                }
                if now != 0 {
                    if full && uncopied_zero {
                        drop(copied);
                        self.synced_at.fetch_max(now, Ordering::Release);
                    } else {
                        copied.advance_synced(full_lo, full_hi, now);
                        drop(copied);
                    }
                } else {
                    drop(copied);
                }
                if !cool_blocks.is_empty() {
                    crate::watch::cool_hot(&cool_blocks);
                }
                return bytes;
            }
            let mut bytes = 0;
            let mut run: Option<(usize, usize)> = None;
            let mut dirty_chunk: Option<usize> = None;
            let mut cool_blocks: Vec<usize> = Vec::new();
            let frame = crate::frame::current();
            let cool_after = hot_cool_frames();
            for &(page, write) in written {
                if page < base || page - base >= pages_len {
                    continue;
                }
                let idx = page - base;
                let at = copied.get(idx, global_synced);
                let is_hot = write == crate::watch::HOT_WRITE && at != 0;
                let is_written = !is_hot && write > at;
                if !is_hot && !is_written {
                    continue;
                }
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    copied.set_raw(idx, now);
                    let c = idx / COPIED_CHUNK;
                    if let Some(prev_c) = dirty_chunk
                        && prev_c != c
                    {
                        copied.recompute_chunk(prev_c);
                    }
                    dirty_chunk = Some(c);
                }
                if is_hot {
                    if let Some((first, prev)) = run.take() {
                        bytes += self
                            .copy((first * PAGE).max(from), ((prev + 1) * PAGE).min(to));
                    }
                    let p_from = (page * PAGE).max(from);
                    let p_to = ((page + 1) * PAGE).min(to);
                    let full_page =
                        p_from == (page * PAGE).max(start) && p_to == ((page + 1) * PAGE).min(end);
                    let changed = self.copy_hot(p_from, p_to);
                    bytes += changed;
                    if copied.record_hot_quiet(idx, frame, changed > 0, full_page, cool_after) {
                        cool_blocks.push(page);
                    }
                } else {
                    copied.mark_hot_changed(idx, frame);
                    match run {
                        Some((first, prev)) if page == prev + 1 => run = Some((first, page)),
                        Some((first, prev)) => {
                            bytes += self
                                .copy((first * PAGE).max(from), ((prev + 1) * PAGE).min(to));
                            run = Some((page, page));
                        }
                        None => run = Some((page, page)),
                    }
                }
            }
            if let Some(c) = dirty_chunk {
                copied.recompute_chunk(c);
            }
            if let Some((first, prev)) = run {
                bytes += self.copy((first * PAGE).max(from), ((prev + 1) * PAGE).min(to));
            }
            if now != 0 {
                if full && uncopied_zero {
                    drop(copied);
                    self.synced_at.fetch_max(now, Ordering::Release);
                } else {
                    copied.advance_synced(full_lo, full_hi, now);
                    drop(copied);
                }
            } else {
                drop(copied);
            }
            if !cool_blocks.is_empty() {
                crate::watch::cool_hot(&cool_blocks);
            }
            return bytes;
        }
        if let Some(card) = &self.card {
            let mut writes = written.iter().peekable();
            let mut uncopied_runs: Vec<(usize, usize)> = Vec::new();
            let mut recheck_pages: Vec<(usize, bool)> = Vec::new();
            let mut newly_copied = 0u64;
            let mut dirty_chunk: Option<usize> = None;
            let last = (to - 1) / PAGE;
            for page in from / PAGE..=last {
                let write = match writes.peek() {
                    Some(&&(p, w)) if p == page => {
                        writes.next();
                        w
                    }
                    _ => 0,
                };
                let idx = page - base;
                let at = copied.get(idx, global_synced);
                if at != 0 && (write == crate::watch::HOT_WRITE || write > at) {
                    if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                        copied.set_raw(idx, now);
                        let c = idx / COPIED_CHUNK;
                        if let Some(prev_c) = dirty_chunk
                            && prev_c != c
                        {
                            copied.recompute_chunk(prev_c);
                        }
                        dirty_chunk = Some(c);
                    }
                    recheck_pages.push((page, write == crate::watch::HOT_WRITE));
                } else if at == 0 {
                    if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                        if copied.set_raw(idx, now) {
                            newly_copied += 1;
                        }
                        let c = idx / COPIED_CHUNK;
                        if let Some(prev_c) = dirty_chunk
                            && prev_c != c
                        {
                            copied.recompute_chunk(prev_c);
                        }
                        dirty_chunk = Some(c);
                    }
                    let p_from = (page * PAGE).max(from);
                    let p_to = ((page + 1) * PAGE).min(to);
                    push_coalesced_span(&mut uncopied_runs, p_from, p_to);
                }
            }
            if let Some(c) = dirty_chunk {
                copied.recompute_chunk(c);
            }
            if newly_copied > 0 {
                self.uncopied.fetch_sub(newly_copied, Ordering::Release);
            }
            let mut bytes = 0;
            for (r_from, r_to) in uncopied_runs {
                bytes += self.copy(r_from, r_to);
            }
            let mut cool_blocks = Vec::new();
            if !recheck_pages.is_empty() {
                let (changed_spans, cool) = self.scan_card_pages(card, from, to, &recheck_pages);
                cool_blocks = cool;
                bytes += self.copy_card_spans(card, &changed_spans, false);
            }
            if now != 0 {
                if full && self.uncopied.load(Ordering::Acquire) == 0 {
                    drop(copied);
                    self.synced_at.fetch_max(now, Ordering::Release);
                } else {
                    copied.advance_synced(full_lo, full_hi, now);
                    drop(copied);
                }
            } else {
                drop(copied);
            }
            if !cool_blocks.is_empty() {
                crate::watch::cool_hot(&cool_blocks);
            }
            return bytes;
        }
        let mut writes = written.iter().peekable();
        let (mut bytes, mut run): (usize, Option<usize>) = (0, None);
        let mut newly_copied = 0u64;
        let mut dirty_chunk: Option<usize> = None;
        let mut cool_blocks: Vec<usize> = Vec::new();
        let frame = crate::frame::current();
        let cool_after = hot_cool_frames();
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let idx = page - base;
            let at = copied.get(idx, global_synced);
            if write == crate::watch::HOT_WRITE && at != 0 {
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    copied.set_raw(idx, now);
                    let c = idx / COPIED_CHUNK;
                    if let Some(prev_c) = dirty_chunk
                        && prev_c != c
                    {
                        copied.recompute_chunk(prev_c);
                    }
                    dirty_chunk = Some(c);
                }
                if let Some(first) = run.take() {
                    bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
                }
                let p_from = (page * PAGE).max(from);
                let p_to = ((page + 1) * PAGE).min(to);
                let full_page =
                    p_from == (page * PAGE).max(start) && p_to == ((page + 1) * PAGE).min(end);
                let changed = self.copy_hot(p_from, p_to);
                bytes += changed;
                if copied.record_hot_quiet(idx, frame, changed > 0, full_page, cool_after) {
                    cool_blocks.push(page);
                }
            } else if at == 0 || write > at {
                copied.mark_hot_changed(idx, frame);
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    if copied.set_raw(idx, now) {
                        newly_copied += 1;
                    }
                    let c = idx / COPIED_CHUNK;
                    if let Some(prev_c) = dirty_chunk
                        && prev_c != c
                    {
                        copied.recompute_chunk(prev_c);
                    }
                    dirty_chunk = Some(c);
                }
                run.get_or_insert(page);
            } else if let Some(first) = run.take() {
                bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
            }
        }
        if let Some(c) = dirty_chunk {
            copied.recompute_chunk(c);
        }
        if newly_copied > 0 {
            self.uncopied.fetch_sub(newly_copied, Ordering::Release);
        }
        if let Some(first) = run {
            bytes += self.copy((first * PAGE).max(from), ((last + 1) * PAGE).min(to));
        }
        if now != 0 {
            if full && self.uncopied.load(Ordering::Acquire) == 0 {
                drop(copied);
                self.synced_at.fetch_max(now, Ordering::Release);
            } else {
                copied.advance_synced(full_lo, full_hi, now);
                drop(copied);
            }
        } else {
            drop(copied);
        }
        if !cool_blocks.is_empty() {
            crate::watch::cool_hot(&cool_blocks);
        }
        bytes
    }

    fn copy_hot(&self, from: usize, to: usize) -> usize {
        if self.card.is_some() {
            return self.copy(from, to);
        }
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        if from >= to {
            return 0;
        }
        let len = to - from;
        if len > PAGE {
            return self.copy(from, to);
        }
        thread_local! {
            static HOT_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        HOT_BUF.with(|cell| {
            #[cfg(feature = "diagnostics")]
            let t_hot = std::time::Instant::now();
            let mut buf = cell.borrow_mut();
            if buf.len() < len {
                buf.resize(PAGE, 0);
            }
            let slice = &mut buf[..len];
            let _ = crate::host::read_into_zero_unmapped(from as *const u8, slice);
            let off = from - start;
            // SAFETY: inside the write-back shared buffer mapping (`from..to` is within `start..end`).
            let held = unsafe { std::slice::from_raw_parts_mut(self.data.add(off), len) };
            let Some(first) = first_difference(slice, held) else {
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(
                    crate::copystat::C::CopyHotUs,
                    t_hot.elapsed().as_micros() as u64,
                );
                return 0;
            };
            let last = first + last_difference(&slice[first..], &held[first..]).unwrap_or(0);
            let n = last + 1 - first;
            held[first..=last].copy_from_slice(&slice[first..=last]);
            self.changed(off + first, off + first + n);
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::add(
                    crate::copystat::C::CopyHotUs,
                    t_hot.elapsed().as_micros() as u64,
                );
                crate::copystat::add(crate::copystat::C::CopyHotBytes, n as u64);
            }
            n
        })
    }

    fn copy(&self, from: usize, to: usize) -> usize {
        self.copy_inner(from, to, true)
    }

    fn copy_inner(&self, from: usize, to: usize, clear_hot: bool) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        if from >= to {
            return 0;
        }
        let (off_from, off_to) = (from - start, to - start);
        if let Some(card) = &self.card {
            if clear_hot {
                card.clear_hot_hashes(off_from, off_to);
            }
            if card.has_old.load(Ordering::Acquire) {
                let mut old = card.old.lock().unwrap_or_else(|e| e.into_inner());
                for p in self.pieces(off_from, off_to) {
                    if let Some(buf) = old.get_mut(&p) {
                        let (p_lo, p_hi) = self.piece(p);
                        let a = off_from.max(p_lo);
                        let b = off_to.min(p_hi);
                        let _ = read_into_zero_unmapped_skip_absent(
                            start + a,
                            &mut buf[a - p_lo..b - p_lo],
                        );
                    }
                }
            }
            self.changed(off_from, off_to);
            self.card_changed(off_from, off_to);
            return to - from;
        }
        // SAFETY: `off_from..off_to` lies inside the writable host shadow mapping.
        let at = unsafe { self.data.add(off_from) };
        copy_mapped_skip_absent(at, from, to - from);
        self.changed(off_from, off_to);
        self.card_changed(off_from, off_to);
        to - from
    }

    pub(crate) fn carded(&self) -> bool {
        self.card.is_some()
    }

    fn card_changed(&self, from: usize, to: usize) {
        let Some(card) = &self.card else {
            return;
        };
        if from >= to {
            return;
        }
        let mut dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
        match dirty.last_mut() {
            Some(last) if from <= last.1 && to >= last.0 => {
                *last = (last.0.min(from), last.1.max(to))
            }
            _ => dirty.push((from, to)),
        }
    }

    fn card_masked_push(&self, offset: usize, data: Vec<u8>, mask: Vec<u8>) {
        let Some(card) = &self.card else {
            return;
        };
        if data.is_empty() {
            return;
        }
        let mut masked = card.masked.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = masked
            .iter_mut()
            .rev()
            .take(8)
            .find(|m| m.offset == offset && m.data.len() == data.len())
            && let Some(prev_mask) = Arc::get_mut(&mut prev.mask)
        {
            for i in 0..data.len() {
                if mask[i] != 0 {
                    prev.data[i] = data[i];
                    prev_mask[i] = 0xFF;
                }
            }
            return;
        }
        masked.push(CardMasked {
            offset,
            data,
            mask: Arc::new(mask),
        });
    }

    pub(crate) fn card_sides(&self) -> Option<(Option<ID3D12Resource>, u64)> {
        let card = self.card.as_ref()?;
        let before = card
            .before
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Some((before, card.size))
    }

    fn copy_card_spans(&self, card: &Card, spans: &[(usize, usize)], clear_hot: bool) -> usize {
        if spans.is_empty() {
            return 0;
        }
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let mut rel: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        let mut total = 0usize;
        for &(a, b) in spans {
            let (from, to) = (a.max(start), b.min(end));
            if from < to {
                rel.push((from - start, to - start));
                total += to - from;
            }
        }
        if rel.is_empty() {
            return 0;
        }
        if clear_hot {
            for &(off_from, off_to) in &rel {
                card.clear_hot_hashes(off_from, off_to);
            }
        }
        if card.has_old.load(Ordering::Acquire) {
            let mut old = card.old.lock().unwrap_or_else(|e| e.into_inner());
            for &(off_from, off_to) in &rel {
                for p in self.pieces(off_from, off_to) {
                    if let Some(buf) = old.get_mut(&p) {
                        let (p_lo, p_hi) = self.piece(p);
                        let a = off_from.max(p_lo);
                        let b = off_to.min(p_hi);
                        let _ = read_into_zero_unmapped_skip_absent(
                            start + a,
                            &mut buf[a - p_lo..b - p_lo],
                        );
                    }
                }
            }
        }
        for &(off_from, off_to) in &rel {
            self.changed(off_from, off_to);
        }
        {
            let mut dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
            for &(off_from, off_to) in &rel {
                match dirty.last_mut() {
                    Some(last) if off_from <= last.1 && off_to >= last.0 => {
                        *last = (last.0.min(off_from), last.1.max(off_to));
                    }
                    _ => dirty.push((off_from, off_to)),
                }
            }
        }
        total
    }

    pub(crate) fn card_prepare_write(&self, address: usize, end: usize) {
        let Some(card) = &self.card else {
            return;
        };
        let (from, to) = self.inside(address, end.saturating_sub(address));
        if from >= to {
            return;
        }
        // Do NOT clear `hot_hashes` here: `card_prepare_write` only prepares `card.old`
        // for pieces that haven't been tracked yet and does not modify title memory.
        // Clearing `hot_hashes` on every UAV declaration forced subsequent hot/unwatched
        // refreshes to re-upload unchanged pages and flood `card.dirty`/`card.sent`.
        // Actual GPU writebacks clear `hot_hashes` for only their merged spans in
        // `card_merge` / `card_merge_relay_batch`.
        let missing: Vec<usize> = {
            let old = card.old.lock().unwrap_or_else(|e| e.into_inner());
            self.pieces(from, to)
                .filter(|p| !old.contains_key(p))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let start = self.start as usize;
        let base = start / PAGE;
        let unwatched = self.watched.load(Ordering::Relaxed) == NOT_WATCHED;
        let all_copied = !unwatched && self.uncopied.load(Ordering::Acquire) == 0;
        let copied_spans_by_piece: Vec<Vec<(usize, usize)>> = if unwatched || all_copied {
            Vec::new()
        } else {
            let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            missing
                .iter()
                .map(|&p| {
                    let (p_lo, p_hi) = self.piece(p);
                    let first_page = (start + p_lo) / PAGE;
                    let last_page = (start + p_hi - 1) / PAGE;
                    let mut spans: Vec<(usize, usize)> = Vec::new();
                    for page in first_page..=last_page {
                        if copied.pages.get(page - base).copied().unwrap_or(0) != 0 {
                            let lo = (page * PAGE).max(start + p_lo) - start;
                            let hi = ((page + 1) * PAGE).min(start + p_hi) - start;
                            if lo < hi {
                                match spans.last_mut() {
                                    Some(last) if lo == last.1 => last.1 = hi,
                                    _ => spans.push((lo, hi)),
                                }
                            }
                        }
                    }
                    spans
                })
                .collect()
        };
        let mut built: Vec<(usize, Vec<u8>)> = Vec::with_capacity(missing.len());
        let mut new_dirty: Vec<(usize, usize)> = Vec::new();
        for (i, &p) in missing.iter().enumerate() {
            let (p_lo, p_hi) = self.piece(p);
            let mut buf = vec![0u8; p_hi - p_lo];
            if unwatched || all_copied {
                if crate::host::read_into((start + p_lo) as *const u8, &mut buf) {
                    new_dirty.push((p_lo, p_hi));
                }
            } else {
                for &(lo, hi) in &copied_spans_by_piece[i] {
                    if crate::host::read_into(
                        (start + lo) as *const u8,
                        &mut buf[lo - p_lo..hi - p_lo],
                    ) {
                        new_dirty.push((lo, hi));
                    }
                }
            }
            built.push((p, buf));
        }
        {
            let mut old = card.old.lock().unwrap_or_else(|e| e.into_inner());
            for (p, buf) in built {
                old.entry(p).or_insert(buf);
            }
            card.has_old.store(true, Ordering::Release);
        }
        if !new_dirty.is_empty() {
            let mut dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
            for (d_from, d_to) in new_dirty {
                match dirty.last_mut() {
                    Some(last) if d_from <= last.1 && d_to >= last.0 => {
                        *last = (last.0.min(d_from), last.1.max(d_to));
                    }
                    _ => dirty.push((d_from, d_to)),
                }
            }
        }
    }

    pub(crate) fn card_has_old_piece(&self, at: usize, len: usize) -> bool {
        let Some(card) = &self.card else {
            return false;
        };
        if len == 0 || at.saturating_add(len) > self.len as usize {
            return false;
        }
        if !card.has_old.load(Ordering::Acquire) {
            return false;
        }
        let old = card.old.lock().unwrap_or_else(|e| e.into_inner());
        self.pieces(at, at + len).all(|p| old.contains_key(&p))
    }

    pub(crate) fn card_piece_is_superseded(&self, lo: usize, hi: usize, after: u64) -> bool {
        let Some(card) = &self.card else {
            return false;
        };
        if lo >= hi {
            return false;
        }
        let dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
        let sent = card.sent.lock().unwrap_or_else(|e| e.into_inner());
        card_range_is_superseded(&dirty, &sent, lo, hi, after)
    }

    pub(crate) fn card_fill_upload(&self, a: usize, len: usize, dst: *mut u8) {
        if len == 0 {
            return;
        }
        let start = self.start as usize;
        let Some(card) = &self.card else {
            return;
        };
        if !card.has_old.load(Ordering::Acquire) {
            copy_mapped_skip_absent(dst, start + a, len);
            return;
        }
        let old = card.old.lock().unwrap_or_else(|e| e.into_inner());
        if old.is_empty() {
            drop(old);
            copy_mapped_skip_absent(dst, start + a, len);
            return;
        }
        let mut cursor = a;
        let end = a + len;
        for p in self.pieces(a, end) {
            let (p_lo, p_hi) = self.piece(p);
            let seg_lo = cursor.max(p_lo);
            let seg_hi = end.min(p_hi);
            if seg_lo >= seg_hi {
                continue;
            }
            let seg_len = seg_hi - seg_lo;
            // SAFETY: `dst` has at least `len` bytes and `seg_lo - a + seg_len <= len`.
            let seg_dst = unsafe { dst.add(seg_lo - a) };
            if let Some(buf) = old.get(&p) {
                // SAFETY: `buf` covers `[p_lo, p_hi)` and `seg_lo..seg_hi` lies inside it.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buf.as_ptr().add(seg_lo - p_lo),
                        seg_dst,
                        seg_len,
                    );
                }
            } else {
                copy_mapped_skip_absent(seg_dst, start + seg_lo, seg_len);
            }
            cursor = seg_hi;
        }
    }

    pub(crate) fn card_make_before(
        &self,
        make: impl FnOnce(u64) -> Option<ID3D12Resource>,
    ) -> bool {
        let Some(card) = &self.card else {
            return false;
        };
        let mut before = card.before.lock().unwrap_or_else(|e| e.into_inner());
        if before.is_some() {
            return false;
        }
        *before = make(card.size);
        if before.is_some() {
            CARD_BYTES[1].fetch_add(card_hash_bytes(card.size), Ordering::Relaxed);
        }
        before.is_some()
    }

    pub(crate) fn card_span(&self, address: usize, end: usize) -> Option<CardSpan> {
        let (from, to) = self.inside(address, end.saturating_sub(address));
        (from < to).then(|| card_span_of(self.start as usize, from, to))
    }

    pub(crate) fn card_run(&self, from: usize, to: usize) -> CardSpan {
        let to = to.min(self.len as usize);
        card_span_of(self.start as usize, from.min(to.saturating_sub(1)), to)
    }

    pub(crate) fn card_piece(&self, span: &CardSpan, i: u32) -> (usize, usize) {
        card_piece_of(self.start as usize, self.len as usize, span, i)
    }

    pub(crate) fn walls_merged(&self, fetched: usize, written: usize) {
        if !crate::host::walls_trace() {
            return;
        }
        static SEEN: Mutex<(u64, BTreeMap<u64, (u64, u32, u64, u64)>)> =
            Mutex::new((0, BTreeMap::new()));
        let frame = crate::frame::current();
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.0 != frame {
            let buffers: Vec<String> = seen
                .1
                .iter()
                .map(|(at, v)| {
                    format!(
                        "{at:#x}+{:#x}: {} pieces, {} fetched, {} written",
                        v.0, v.1, v.2, v.3
                    )
                })
                .take(32)
                .collect();
            if !buffers.is_empty() {
                tracing::info!(
                    frame = seen.0,
                    count = seen.1.len(),
                    buffers = ?buffers,
                    "walls trace: shader-written card buffers merged into title memory in this frame"
                );
            }
            seen.1.clear();
            seen.0 = frame;
        }
        let entry = seen.1.entry(self.start).or_insert((self.len, 0, 0, 0));
        entry.1 += 1;
        entry.2 += fetched as u64;
        entry.3 += written as u64;
    }

    pub(crate) fn card_merge(&self, at: usize, gpu: &[u8], title: &mut Vec<u8>) -> usize {
        let start = self.start as usize;
        let Some(card) = &self.card else {
            return 0;
        };
        if gpu.is_empty() || at + gpu.len() > self.len as usize {
            return 0;
        }
        let p = self.pieces(at, at + gpu.len()).start;
        let (p_lo, _) = self.piece(p);
        let (changed_at, changed_n, written) = {
            let mut old_map = card.old.lock().unwrap_or_else(|e| e.into_inner());
            let Some(piece_buf) = old_map.get_mut(&p) else {
                return 0;
            };
            let old = &mut piece_buf[at - p_lo..at - p_lo + gpu.len()];
            let Some(first) = first_difference(gpu, old) else {
                return 0;
            };
            let last = last_difference(gpu, old).unwrap_or(first);
            let (at, n) = (at + first, last + 1 - first);
            let (gpu, old) = (&gpu[first..=last], &mut old[first..=last]);
            title.clear();
            title.resize(n, 0);
            let mut written = 0;
            if crate::host::read_into((start + at) as *const u8, title) {
                let as_read = merge_race_on().then(|| title.clone());
                let mut differs = 0u8;
                for ((t, g), o) in title.iter_mut().zip(gpu).zip(old.iter_mut()) {
                    let wrote = if *g != *o { 0xffu8 } else { 0 };
                    let merged = (*t & !wrote) | (*g & wrote);
                    differs |= merged ^ *t;
                    *t = merged;
                    *o = merged;
                }
                if differs != 0 {
                    crate::watch::writing(start + at, n);
                    merge_race(
                        as_read.as_deref(),
                        (self.start, self.len),
                        start + at,
                        &title[..],
                        "card",
                    );
                    crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
                    written = n;
                }
            } else {
                old.copy_from_slice(gpu);
            }
            (at, n, written)
        };
        card.clear_hot_hashes(changed_at, changed_at + changed_n);
        self.card_changed(changed_at, changed_at + changed_n);
        if written > 0 {
            self.scanned.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        written
    }

    #[allow(dead_code)]
    pub(crate) fn card_merge_relay(
        &self,
        at: usize,
        gpu: &[u8],
        fetched_after: u64,
        title: &mut Vec<u8>,
    ) -> usize {
        let start = self.start as usize;
        let Some(card) = &self.card else {
            return 0;
        };
        if gpu.is_empty() || at + gpu.len() > self.len as usize {
            return 0;
        }
        let _side = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let p = self.pieces(at, at + gpu.len()).start;
        let (p_lo, _) = self.piece(p);
        let mut old_map = card.old.lock().unwrap_or_else(|e| e.into_inner());
        let Some(piece_buf) = old_map.get_mut(&p) else {
            return 0;
        };
        let held = &mut piece_buf[at - p_lo..at - p_lo + gpu.len()];
        let Some(first) = first_difference(gpu, held) else {
            return 0;
        };
        let last = last_difference(gpu, held).unwrap_or(first);
        let (at, n) = (at + first, last + 1 - first);
        let (gpu, held) = (&gpu[first..=last], &mut held[first..=last]);
        let skip: Vec<(usize, usize)> = {
            let dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
            let sent = card.sent.lock().unwrap_or_else(|e| e.into_inner());
            dirty
                .iter()
                .copied()
                .chain(
                    sent.iter()
                        .filter(|s| s.2 > fetched_after)
                        .map(|s| (s.0, s.1)),
                )
                .filter(|&(a, b)| a < at + n && b > at)
                .collect()
        };
        title.clear();
        title.resize(n, 0);
        let mapped = crate::host::read_into((start + at) as *const u8, title);
        let as_read = (mapped && merge_race_on()).then(|| title.clone());
        let mut changed = false;
        for (a, b) in outside(at, at + n, skip) {
            let (a, b) = (a - at, b - at);
            changed |= merge_piece(&gpu[a..b], &mut held[a..b], &mut title[a..b]);
        }
        if !mapped || !changed {
            return 0;
        }
        card.clear_hot_hashes(at, at + n);
        crate::watch::writing_gpu_back(start + at, n);
        merge_race(
            as_read.as_deref(),
            (self.start, self.len),
            start + at,
            &title[..],
            "card relay",
        );
        crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
        n
    }

    pub(crate) fn card_merge_relay_batch(
        &self,
        pieces: &[(usize, &[u8], u64)],
        parallel: bool,
    ) -> usize {
        let start = self.start as usize;
        let Some(card) = &self.card else {
            return 0;
        };
        if pieces.is_empty() {
            return 0;
        }
        let _side = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut old_map = card.old.lock().unwrap_or_else(|e| e.into_inner());
        let skip = Arc::new(RelaySkipSnapshot::capture(card, pieces));

        let mut piece_index: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        let mut works: Vec<RelayPieceWork> = Vec::new();
        let mut total_stats = RelayMergeStats::default();
        for &(at, gpu, fetched_after) in pieces {
            if gpu.is_empty() || at + gpu.len() > self.len as usize {
                continue;
            }
            let p = self.pieces(at, at + gpu.len()).start;
            let (p_lo, _) = self.piece(p);
            if let Some(&idx) = piece_index.get(&p) {
                let w = &mut works[idx];
                if at >= p_lo && at - p_lo + gpu.len() <= w.old_len {
                    w.items.push((at, gpu.as_ptr() as usize, gpu.len(), fetched_after));
                } else {
                    total_stats.add(RELAY_OUT_MISSING_OLD, gpu.len() as u64);
                }
            } else if let Some(piece_buf) = old_map.get_mut(&p)
                && at >= p_lo
                && at - p_lo + gpu.len() <= piece_buf.len()
            {
                let idx = works.len();
                piece_index.insert(p, idx);
                works.push(RelayPieceWork {
                    p_lo,
                    old_ptr: piece_buf.as_mut_ptr() as usize,
                    old_len: piece_buf.len(),
                    items: vec![(at, gpu.as_ptr() as usize, gpu.len(), fetched_after)],
                });
            } else {
                total_stats.add(RELAY_OUT_MISSING_OLD, gpu.len() as u64);
            }
        }
        if works.is_empty() {
            record_relay_merge_stats(&total_stats);
            return 0;
        }

        let record_race = merge_race_on();
        let raw_buckets: Vec<RelayMergeBucket> = if parallel && works.len() >= 16 {
            let worker_count = (works.len() / 8).clamp(2, 6);
            let chunk_size = works.len().div_ceil(worker_count);
            let mut results: Vec<Arc<Mutex<Option<RelayMergeBucket>>>> =
                Vec::with_capacity(worker_count);
            let mut jobs: Vec<Box<dyn FnOnce() + Send>> = Vec::with_capacity(worker_count);
            let mut iter = works.into_iter();
            loop {
                let chunk: Vec<RelayPieceWork> = iter.by_ref().take(chunk_size).collect();
                if chunk.is_empty() {
                    break;
                }
                let skip = skip.clone();
                let slot = Arc::new(Mutex::new(None));
                results.push(slot.clone());
                jobs.push(Box::new(move || {
                    let out = merge_relay_chunk(start, chunk, &skip, record_race);
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
                }));
            }
            wide_relay_run(jobs);
            results
                .into_iter()
                .filter_map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).take())
                .collect()
        } else {
            vec![merge_relay_chunk(start, works, &skip, record_race)]
        };

        let mut buckets: Vec<RelayMergeBucket> = Vec::with_capacity(raw_buckets.len());
        for b in raw_buckets {
            total_stats.merge_from(&b.stats);
            if !b.writes.is_empty() {
                buckets.push(b);
            }
        }
        record_relay_merge_stats(&total_stats);

        if buckets.is_empty() {
            return 0;
        }
        self.scanned.lock().unwrap_or_else(|e| e.into_inner()).clear();

        let total_writes: usize = buckets.iter().map(|b| b.writes.len()).sum();
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(total_writes);
        let mut total_written = 0usize;
        let mut has_hash_updates = false;
        let mut has_hash_clears = false;
        for b in &buckets {
            has_hash_updates |= !b.hash_updates.is_empty();
            has_hash_clears |= !b.hash_clears.is_empty();
            for w in &b.writes {
                spans.push((w.at, w.at + w.len));
                total_written += w.len;
            }
        }

        if has_hash_updates || (has_hash_clears && card.has_hot_hashes.load(Ordering::Acquire)) {
            let mut guard = card.hot_hashes.lock().unwrap_or_else(|e| e.into_inner());
            if has_hash_updates {
                let slots_needed = (self.len as usize).div_ceil(HOT_HASH_BLOCK);
                if guard.slots.len() < slots_needed {
                    guard.slots.resize(slots_needed, 0);
                }
            }
            if has_hash_clears && !guard.slots.is_empty() {
                let mut clear_spans: Vec<(usize, usize)> = Vec::new();
                for b in &buckets {
                    clear_spans.extend_from_slice(&b.hash_clears);
                }
                for (a, b) in coalesce_hash_spans(&clear_spans) {
                    let lo = a / HOT_HASH_BLOCK;
                    let hi = b.div_ceil(HOT_HASH_BLOCK).min(guard.slots.len());
                    if lo < hi {
                        guard.slots[lo..hi].fill(0);
                    }
                }
            }
            if has_hash_updates {
                for b in &buckets {
                    for &(slot, hash) in &b.hash_updates {
                        if let Some(dst) = guard.slots.get_mut(slot) {
                            *dst = hash;
                        }
                    }
                }
                if !guard.quiet.is_empty() {
                    let frame = crate::frame::current();
                    let base = start / PAGE;
                    for &(a, b) in &spans {
                        let q_lo = ((start + a) / PAGE)
                            .saturating_sub(base)
                            .min(guard.quiet.len());
                        let q_hi = (start + b)
                            .div_ceil(PAGE)
                            .saturating_sub(base)
                            .min(guard.quiet.len());
                        for q in &mut guard.quiet[q_lo..q_hi] {
                            q.mark_changed(frame);
                        }
                    }
                }
                card.has_hot_hashes.store(true, Ordering::Release);
            }
        }
        for (a, b) in coalesce_watch_spans(start, &spans) {
            crate::watch::writing_gpu_back(start + a, b - a);
        }

        let copy_span = (self.start, self.len);
        if parallel && buckets.len() >= 2 && total_written >= (2 << 20) {
            let jobs: Vec<Box<dyn FnOnce() + Send>> = buckets
                .into_iter()
                .map(|bucket| {
                    Box::new(move || {
                        for w in &bucket.writes {
                            // SAFETY: `Arena` offsets lie inside `bucket.arena`; `Gpu` pointers
                            // borrow the readback slices passed into `card_merge_relay_batch`,
                            // which joins all worker threads before returning.
                            let bytes = unsafe { w.bytes(&bucket.arena) };
                            merge_race(
                                w.as_read.as_deref(),
                                copy_span,
                                start + w.at,
                                bytes,
                                "card relay",
                            );
                            crate::host::write_mapped(
                                (start + w.at) as *mut u8,
                                bytes.as_ptr(),
                                w.len,
                            );
                        }
                    }) as Box<dyn FnOnce() + Send>
                })
                .collect();
            wide_relay_run(jobs);
        } else {
            for bucket in &buckets {
                for w in &bucket.writes {
                    // SAFETY: as above.
                    let bytes = unsafe { w.bytes(&bucket.arena) };
                    merge_race(
                        w.as_read.as_deref(),
                        copy_span,
                        start + w.at,
                        bytes,
                        "card relay",
                    );
                    crate::host::write_mapped((start + w.at) as *mut u8, bytes.as_ptr(), w.len);
                }
            }
        }
        total_written
    }
}

struct RelaySkipSnapshot {
    ranges: RelaySkipRanges,
    masked: Vec<(usize, Arc<Vec<u8>>, u64)>,
}

enum RelaySkipRanges {
    Empty,
    Unified(Vec<(usize, usize)>),
    Mixed {
        dirty: Vec<(usize, usize)>,
        sent: Vec<(usize, usize, u64)>,
    },
}

impl RelaySkipSnapshot {
    fn capture(card: &Card, pieces: &[(usize, &[u8], u64)]) -> Self {
        let first_after = pieces[0].2;
        let uniform = pieces.iter().all(|p| p.2 == first_after);
        let min_after = pieces.iter().map(|p| p.2).min().unwrap_or(0);
        let min_at = pieces.iter().map(|p| p.0).min().unwrap_or(0);
        let max_end = pieces.iter().map(|p| p.0 + p.1.len()).max().unwrap_or(0);
        let dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
        let sent = card.sent.lock().unwrap_or_else(|e| e.into_inner());
        let ranges = if uniform {
            let mut all: Vec<(usize, usize)> = dirty
                .iter()
                .copied()
                .chain(
                    sent.iter()
                        .filter(|s| s.2 > first_after)
                        .map(|s| (s.0, s.1)),
                )
                .collect();
            if all.is_empty() {
                RelaySkipRanges::Empty
            } else {
                all = joined(all);
                RelaySkipRanges::Unified(all)
            }
        } else if dirty.is_empty() && sent.is_empty() {
            RelaySkipRanges::Empty
        } else {
            let dirty_joined = if dirty.is_empty() {
                Vec::new()
            } else {
                joined(dirty.clone())
            };
            RelaySkipRanges::Mixed {
                dirty: dirty_joined,
                sent: sent.clone(),
            }
        };
        drop(sent);
        drop(dirty);
        let mut masked = Vec::new();
        {
            let m = card.masked.lock().unwrap_or_else(|e| e.into_inner());
            for item in m.iter() {
                if item.offset < max_end && item.offset + item.mask.len() > min_at {
                    masked.push((item.offset, item.mask.clone(), u64::MAX));
                }
            }
        }
        {
            let sm = card.sent_masked.lock().unwrap_or_else(|e| e.into_inner());
            for (off, mask, seq) in sm.iter() {
                if *seq > min_after && *off < max_end && *off + mask.len() > min_at {
                    masked.push((*off, mask.clone(), *seq));
                }
            }
        }
        Self { ranges, masked }
    }

    fn active_ranges(&self, at: usize, end: usize, fetched_after: u64, out: &mut Vec<(usize, usize)>) {
        out.clear();
        match &self.ranges {
            RelaySkipRanges::Empty => out.push((at, end)),
            RelaySkipRanges::Unified(ranges) => {
                let mut cur = at;
                let idx = ranges.partition_point(|&(_, b)| b <= at);
                for &(a, b) in &ranges[idx..] {
                    if a >= end {
                        break;
                    }
                    if a > cur {
                        out.push((cur, a));
                    }
                    cur = cur.max(b);
                    if cur >= end {
                        break;
                    }
                }
                if cur < end {
                    out.push((cur, end));
                }
            }
            RelaySkipRanges::Mixed { dirty, sent } => {
                let skip: Vec<(usize, usize)> = dirty
                    .iter()
                    .copied()
                    .chain(
                        sent.iter()
                            .filter(|s| s.2 > fetched_after)
                            .map(|s| (s.0, s.1)),
                    )
                    .filter(|&(a, b)| a < end && b > at)
                    .collect();
                if skip.is_empty() {
                    out.push((at, end));
                } else {
                    out.extend(outside(at, end, skip));
                }
            }
        }
        if self.masked.is_empty() || out.is_empty() {
            return;
        }
        let mut next: Vec<(usize, usize)> = Vec::new();
        for (m_off, m_mask, m_seq) in &self.masked {
            if *m_seq <= fetched_after {
                continue;
            }
            let m_end = *m_off + m_mask.len();
            if *m_off >= end || m_end <= at {
                continue;
            }
            next.clear();
            let push_merged = |v: &mut Vec<(usize, usize)>, s: usize, e: usize| {
                if s >= e {
                    return;
                }
                match v.last_mut() {
                    Some(last) if last.1 == s => last.1 = e,
                    _ => v.push((s, e)),
                }
            };
            for &(a, b) in out.iter() {
                if b <= *m_off || a >= m_end {
                    push_merged(&mut next, a, b);
                    continue;
                }
                if a < *m_off {
                    push_merged(&mut next, a, *m_off);
                }
                let lo = a.max(*m_off);
                let hi = b.min(m_end);
                let mut cur = lo;
                while cur < hi {
                    while cur < hi && m_mask[cur - *m_off] != 0 {
                        cur += 1;
                    }
                    if cur >= hi {
                        break;
                    }
                    let run_start = cur;
                    while cur < hi && m_mask[cur - *m_off] == 0 {
                        cur += 1;
                    }
                    push_merged(&mut next, run_start, cur);
                }
                if b > m_end {
                    push_merged(&mut next, m_end, b);
                }
            }
            std::mem::swap(out, &mut next);
            if out.is_empty() {
                return;
            }
        }
    }
}

struct RelayPieceWork {
    p_lo: usize,
    old_ptr: usize,
    old_len: usize,
    items: Vec<(usize, usize, usize, u64)>,
}

enum RelayWriteSrc {
    Arena(usize),
    Gpu(usize),
}

struct RelayPendingWrite {
    at: usize,
    len: usize,
    src: RelayWriteSrc,
    as_read: Option<Vec<u8>>,
}

impl RelayPendingWrite {
    unsafe fn bytes<'a>(&self, arena: &'a [u8]) -> &'a [u8] {
        match self.src {
            RelayWriteSrc::Arena(off) => &arena[off..off + self.len],
            RelayWriteSrc::Gpu(ptr) => unsafe {
                std::slice::from_raw_parts(ptr as *const u8, self.len)
            },
        }
    }
}

const RELAY_OUT_WRITTEN: usize = 0;
const RELAY_OUT_MISSING_OLD: usize = 1;
const RELAY_OUT_EQUAL_OLD: usize = 2;
const RELAY_OUT_SUPERSEDED: usize = 3;
const RELAY_OUT_UNMAPPED: usize = 4;
const RELAY_OUT_ALREADY_EQUAL: usize = 5;
const RELAY_OUT_KINDS: usize = 6;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct RelayMergeStats {
    pieces: [u64; RELAY_OUT_KINDS],
    bytes: [u64; RELAY_OUT_KINDS],
}

impl RelayMergeStats {
    #[inline]
    fn add(&mut self, kind: usize, len: u64) {
        self.pieces[kind] += 1;
        self.bytes[kind] += len;
    }

    #[inline]
    fn merge_from(&mut self, other: &Self) {
        for i in 0..RELAY_OUT_KINDS {
            self.pieces[i] += other.pieces[i];
            self.bytes[i] += other.bytes[i];
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.pieces.iter().all(|&p| p == 0)
    }
}

static RELAY_MERGE_PIECES: [AtomicU64; RELAY_OUT_KINDS] =
    [const { AtomicU64::new(0) }; RELAY_OUT_KINDS];
static RELAY_MERGE_BYTES: [AtomicU64; RELAY_OUT_KINDS] =
    [const { AtomicU64::new(0) }; RELAY_OUT_KINDS];
static RELAY_MERGE_LOG_MS: AtomicU64 = AtomicU64::new(0);

fn relay_merge_now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

fn record_relay_merge_stats(stats: &RelayMergeStats) {
    if stats.is_empty() {
        return;
    }
    for i in 0..RELAY_OUT_KINDS {
        if stats.pieces[i] != 0 {
            RELAY_MERGE_PIECES[i].fetch_add(stats.pieces[i], Ordering::Relaxed);
            RELAY_MERGE_BYTES[i].fetch_add(stats.bytes[i], Ordering::Relaxed);
        }
    }
    let now = relay_merge_now_ms();
    let prev = RELAY_MERGE_LOG_MS.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < 5_000 {
        return;
    }
    if RELAY_MERGE_LOG_MS
        .compare_exchange(prev, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let mut p = [0u64; RELAY_OUT_KINDS];
    let mut b = [0u64; RELAY_OUT_KINDS];
    let mut total_p = 0u64;
    for i in 0..RELAY_OUT_KINDS {
        p[i] = RELAY_MERGE_PIECES[i].swap(0, Ordering::Relaxed);
        b[i] = RELAY_MERGE_BYTES[i].swap(0, Ordering::Relaxed);
        total_p += p[i];
    }
    if total_p == 0 {
        return;
    }
    let mb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    tracing::info!(
        written = p[RELAY_OUT_WRITTEN],
        written_mb = format_args!("{:.2}", mb(b[RELAY_OUT_WRITTEN])),
        missing_old = p[RELAY_OUT_MISSING_OLD],
        missing_old_mb = format_args!("{:.2}", mb(b[RELAY_OUT_MISSING_OLD])),
        equal_old = p[RELAY_OUT_EQUAL_OLD],
        equal_old_mb = format_args!("{:.2}", mb(b[RELAY_OUT_EQUAL_OLD])),
        superseded = p[RELAY_OUT_SUPERSEDED],
        superseded_mb = format_args!("{:.2}", mb(b[RELAY_OUT_SUPERSEDED])),
        unmapped = p[RELAY_OUT_UNMAPPED],
        unmapped_mb = format_args!("{:.2}", mb(b[RELAY_OUT_UNMAPPED])),
        already_equal = p[RELAY_OUT_ALREADY_EQUAL],
        already_equal_mb = format_args!("{:.2}", mb(b[RELAY_OUT_ALREADY_EQUAL])),
        "card relay merge outcomes (5s)"
    );
}

#[allow(dead_code)]
fn card_range_is_superseded(
    dirty: &[(usize, usize)],
    sent: &[(usize, usize, u64)],
    lo: usize,
    hi: usize,
    after: u64,
) -> bool {
    if lo >= hi {
        return false;
    }
    let mut overlaps: Vec<(usize, usize)> = Vec::new();
    for &(a, b) in dirty {
        if a < hi && b > lo {
            if a <= lo && b >= hi {
                return true;
            }
            overlaps.push((a, b));
        }
    }
    for &(a, b, seq) in sent {
        if seq > after && a < hi && b > lo {
            if a <= lo && b >= hi {
                return true;
            }
            overlaps.push((a, b));
        }
    }
    !overlaps.is_empty() && outside(lo, hi, overlaps).is_empty()
}

struct RelayMergeBucket {
    arena: Vec<u8>,
    writes: Vec<RelayPendingWrite>,
    hash_updates: Vec<(usize, u128)>,
    hash_clears: Vec<(usize, usize)>,
    stats: RelayMergeStats,
}

fn merge_relay_chunk(
    start: usize,
    works: Vec<RelayPieceWork>,
    skip: &RelaySkipSnapshot,
    record_race: bool,
) -> RelayMergeBucket {
    let mut arena = Vec::new();
    let mut writes = Vec::new();
    let mut hash_updates = Vec::new();
    let mut hash_clears = Vec::new();
    let mut scratch = Vec::new();
    let mut active = Vec::new();
    let mut stats = RelayMergeStats::default();
    for work in works {
        // SAFETY: `old_map` is locked exclusively by `card_merge_relay_batch` until
        // all workers finish, and each `RelayPieceWork` owns a distinct piece key `p`
        // whose `Vec<u8>` allocation is disjoint from all other workers' buffers.
        let piece_buf =
            unsafe { std::slice::from_raw_parts_mut(work.old_ptr as *mut u8, work.old_len) };
        for (at, gpu_ptr, gpu_len, fetched_after) in work.items {
            // SAFETY: `gpu_ptr..gpu_ptr+gpu_len` is a valid borrowed read-only slice for
            // the duration of `card_merge_relay_batch`, which joins before returning.
            let gpu = unsafe { std::slice::from_raw_parts(gpu_ptr as *const u8, gpu_len) };
            let held = &mut piece_buf[at - work.p_lo..at - work.p_lo + gpu_len];
            let Some(first) = first_difference(gpu, held) else {
                stats.add(RELAY_OUT_EQUAL_OLD, gpu_len as u64);
                continue;
            };
            let last = last_difference(gpu, held).unwrap_or(first);
            let (diff_at, n) = (at + first, last + 1 - first);
            skip.active_ranges(diff_at, diff_at + n, fetched_after, &mut active);
            if active.is_empty() {
                stats.add(RELAY_OUT_SUPERSEDED, gpu_len as u64);
                continue;
            }
            let (gpu, held) = (&gpu[first..=last], &mut held[first..=last]);
            if scratch.len() < n {
                scratch.resize(n, 0);
            }
            let scratch_slice = &mut scratch[..n];
            let mapped = crate::host::read_into((start + diff_at) as *const u8, scratch_slice);
            if !mapped {
                for &(a, b) in &active {
                    let (a, b) = (a - diff_at, b - diff_at);
                    held[a..b].copy_from_slice(&gpu[a..b]);
                }
                stats.add(RELAY_OUT_UNMAPPED, gpu_len as u64);
                continue;
            }
            let as_read = record_race.then(|| scratch_slice.to_vec());
            let full_active = active.len() == 1 && active[0] == (diff_at, diff_at + n);
            if full_active && scratch_slice[..] == held[..] {
                held.copy_from_slice(gpu);
                stats.add(RELAY_OUT_WRITTEN, n as u64);
                // Point to `held` (cached system RAM inside `old_map`, locked for the
                // entire `card_merge_relay_batch` and disjoint across items) instead of
                // re-reading `gpu` from the `D3D12_HEAP_TYPE_READBACK` mapping.
                writes.push(RelayPendingWrite {
                    at: diff_at,
                    len: n,
                    src: RelayWriteSrc::Gpu(held.as_ptr() as usize),
                    as_read,
                });
                let diff_end = diff_at + n;
                let first_full = diff_at.next_multiple_of(HOT_HASH_BLOCK);
                let last_full = (diff_end / HOT_HASH_BLOCK) * HOT_HASH_BLOCK;
                if diff_at < first_full {
                    hash_clears.push((diff_at, first_full.min(diff_end)));
                }
                if first_full < last_full {
                    let mut b_at = first_full;
                    while b_at < last_full {
                        let h =
                            hot_page_hash(&held[b_at - diff_at..b_at - diff_at + HOT_HASH_BLOCK])
                                | 1;
                        hash_updates.push((b_at / HOT_HASH_BLOCK, h));
                        b_at += HOT_HASH_BLOCK;
                    }
                }
                if last_full < diff_end {
                    hash_clears.push((last_full.max(diff_at), diff_end));
                }
                continue;
            }
            let mut changed = false;
            for &(a, b) in &active {
                let (a, b) = (a - diff_at, b - diff_at);
                changed |= merge_piece(&gpu[a..b], &mut held[a..b], &mut scratch_slice[a..b]);
            }
            if !changed {
                stats.add(RELAY_OUT_ALREADY_EQUAL, gpu_len as u64);
                continue;
            }
            stats.add(RELAY_OUT_WRITTEN, n as u64);
            let arena_off = arena.len();
            arena.extend_from_slice(scratch_slice);
            writes.push(RelayPendingWrite {
                at: diff_at,
                len: n,
                src: RelayWriteSrc::Arena(arena_off),
                as_read,
            });
            hash_clears.push((diff_at, diff_at + n));
        }
    }
    RelayMergeBucket {
        arena,
        writes,
        hash_updates,
        hash_clears,
        stats,
    }
}

fn coalesce_watch_spans(start: usize, spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted: Vec<(usize, usize)> = spans.iter().copied().filter(|&(a, b)| a < b).collect();
    if sorted.is_empty() {
        return sorted;
    }
    sorted.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
    for (a, b) in sorted {
        match out.last_mut() {
            Some(last) if (start + a) / PAGE <= (start + last.1).div_ceil(PAGE) => {
                last.1 = last.1.max(b);
            }
            _ => out.push((a, b)),
        }
    }
    out
}

fn coalesce_hash_spans(spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted: Vec<(usize, usize)> = spans.iter().copied().filter(|&(a, b)| a < b).collect();
    if sorted.is_empty() {
        return sorted;
    }
    sorted.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
    for (a, b) in sorted {
        match out.last_mut() {
            Some(last) if a / HOT_HASH_BLOCK <= last.1.div_ceil(HOT_HASH_BLOCK) => {
                last.1 = last.1.max(b);
            }
            _ => out.push((a, b)),
        }
    }
    out
}

#[cfg(test)]
mod relay_batch_tests {
    use super::{
        Card, HOT_HASH_BLOCK, PAGE, RelayPieceWork, RelaySkipSnapshot, coalesce_hash_spans,
        coalesce_watch_spans, merge_relay_chunk,
    };
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn watch_spans_coalesce_adjacent_pages_without_bridging_untouched_pages() {
        let start = 0x10_0000usize; // page-aligned
        let spans = [
            (0x0010, 0x0ff0),                 // page 0
            (0x2000, 0xfe00),                 // still page 0
            (PAGE + 0x40, PAGE + 0x800),      // page 1 (adjacent to page 0)
            (3 * PAGE + 0x10, 3 * PAGE + 0x20), // page 3 (leaves page 2 untouched!)
            (4 * PAGE, 4 * PAGE + 0x100),     // page 4 (adjacent to page 3)
        ];
        assert_eq!(
            coalesce_watch_spans(start, &spans),
            vec![
                (0x0010, PAGE + 0x800),
                (3 * PAGE + 0x10, 4 * PAGE + 0x100),
            ]
        );
        assert!(coalesce_watch_spans(start, &[(10, 10)]).is_empty());
    }

    #[test]
    fn hash_spans_coalesce_adjacent_4k_blocks_only() {
        let spans = [
            (100, 200),
            (HOT_HASH_BLOCK + 10, HOT_HASH_BLOCK + 50),
            (3 * HOT_HASH_BLOCK + 1, 3 * HOT_HASH_BLOCK + 8),
        ];
        assert_eq!(
            coalesce_hash_spans(&spans),
            vec![
                (100, HOT_HASH_BLOCK + 50),
                (3 * HOT_HASH_BLOCK + 1, 3 * HOT_HASH_BLOCK + 8),
            ]
        );
    }

    #[test]
    fn skip_snapshot_unified_and_mixed_cut_dirty_and_newer_sent_ranges() {
        let card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::new(vec![(100, 140)]),
            sent: Mutex::new(vec![(20, 40, 5), (60, 80, 9), (130, 160, 11)]),
            masked: Mutex::default(),
            sent_masked: Mutex::default(),
            hot_hashes: Mutex::default(),
            has_hot_hashes: AtomicBool::new(false),
            size: 1024,
        };
        let buf = [0u8; 256];
        let uniform = RelaySkipSnapshot::capture(&card, &[(0, &buf[..], 7)]);
        let mut active = Vec::new();
        uniform.active_ranges(0, 200, 7, &mut active);
        // (20, 40, seq=5 <= 7) is NOT skipped; (60, 80, seq=9 > 7) and (100, 160) ARE skipped.
        assert_eq!(active, vec![(0, 60), (80, 100), (160, 200)]);

        let mixed = RelaySkipSnapshot::capture(&card, &[(0, &buf[..128], 4), (128, &buf[128..], 10)]);
        mixed.active_ranges(0, 128, 4, &mut active);
        assert_eq!(active, vec![(0, 20), (40, 60), (80, 100)]);
        mixed.active_ranges(128, 200, 10, &mut active);
        assert_eq!(active, vec![(160, 200)]);
    }

    #[test]
    fn diff_runs_or_masked_handles_sparse_and_dense_byte_diffs_with_alignment() {
        use super::{MergeDiff, diff_runs_or_masked};

        // Sparse diff (<= 16 runs) returns exact runs and updates held in place.
        let mut held = vec![0u8; 64];
        let mut title = held.clone();
        title[5] = 0x11;
        title[20..24].fill(0x22);
        match diff_runs_or_masked(64, 256, &title, &mut held, 16) {
            MergeDiff::Runs(runs) => assert_eq!(runs, vec![(5, 1), (20, 4)]),
            _ => panic!("expected Runs for 2 sparse diffs"),
        }
        assert_eq!(held, title);

        // Dense diff (> 16 scattered single-byte runs, with odd `at = 3`) switches to 4-byte aligned Masked.
        let mut held = vec![0xAAu8; 128];
        let mut title = held.clone();
        for k in 0..30 {
            title[2 + k * 3] = (k as u8).wrapping_add(1);
        }
        let expected_held = title.clone();
        match diff_runs_or_masked(3, 256, &title, &mut held, 16) {
            MergeDiff::Masked {
                offset,
                data,
                mask,
                changed_runs,
                changed_bytes,
                ..
            } => {
                assert_eq!(offset % 4, 0);
                assert_eq!(data.len() % 4, 0);
                assert_eq!(data.len(), mask.len());
                assert_eq!(changed_runs, 30);
                assert_eq!(changed_bytes, 30);
                for i in 0..data.len() {
                    let abs = offset + i;
                    let rel = abs.checked_sub(3);
                    let changed = rel.is_some_and(|r| r < 128 && (r >= 2 && (r - 2) % 3 == 0 && (r - 2) / 3 < 30));
                    if changed {
                        assert_eq!(mask[i], 0xFF);
                        assert_eq!(data[i], expected_held[rel.unwrap()]);
                    } else {
                        assert_eq!(mask[i], 0x00);
                    }
                }
            }
            _ => panic!("expected Masked for 30 scattered byte diffs"),
        }
        assert_eq!(held, expected_held);
    }

    #[test]
    fn skip_snapshot_respects_byte_exact_masked_and_sent_masked_entries() {
        use super::CardMasked;
        use std::sync::Arc;

        // Byte mask at offset 16..24: bytes 17, 18, 21 are CPU-changed (0xFF); rest are 0x00.
        let mask_live = Arc::new(vec![0, 0xFF, 0xFF, 0, 0, 0xFF, 0, 0]);
        // Sent mask at offset 32..36 with seq=10: byte 33 is CPU-changed.
        let mask_sent = Arc::new(vec![0, 0xFF, 0, 0]);
        let card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::default(),
            sent: Mutex::default(),
            masked: Mutex::new(vec![CardMasked {
                offset: 16,
                data: vec![0; 8],
                mask: mask_live,
            }]),
            sent_masked: Mutex::new(vec![(32, mask_sent, 10)]),
            hot_hashes: Mutex::default(),
            has_hot_hashes: AtomicBool::new(false),
            size: 1024,
        };
        let buf = [0u8; 64];
        let skip = RelaySkipSnapshot::capture(&card, &[(0, &buf[..], 5)]);
        let mut active = Vec::new();
        // fetched_after=5 < 10: skips 17..19, 21..22, and 33..34, preserving adjacent GPU bytes!
        skip.active_ranges(10, 40, 5, &mut active);
        assert_eq!(active, vec![(10, 17), (19, 21), (22, 33), (34, 40)]);
        // fetched_after=10: skips only live masked (17..19, 21..22), NOT seq=10 sent_masked!
        skip.active_ranges(10, 40, 10, &mut active);
        assert_eq!(active, vec![(10, 17), (19, 21), (22, 40)]);
    }

    #[test]
    fn batched_relay_merge_chunk_merges_multiple_pieces_and_respects_skip() {
        let mut title_mem: Vec<u8> = (0..2 * PAGE).map(|i| (i * 13 + 7) as u8).collect();
        let mut old0 = title_mem[0..PAGE].to_vec();
        let mut old1 = title_mem[PAGE..2 * PAGE].to_vec();
        let mut gpu0 = old0.clone();
        let mut gpu1 = old1.clone();

        // GPU modified bytes in both pieces.
        gpu0[16..24].fill(0xaa);
        gpu0[100..112].fill(0xbb); // covered by skip
        gpu1[32..48].fill(0xcc);
        // Title modified a different byte in piece 0 concurrently.
        title_mem[18] = 0x55;

        let card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::new(vec![(100, 112)]),
            sent: Mutex::default(),
            masked: Mutex::default(),
            sent_masked: Mutex::default(),
            hot_hashes: Mutex::default(),
            has_hot_hashes: AtomicBool::new(false),
            size: (2 * PAGE) as u64,
        };
        let skip = RelaySkipSnapshot::capture(&card, &[(0, &gpu0[..], 1), (PAGE, &gpu1[..], 1)]);
        let works = vec![
            RelayPieceWork {
                p_lo: 0,
                old_ptr: old0.as_mut_ptr() as usize,
                old_len: old0.len(),
                items: vec![(0, gpu0.as_ptr() as usize, gpu0.len(), 1)],
            },
            RelayPieceWork {
                p_lo: PAGE,
                old_ptr: old1.as_mut_ptr() as usize,
                old_len: old1.len(),
                items: vec![(PAGE, gpu1.as_ptr() as usize, gpu1.len(), 1)],
            },
        ];
        let start = title_mem.as_ptr() as usize;
        let bucket = merge_relay_chunk(start, works, &skip, false);
        assert_eq!(bucket.writes.len(), 2);
        assert_eq!(bucket.stats.pieces[super::RELAY_OUT_WRITTEN], 2);
        for w in &bucket.writes {
            // SAFETY: `gpu0`/`gpu1` are alive for the duration of this test.
            let bytes = unsafe { w.bytes(&bucket.arena) };
            title_mem[w.at..w.at + w.len].copy_from_slice(bytes);
        }
        assert!(title_mem[16..24].iter().all(|&b| b == 0xaa));
        assert!(title_mem[100..112].iter().all(|&b| b != 0xbb));
        assert!(title_mem[PAGE + 32..PAGE + 48].iter().all(|&b| b == 0xcc));
        assert_eq!(old0[16..24], [0xaa; 8]);
        assert_ne!(old0[100], 0xbb);
        assert_eq!(old1[32..48], [0xcc; 16]);
    }

    #[test]
    fn merge_piece_fast_paths_match_three_way_semantics() {
        use super::merge_piece;

        // Fast path 1: title == held, now != held -> copies now into both.
        let mut held = vec![1, 2, 3, 4];
        let mut title = held.clone();
        assert!(merge_piece(&[1, 9, 3, 8], &mut held, &mut title));
        assert_eq!(held, vec![1, 9, 3, 8]);
        assert_eq!(title, vec![1, 9, 3, 8]);

        // Fast path 2: title == held == now -> unchanged.
        assert!(!merge_piece(&[1, 9, 3, 8], &mut held, &mut title));

        // Fast path 3: title == now, held != now -> updates held, reports unchanged.
        let mut held2 = vec![1, 2, 3, 4];
        let mut title2 = vec![1, 9, 3, 8];
        assert!(!merge_piece(&[1, 9, 3, 8], &mut held2, &mut title2));
        assert_eq!(held2, vec![1, 9, 3, 8]);
        assert_eq!(title2, vec![1, 9, 3, 8]);

        // Slow path: concurrent CPU edit at index 0, GPU edit at index 2.
        let mut held3 = vec![1, 2, 3, 4];
        let mut title3 = vec![7, 2, 3, 4];
        assert!(merge_piece(&[1, 2, 9, 4], &mut held3, &mut title3));
        assert_eq!(title3, vec![7, 2, 9, 4]);
        assert_eq!(held3, vec![1, 2, 9, 4]);
    }

    #[test]
    fn relay_merge_fast_path_skips_arena_copy_and_emits_hot_hash_updates() {
        use super::{RelayWriteSrc, hot_page_hash};

        let title_mem: Vec<u8> = (0..PAGE).map(|i| (i * 7 + 1) as u8).collect();
        let mut old0 = title_mem.clone();
        let mut gpu0 = old0.clone();
        // GPU overwrites the entire 64 KiB piece; CPU did not touch title_mem.
        for (i, b) in gpu0.iter_mut().enumerate() {
            *b = (i.wrapping_mul(31) ^ 0x5a) as u8;
        }
        let card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::default(),
            sent: Mutex::default(),
            masked: Mutex::default(),
            sent_masked: Mutex::default(),
            hot_hashes: Mutex::default(),
            has_hot_hashes: AtomicBool::new(false),
            size: PAGE as u64,
        };
        let skip = RelaySkipSnapshot::capture(&card, &[(0, &gpu0[..], 1)]);
        let works = vec![RelayPieceWork {
            p_lo: 0,
            old_ptr: old0.as_mut_ptr() as usize,
            old_len: old0.len(),
            items: vec![(0, gpu0.as_ptr() as usize, gpu0.len(), 1)],
        }];
        let bucket = merge_relay_chunk(title_mem.as_ptr() as usize, works, &skip, false);
        assert_eq!(bucket.writes.len(), 1);
        assert!(bucket.arena.is_empty(), "clean GPU write should borrow gpu slice directly");
        assert!(matches!(bucket.writes[0].src, RelayWriteSrc::Gpu(_)));
        assert!(bucket.hash_clears.is_empty());
        assert_eq!(bucket.hash_updates.len(), PAGE / HOT_HASH_BLOCK);
        for &(slot, hash) in &bucket.hash_updates {
            let off = slot * HOT_HASH_BLOCK;
            assert_eq!(hash, hot_page_hash(&gpu0[off..off + HOT_HASH_BLOCK]) | 1);
        }
        assert_eq!(old0, gpu0);
    }

    #[test]
    fn relay_merge_drop_counters_track_all_zero_writeback_outcomes_and_superseded_helper() {
        use super::{
            RELAY_OUT_ALREADY_EQUAL, RELAY_OUT_EQUAL_OLD, RELAY_OUT_SUPERSEDED,
            RELAY_OUT_UNMAPPED, RELAY_OUT_WRITTEN, card_range_is_superseded,
        };
        use windows_sys::Win32::System::Memory::{
            MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc, VirtualFree,
        };

        // Allocate 4 pieces of committed memory for equal_old, superseded, already_equal, and written.
        let mut title_mem: Vec<u8> = (0..4 * PAGE).map(|i| (i * 17 + 3) as u8).collect();
        let mut old0 = title_mem[0..PAGE].to_vec();
        let mut old1 = title_mem[PAGE..2 * PAGE].to_vec();
        let mut old2 = title_mem[2 * PAGE..3 * PAGE].to_vec();
        let mut old3 = title_mem[3 * PAGE..4 * PAGE].to_vec();

        // Piece 0: gpu == old -> equal_old.
        let gpu0 = old0.clone();
        // Piece 1: gpu differs from old at [16..32), but covered by dirty -> superseded.
        let mut gpu1 = old1.clone();
        gpu1[16..32].fill(0xaa);
        // Piece 2: gpu differs from old at [64..80), AND title_mem ALREADY has those exact bytes -> already_equal.
        let mut gpu2 = old2.clone();
        gpu2[64..80].fill(0x55);
        title_mem[2 * PAGE + 64..2 * PAGE + 80].fill(0x55);
        // Piece 3: gpu differs from old at [8..16) and title_mem does not -> written.
        let mut gpu3 = old3.clone();
        gpu3[8..16].fill(0xee);

        let card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::new(vec![(PAGE + 16, PAGE + 32)]),
            sent: Mutex::new(vec![(PAGE + 32, PAGE + 64, 10)]),
            masked: Mutex::default(),
            sent_masked: Mutex::default(),
            hot_hashes: Mutex::default(),
            has_hot_hashes: AtomicBool::new(false),
            size: (4 * PAGE) as u64,
        };
        let skip = RelaySkipSnapshot::capture(
            &card,
            &[
                (0, &gpu0[..], 5),
                (PAGE, &gpu1[..], 5),
                (2 * PAGE, &gpu2[..], 5),
                (3 * PAGE, &gpu3[..], 5),
            ],
        );
        let works = vec![
            RelayPieceWork {
                p_lo: 0,
                old_ptr: old0.as_mut_ptr() as usize,
                old_len: old0.len(),
                items: vec![(0, gpu0.as_ptr() as usize, gpu0.len(), 5)],
            },
            RelayPieceWork {
                p_lo: PAGE,
                old_ptr: old1.as_mut_ptr() as usize,
                old_len: old1.len(),
                items: vec![(PAGE, gpu1.as_ptr() as usize, gpu1.len(), 5)],
            },
            RelayPieceWork {
                p_lo: 2 * PAGE,
                old_ptr: old2.as_mut_ptr() as usize,
                old_len: old2.len(),
                items: vec![(2 * PAGE, gpu2.as_ptr() as usize, gpu2.len(), 5)],
            },
            RelayPieceWork {
                p_lo: 3 * PAGE,
                old_ptr: old3.as_mut_ptr() as usize,
                old_len: old3.len(),
                items: vec![(3 * PAGE, gpu3.as_ptr() as usize, gpu3.len(), 5)],
            },
        ];
        let bucket = merge_relay_chunk(title_mem.as_ptr() as usize, works, &skip, false);
        assert_eq!(bucket.writes.len(), 1);
        assert_eq!(bucket.stats.pieces[RELAY_OUT_EQUAL_OLD], 1);
        assert_eq!(bucket.stats.bytes[RELAY_OUT_EQUAL_OLD], PAGE as u64);
        assert_eq!(bucket.stats.pieces[RELAY_OUT_SUPERSEDED], 1);
        assert_eq!(bucket.stats.bytes[RELAY_OUT_SUPERSEDED], PAGE as u64);
        assert_eq!(bucket.stats.pieces[RELAY_OUT_ALREADY_EQUAL], 1);
        assert_eq!(bucket.stats.bytes[RELAY_OUT_ALREADY_EQUAL], PAGE as u64);
        assert_eq!(bucket.stats.pieces[RELAY_OUT_WRITTEN], 1);
        assert_eq!(bucket.stats.bytes[RELAY_OUT_WRITTEN], 8);

        // Uncommitted (MEM_RESERVE only) page triggers RELAY_OUT_UNMAPPED and still updates `old`.
        // SAFETY: reserved-only region owned by this test and freed below.
        let unmapped_ptr = unsafe {
            VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE, PAGE_NOACCESS) as usize
        };
        assert_ne!(unmapped_ptr, 0);
        let mut old_u = vec![0u8; PAGE];
        let mut gpu_u = vec![0u8; PAGE];
        gpu_u[0..16].fill(0x77);
        let empty_skip = RelaySkipSnapshot {
            ranges: super::RelaySkipRanges::Empty,
            masked: Vec::new(),
        };
        let bucket_u = merge_relay_chunk(
            unmapped_ptr,
            vec![RelayPieceWork {
                p_lo: 0,
                old_ptr: old_u.as_mut_ptr() as usize,
                old_len: old_u.len(),
                items: vec![(0, gpu_u.as_ptr() as usize, gpu_u.len(), 5)],
            }],
            &empty_skip,
            false,
        );
        assert!(bucket_u.writes.is_empty());
        assert_eq!(bucket_u.stats.pieces[RELAY_OUT_UNMAPPED], 1);
        assert_eq!(bucket_u.stats.bytes[RELAY_OUT_UNMAPPED], PAGE as u64);
        assert_eq!(&old_u[0..16], &[0x77; 16]);
        // SAFETY: freeing our reserved test region.
        unsafe { VirtualFree(unmapped_ptr as *mut _, 0, MEM_RELEASE) };

        // Verify `card_range_is_superseded` handles single-range, multi-range (dirty + newer sent),
        // and non-superseded (older sent) cases.
        let dirty = [(PAGE + 16, PAGE + 32)];
        let sent = [(PAGE + 32, PAGE + 64, 10)];
        assert!(card_range_is_superseded(&dirty, &sent, PAGE + 16, PAGE + 32, 10));
        assert!(card_range_is_superseded(&dirty, &sent, PAGE + 16, PAGE + 64, 5));
        assert!(!card_range_is_superseded(&dirty, &sent, PAGE + 16, PAGE + 64, 10));
        assert!(!card_range_is_superseded(&dirty, &sent, PAGE, PAGE + 32, 5));
    }
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

static BUFFERS: RwLock<BTreeMap<(u64, usize), Entry>> = RwLock::new(BTreeMap::new());

fn buffers_read() -> std::sync::RwLockReadGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.read().unwrap_or_else(|e| e.into_inner())
}

fn buffers_write() -> std::sync::RwLockWriteGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.write().unwrap_or_else(|e| e.into_inner())
}

pub(crate) const SHADOW_EPOCH_BUCKETS: usize = 64;
static SHADOW_EPOCHS: [AtomicU64; SHADOW_EPOCH_BUCKETS] =
    [const { AtomicU64::new(1) }; SHADOW_EPOCH_BUCKETS];

#[inline]
fn bump_shadow_epochs(start: u64, len: u64) {
    if len == 0 {
        return;
    }
    let b0 = (start >> 20) as usize;
    let b1 = (start.saturating_add(len - 1) >> 20) as usize;
    let count = (b1 - b0 + 1).min(SHADOW_EPOCH_BUCKETS);
    for i in 0..count {
        SHADOW_EPOCHS[(b0 + i) & (SHADOW_EPOCH_BUCKETS - 1)].fetch_add(1, Ordering::Release);
    }
}

#[inline]
pub(crate) fn epoch_for_range(start: u64, len: u64) -> u64 {
    let b0 = (start >> 20) as usize;
    let b1 = (start.saturating_add(len.max(1) - 1) >> 20) as usize;
    if b0 == b1 {
        return SHADOW_EPOCHS[b0 & (SHADOW_EPOCH_BUCKETS - 1)].load(Ordering::Acquire);
    }
    let count = (b1 - b0 + 1).min(SHADOW_EPOCH_BUCKETS);
    let mut sum = 0u64;
    for i in 0..count {
        sum = sum.wrapping_add(
            SHADOW_EPOCHS[(b0 + i) & (SHADOW_EPOCH_BUCKETS - 1)].load(Ordering::Acquire),
        );
    }
    sum
}

pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers_write().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
    bump_shadow_epochs(start, len);
    SHADOW_GEN.fetch_add(1, Ordering::Release);
}

static SHADOW_GEN: AtomicU64 = AtomicU64::new(1);

#[allow(dead_code)]
pub(crate) fn generation() -> u64 {
    SHADOW_GEN.load(Ordering::Acquire)
}

pub(crate) fn unregister(resource: usize, start: u64) {
    if let Some(entry) = buffers_write().remove(&(start, resource)) {
        if let Some(s) = &entry.shadow {
            s.mark_dead();
        }
        bump_shadow_epochs(start, entry.end.saturating_sub(start));
        SHADOW_GEN.fetch_add(1, Ordering::Release);
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn extent(address: u64) -> Option<(u64, u64)> {
    buffers_read()
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end > address)
        .map(|(k, e)| (k.0, e.end - k.0))
}

static INPLACE_EVICTED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

pub(crate) fn take_in_place_evicted() -> Vec<(u64, u64)> {
    let mut list = INPLACE_EVICTED.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *list)
}

pub(crate) fn in_place_let_go(at: u64, len: u64) -> u64 {
    let end = at.saturating_add(len);
    let mut gone: Vec<Arc<Shadow>> = Vec::new();
    {
        let mut map = LOOSE.write().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<(u64, u64)> = map
            .range((at.saturating_sub(WINDOW), 0)..(end, 0))
            .filter(|(key, shadow)| shadow.place.is_some() && key.0 + key.1 > at)
            .map(|(key, _)| *key)
            .collect();
        gone.extend(keys.into_iter().filter_map(|key| map.remove(&key)));
    }
    {
        let mut map = buffers_write();
        for (_, entry) in map.range_mut(..(end, 0)).rev().take(256) {
            if entry.end > at && entry.shadow.as_ref().is_some_and(|s| s.place.is_some()) {
                gone.extend(entry.shadow.take());
            }
        }
    }
    let n = gone.len() as u64;
    if !gone.is_empty() {
        for shadow in &gone {
            shadow.mark_dead();
        }
        bump_shadow_epochs(at, len);
        SHADOW_GEN.fetch_add(1, Ordering::Release);
        let mut evicted = INPLACE_EVICTED.lock().unwrap_or_else(|e| e.into_inner());
        for shadow in &gone {
            evicted.push((shadow.start, shadow.start.saturating_add(shadow.len)));
        }
    }
    for shadow in gone {
        crate::host::retire(shadow);
    }
    n
}

pub(crate) fn spans() -> Vec<(u64, u64, bool)> {
    let mut spans: Vec<(u64, u64, bool)> = buffers_read()
        .iter()
        .map(|(key, entry)| (key.0, entry.end - key.0, false))
        .collect();
    let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
    spans.extend(loose.keys().map(|&(start, len)| (start, len, true)));
    spans
}

pub(crate) fn find_existing(address: u64, len: u64) -> Option<Arc<Shadow>> {
    find_existing_with_range(address, len).map(|(s, _, _)| s)
}

pub(crate) fn find_existing_with_range(address: u64, len: u64) -> Option<(Arc<Shadow>, u64, u64)> {
    let end = address.checked_add(len.max(1))?;
    let (buf_lo, buf_hi) = {
        let map = buffers_read();
        let mut max_skipped_end = 0u64;
        let mut matched: Option<((u64, usize), &Entry)> = None;
        for (&k, e) in map.range(..=(address, usize::MAX)).rev().take(64) {
            if e.end >= end {
                matched = Some((k, e));
                break;
            }
            max_skipped_end = max_skipped_end.max(e.end);
        }
        let next_start = map
            .range((address.saturating_add(1), 0)..)
            .next()
            .map_or(u64::MAX, |(k, _)| k.0);
        if let Some((k, entry)) = matched
            && let Some(shadow) = &entry.shadow
        {
            shadow.used.store(crate::host::moment(), Ordering::Relaxed);
            let (vs, ve) = if max_skipped_end <= address {
                (k.0.max(max_skipped_end), entry.end.min(next_start))
            } else {
                (address, end)
            };
            return Some((shadow.clone(), vs, ve));
        }
        let (lo, hi) = if matched.is_some() {
            (address, end)
        } else if max_skipped_end <= address {
            (max_skipped_end, next_start)
        } else {
            (address, next_start)
        };
        (lo, hi)
    };
    let map = LOOSE.read().unwrap_or_else(|e| e.into_inner());
    let from = address.saturating_sub(WINDOW);
    map.range((from, 0)..=(address, u64::MAX))
        .find(|((start, bytes), _)| *start <= address && *start + *bytes >= end)
        .map(|(_, s)| {
            s.used.store(crate::host::moment(), Ordering::Relaxed);
            let vs = s.start.max(buf_lo).min(address);
            let ve = s.start.saturating_add(s.len).min(buf_hi).max(end);
            (s.clone(), vs, ve)
        })
}

pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    lookup_with_range(address, len, false).map(|(s, _, _)| s)
}

pub(crate) fn find_with_range(address: u64, len: u64) -> Option<(Arc<Shadow>, u64, u64)> {
    lookup_with_range(address, len, false)
}

#[allow(dead_code)]
pub(crate) fn find_any(address: u64, len: u64) -> Option<Arc<Shadow>> {
    if !fine() {
        return find(address, len);
    }
    lookup_with_range(address, len, true).map(|(s, _, _)| s)
}

pub(crate) fn find_any_with_range(address: u64, len: u64) -> Option<(Arc<Shadow>, u64, u64)> {
    if !fine() {
        return find_with_range(address, len);
    }
    lookup_with_range(address, len, true)
}

fn lookup_with_range(address: u64, len: u64, any: bool) -> Option<(Arc<Shadow>, u64, u64)> {
    let end = address.checked_add(len.max(1))?;
    {
        let map = buffers_read();
        let mut max_skipped_end = 0u64;
        let mut found: Option<((u64, usize), &Entry)> = None;
        for (&k, e) in map.range(..=(address, usize::MAX)).rev().take(64) {
            if e.end >= end {
                found = Some((k, e));
                break;
            }
            max_skipped_end = max_skipped_end.max(e.end);
        }
        let next_start = map
            .range((address.saturating_add(1), 0)..)
            .next()
            .map_or(u64::MAX, |(k, _)| k.0);
        match found {
            Some((k, entry)) if entry.shadow.is_some() => {
                if let Some(s) = &entry.shadow {
                    s.used.store(crate::host::moment(), Ordering::Relaxed);
                    let (vs, ve) = if max_skipped_end <= address {
                        (k.0.max(max_skipped_end), entry.end.min(next_start))
                    } else {
                        (address, end)
                    };
                    return Some((s.clone(), vs, ve));
                }
            }
            None => {
                let buf_lo = if max_skipped_end <= address {
                    max_skipped_end
                } else {
                    address
                };
                drop(map);
                let s = loose(address, end, len <= 1, any)?;
                let vs = s.start.max(buf_lo).min(address);
                let ve = s.start.saturating_add(s.len).min(next_start).max(end);
                return Some((s, vs, ve));
            }
            Some(_) => {}
        }
    }
    let mut map = buffers_write();
    let mut max_skipped_end = 0u64;
    let mut key: Option<(u64, usize)> = None;
    for (&k, e) in map.range(..=(address, usize::MAX)).rev().take(64) {
        if e.end >= end {
            key = Some(k);
            break;
        }
        max_skipped_end = max_skipped_end.max(e.end);
    }
    let next_start = map
        .range((address.saturating_add(1), 0)..)
        .next()
        .map_or(u64::MAX, |(k, _)| k.0);
    let Some(key) = key else {
        let buf_lo = if max_skipped_end <= address {
            max_skipped_end
        } else {
            address
        };
        drop(map);
        let s = loose(address, end, len <= 1, any)?;
        let vs = s.start.max(buf_lo).min(address);
        let ve = s.start.saturating_add(s.len).min(next_start).max(end);
        return Some((s, vs, ve));
    };
    let entry = map.get_mut(&key)?;
    let entry_end = entry.end;
    let valid_bounds = |k0: u64| {
        if max_skipped_end <= address {
            (k0.max(max_skipped_end), entry_end.min(next_start))
        } else {
            (address, end)
        }
    };
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        let size = len.next_multiple_of(1 << 16);
        static INPLACE_BIG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let inplace_big = *INPLACE_BIG
            .get_or_init(|| std::env::var_os("OATHKEEP_INPLACE_BIG").is_some_and(|v| v == "1"));
        let prefer_card = !inplace_big && cards() && size >= card_limits().0;
        if !prefer_card && let Some(placed) = crate::inplace::place_for(key.0, len, false) {
            let shadow = Arc::new(Shadow::placed(placed, key.0, len, false));
            entry.shadow = Some(shadow.clone());
            let (vs, ve) = valid_bounds(key.0);
            return Some((shadow, vs, ve));
        }
        let (buffer, data, card) = match card_for(size) {
            Some(on_card) => {
                crate::host::name(&on_card, || {
                    format!("title buffer {:#x}+{len:#x} on the card", key.0)
                });
                tracing::info!(
                    start = format_args!("{:#x}", key.0),
                    len,
                    "a buffer's host copy is in the card's memory"
                );
                let card = Card {
                    before: Mutex::default(),
                    old: Mutex::default(),
                    has_old: AtomicBool::new(false),
                    dirty: Mutex::default(),
                    sent: Mutex::default(),
                    masked: Mutex::default(),
                    sent_masked: Mutex::default(),
                    hot_hashes: Mutex::default(),
                    has_hot_hashes: AtomicBool::new(false),
                    size,
                };
                (on_card, std::ptr::null_mut(), Some(card))
            }
            None => {
                if prefer_card && let Some(placed) = crate::inplace::place_for(key.0, len, false) {
                    let shadow = Arc::new(Shadow::placed(placed, key.0, len, false));
                    entry.shadow = Some(shadow.clone());
                    let (vs, ve) = valid_bounds(key.0);
                    return Some((shadow, vs, ve));
                }
                let (buffer, data) = host()?.shared_buffer(size)?;
                crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
                (buffer, data, None)
            }
        };
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        let mut shadow = Shadow::new(buffer, data, gpu, key.0, len);
        shadow.card = card;
        let shadow = Arc::new(shadow);
        if shadow.card.is_some() {
            CARDED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::downgrade(&shadow));
        }
        entry.shadow = Some(shadow);
    } else if let Some(s) = &entry.shadow {
        s.used.store(crate::host::moment(), Ordering::Relaxed);
    }
    let (vs, ve) = valid_bounds(key.0);
    entry.shadow.clone().map(|s| (s, vs, ve))
}

const WINDOW: u64 = 4 << 20;

fn window() -> u64 {
    static GRID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *GRID.get_or_init(|| {
        let kb = std::env::var("OATHKEEP_WINDOW_KB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .or(Some(1024));
        grid(kb)
    })
}

fn grid(kb: Option<u64>) -> u64 {
    match kb {
        Some(kb) if kb.is_power_of_two() && (64..=4096).contains(&kb) => kb << 10,
        _ => WINDOW,
    }
}

fn window_of(address: u64, end: u64, grid: u64) -> (u64, u64) {
    let start = address & !(grid - 1);
    (start, end.next_multiple_of(grid) - start)
}

fn tails() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WINDOW_TAILS").is_none_or(|v| v != "0"))
}

const TAIL_STEP: u64 = 256 << 10;

fn tail_of_grid(address: u64, end: u64, cell_grid: u64) -> (u64, u64) {
    let step_grid = cell_grid.max(TAIL_STEP);
    let (cell, len) = window_of(
        address,
        end.max(address.saturating_add(TAIL_STEP)),
        step_grid,
    );
    let start = address & !(TAIL_STEP - 1);
    (start, cell + len - start)
}

fn tail_of(address: u64, end: u64) -> (u64, u64) {
    tail_of_grid(address, end, window())
}

fn inplace_unsized_fallback(address: u64, end: u64) -> (u64, u64) {
    let start = address & !(TAIL_STEP - 1);
    let min_end = end.max(address.saturating_add(TAIL_STEP));
    let len = (min_end - start).next_multiple_of(TAIL_STEP);
    (start, len)
}

fn holder(windows: impl Iterator<Item = (u64, u64)>, address: u64, end: u64) -> Option<(u64, u64)> {
    windows
        .filter(|&(start, bytes)| start <= address && start + bytes >= end)
        .max_by_key(|&(start, bytes)| (bytes, std::cmp::Reverse(start)))
}

fn fine() -> bool {
    tails() || window() < WINDOW
}

#[cfg(feature = "diagnostics")]
pub(crate) fn window_cover() -> u64 {
    let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
    cover(loose.keys().copied())
}

#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
fn cover(windows: impl Iterator<Item = (u64, u64)>) -> u64 {
    let (mut covered, mut reached) = (0, 0);
    for (start, bytes) in windows {
        let end = start + bytes;
        if end > reached {
            covered += end - start.max(reached);
            reached = end;
        }
    }
    covered
}

fn longest_tail_grid(
    windows: impl Iterator<Item = (u64, u64)>,
    address: u64,
    end: u64,
    cell_grid: u64,
) -> Option<(u64, u64)> {
    let step_grid = cell_grid.max(TAIL_STEP);
    let (cell, len) = window_of(
        address,
        end.max(address.saturating_add(TAIL_STEP)),
        step_grid,
    );
    let step = address & !(TAIL_STEP - 1);
    windows
        .filter(|&(start, bytes)| start <= step && start + bytes == cell + len)
        .min()
}

fn longest_tail(
    windows: impl Iterator<Item = (u64, u64)>,
    address: u64,
    end: u64,
) -> Option<(u64, u64)> {
    longest_tail_grid(windows, address, end, window())
}

static LOOSE: RwLock<BTreeMap<(u64, u64), Arc<Shadow>>> = RwLock::new(BTreeMap::new());

fn loose(address: u64, end: u64, unsized_ask: bool, any: bool) -> Option<Arc<Shadow>> {
    let unsized_grid = window().max(TAIL_STEP);
    let search_from = address.saturating_sub(WINDOW);
    let inplace_l3 = crate::inplace::level() >= 3;
    let pick_existing = |map: &BTreeMap<(u64, u64), Arc<Shadow>>| -> Option<(u64, u64)> {
        if unsized_ask && inplace_l3 {
            let there = map
                .range((search_from, 0)..=(address, u64::MAX))
                .filter(|((start, bytes), _)| {
                    *start <= address
                        && *start + *bytes >= end
                        && (*start + *bytes).saturating_sub(address) >= (256 << 10)
                })
                .map(|(key, _)| *key);
            holder(there, address, end)
        } else if unsized_ask && tails() {
            let there = map
                .range((search_from, 0)..=(address, u64::MAX))
                .map(|(key, _)| *key);
            Some(longest_tail(there, address, end).unwrap_or_else(|| tail_of(address, end)))
        } else if (fine() || inplace_l3) && !unsized_ask {
            let _ = any;
            let there = map
                .range((search_from, 0)..=(address, u64::MAX))
                .map(|(key, _)| *key);
            holder(there, address, end)
        } else {
            let grid_size = if unsized_ask { unsized_grid } else { window() };
            let target_end = if unsized_ask {
                end.max(address.saturating_add(TAIL_STEP))
            } else {
                end
            };
            Some(window_of(address, target_end, grid_size))
        }
    };
    let note = |shadow: &Shadow| {
        shadow.used.store(crate::host::moment(), Ordering::Relaxed);
        shadow.asked[0].fetch_min(address, Ordering::Relaxed);
        shadow.asked[1].fetch_max(end, Ordering::Relaxed);
        if unsized_ask {
            shadow.unsized_asks.fetch_add(1, Ordering::Relaxed);
        }
    };
    {
        let map = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        if let Some(key) = pick_existing(&map)
            && let Some(shadow) = map.get(&key)
        {
            note(shadow);
            return Some(shadow.clone());
        }
    }
    let named = {
        let grid_size = if unsized_ask { unsized_grid } else { window() };
        let target_end = if unsized_ask {
            end.max(address.saturating_add(TAIL_STEP))
        } else {
            end
        };
        let grid = window_of(address, target_end, grid_size);
        if inplace_l3 && !crate::inplace::section_run_in_place(grid.0, grid.1) {
            if let Some((rs, rl, next_mapped)) =
                crate::inplace::section_run_around(address, end, grid_size)
                && (!unsized_ask || (rs + rl).saturating_sub(address) >= (256 << 10) || !next_mapped)
            {
                (rs, rl)
            } else if !unsized_ask {
                window_of(address, end, 64 << 10)
            } else {
                inplace_unsized_fallback(address, end)
            }
        } else {
            grid
        }
    };
    let mut map = LOOSE.write().unwrap_or_else(|e| e.into_inner());
    let (start, len) = pick_existing(&map).unwrap_or(named);
    if let Some(shadow) = map.get(&(start, len)) {
        note(shadow);
        return Some(shadow.clone());
    }
    if let Some(placed) = crate::inplace::place_for(start, len, true) {
        let shadow = Arc::new(Shadow::placed(placed, start, len, true));
        note(&shadow);
        map.insert((start, len), shadow.clone());
        return Some(shadow);
    }
    let (buffer, data, card) = match window_card_for(len) {
        Some(on_card) => {
            crate::host::name(&on_card, || {
                format!("title memory {start:#x}+{len:#x} on the card")
            });
            let card = Card {
                before: Mutex::default(),
                old: Mutex::default(),
                has_old: AtomicBool::new(false),
                dirty: Mutex::default(),
                sent: Mutex::default(),
                masked: Mutex::default(),
                sent_masked: Mutex::default(),
                hot_hashes: Mutex::default(),
                has_hot_hashes: AtomicBool::new(false),
                size: len,
            };
            (on_card, std::ptr::null_mut(), Some(card))
        }
        None => {
            let (buffer, data) = host()?.shared_buffer(len)?;
            crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
            (buffer, data, None)
        }
    };
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let mut shadow = Shadow::new(buffer, data, gpu, start, len);
    shadow.window = true;
    shadow.card = card;
    let shadow = Arc::new(shadow);
    if shadow.card.is_some() {
        CARDED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::downgrade(&shadow));
    }
    note(&shadow);
    map.insert((start, len), shadow.clone());
    Some(shadow)
}

#[cfg(test)]
mod window_tests {
    use super::{
        TAIL_STEP, WINDOW, cover, grid, holder, inplace_unsized_fallback, longest_tail_grid,
        tail_of_grid, window_of,
    };

    #[test]
    fn inplace_unsized_fallback_keeps_headroom_without_spanning_whole_window() {
        let addr = 0xd_120c_1234u64; // near end of a 1 MiB section
        let (start, len) = inplace_unsized_fallback(addr, addr + 1);
        assert_eq!(start, 0xd_120c_0000);
        assert_eq!(len, 2 * TAIL_STEP);
        assert!(start <= addr);
        assert!(start + len >= addr + TAIL_STEP);
        assert!(len <= 2 * TAIL_STEP);

        let aligned = 0xd_120c_0000u64;
        let (start2, len2) = inplace_unsized_fallback(aligned, aligned + 1);
        assert_eq!((start2, len2), (aligned, TAIL_STEP));
    }

    #[test]
    fn an_ask_takes_the_longest_window_that_holds_it() {
        let cell = 0xd_1200_0000u64;
        let (ask, end) = (cell + 0x9_0000, cell + 0xb_0000);
        assert_eq!(holder([].into_iter(), ask, end), None);
        let there = [
            (cell, 0x4_0000),
            (cell + 0x8_0000, 0x2_0000),
            (cell + 0x8_0000, 0x4_0000),
            (cell + 0x4_0000, 0x10_0000),
            (cell + 0xa_0000, 0x40_0000),
        ];
        assert_eq!(
            holder(there.into_iter(), ask, end),
            Some((cell + 0x4_0000, 0x10_0000))
        );
        let two = [(cell + 0x8_0000, 0x8_0000), (cell + 0x4_0000, 0x8_0000)];
        assert_eq!(
            holder(two.into_iter(), ask, end),
            Some((cell + 0x4_0000, 0x8_0000))
        );
    }

    #[test]
    fn overlapping_windows_cover_their_memory_once() {
        assert_eq!(cover([].into_iter()), 0);
        let apart = [(0x10_0000u64, 0x4_0000u64), (0x20_0000, 0x4_0000)];
        assert_eq!(cover(apart.into_iter()), 0x8_0000);
        let over = [
            (0x10_0000u64, 0x10_0000u64),
            (0x14_0000, 0x4_0000),
            (0x1c_0000, 0x8_0000),
            (0x24_0000, 0x4_0000),
        ];
        assert_eq!(cover(over.into_iter()), 0x18_0000);
    }

    #[test]
    fn tails_run_to_the_cells_end_and_the_longest_is_taken() {
        let cell = 0xd_1200_0000u64;
        let ask = cell + 0x12_3456;
        assert_eq!(
            tail_of_grid(ask, ask + 1, WINDOW),
            (cell + 0x10_0000, WINDOW - 0x10_0000)
        );
        assert_eq!(
            tail_of_grid(cell + 0xffff, cell + 0x1_0000, WINDOW),
            (cell, WINDOW)
        );
        let end = cell + WINDOW;
        let tail = |from: u64| (cell + from, WINDOW - from);
        assert_eq!(longest_tail_grid([].into_iter(), ask, ask + 1, WINDOW), None);
        assert_eq!(
            longest_tail_grid([tail(0x20_0000)].into_iter(), ask, ask + 1, WINDOW),
            None
        );
        let there = [
            (cell - WINDOW, WINDOW),
            (cell, 0x4_0000),
            tail(0x8_0000),
            tail(0x10_0000),
            (cell + 0x10_0000, 0x4_0000),
            tail(0x20_0000),
        ];
        assert_eq!(
            longest_tail_grid(there.into_iter(), ask, ask + 1, WINDOW),
            Some(tail(0x8_0000))
        );
        let (start, bytes) = tail(0x8_0000);
        assert!(start <= ask && start + bytes == end);
        let with_whole = [(cell, WINDOW), tail(0x8_0000)];
        assert_eq!(
            longest_tail_grid(with_whole.into_iter(), ask, ask + 1, WINDOW),
            Some((cell, WINDOW))
        );

        // 1 MiB window grid keeps at least TAIL_STEP headroom and snaps to 1 MiB cells.
        let one_mb = 1u64 << 20;
        assert_eq!(
            tail_of_grid(cell + 0x1_2345, cell + 0x1_2346, one_mb),
            (cell, one_mb)
        );
        assert_eq!(
            tail_of_grid(cell + 0x5_0000, cell + 0x5_0001, one_mb),
            (cell + 0x4_0000, one_mb - 0x4_0000)
        );
        // Near the end of a 1 MiB cell (<256 KiB left), extends into the next 1 MiB cell.
        let near_end = cell + 0xd_0000;
        let (ns, nl) = tail_of_grid(near_end, near_end + 1, one_mb);
        assert_eq!((ns, nl), (cell + 0xc_0000, 0x14_0000));
        assert!(ns + nl >= near_end + TAIL_STEP);
        // A subsequent ask inside the next 1 MiB cell reuses that spanning tail without overlap.
        let next_cell_ask = cell + one_mb + 0x2_0000;
        assert_eq!(
            longest_tail_grid([(ns, nl)].into_iter(), next_cell_ask, next_cell_ask + 1, one_mb),
            Some((ns, nl))
        );
    }

    #[test]
    fn the_grid_is_a_power_of_two_or_the_default() {
        assert_eq!(grid(None), WINDOW);
        assert_eq!(grid(Some(4096)), WINDOW);
        assert_eq!(grid(Some(1024)), 1 << 20);
        assert_eq!(grid(Some(256)), 256 << 10);
        assert_eq!(grid(Some(64)), 64 << 10);
        for bad in [0, 32, 100, 1000, 8192] {
            assert_eq!(grid(Some(bad)), WINDOW, "{bad}");
        }
    }

    #[test]
    fn windows_hold_their_ask_on_the_grid() {
        let (address, end) = (0xd_1234_5678u64, 0xd_1234_5678u64 + 0x9000);
        assert_eq!(window_of(address, end, WINDOW), (0xd_1200_0000, 4 << 20));
        assert_eq!(window_of(address, end, 1 << 20), (0xd_1230_0000, 1 << 20));
        assert_eq!(
            window_of(address, end, 256 << 10),
            (0xd_1234_0000, 256 << 10)
        );
        let end = address + 0x4_0000;
        assert_eq!(
            window_of(address, end, 256 << 10),
            (0xd_1234_0000, 512 << 10)
        );
        for g in [64u64 << 10, 256 << 10, 1 << 20, WINDOW] {
            let (start, len) = window_of(address, end, g);
            assert!(start <= address && start + len >= end);
            assert!(start % g == 0 && len % g == 0);
        }
    }

    #[test]
    fn hot_page_hash_detects_single_bit_changes() {
        use super::{PAGE, hot_page_hash, hot_page_hash_scalar};
        let mut page = [0u8; PAGE];
        let h0 = hot_page_hash(&page);
        assert_eq!(h0, hot_page_hash(&page));
        assert_eq!(h0, hot_page_hash_scalar(&page));
        for at in [0, 7, 31, 32, 63, 64, 127, 128, 2048, PAGE - 1] {
            page[at] ^= 1;
            let h1 = hot_page_hash(&page);
            assert_ne!(h0, h1, "byte {at}");
            assert_eq!(h1, hot_page_hash_scalar(&page), "scalar/avx2 match at {at}");
            page[at] ^= 1;
            assert_eq!(h0, hot_page_hash(&page));
        }
        // Check AVX2 vs scalar equivalence across unaligned/short/long sizes and block swaps.
        for len in [0usize, 1, 7, 8, 31, 63, 64, 127, 128, 129, 255, 256, 4096, 4103] {
            let buf: Vec<u8> = (0..len).map(|i| (i * 73 + 19) as u8).collect();
            assert_eq!(
                hot_page_hash(&buf),
                hot_page_hash_scalar(&buf),
                "len {len}"
            );
        }
        let mut swapped = vec![0u8; 256];
        for (i, b) in swapped.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(29).wrapping_add(3);
        }
        let orig = hot_page_hash(&swapped);
        let (a, b) = swapped.split_at_mut(128);
        a.swap_with_slice(b);
        assert_ne!(orig, hot_page_hash(&swapped), "128-byte block swap must change hash");
    }

    #[test]
    fn merge_refresh_ranges_keeps_far_spans_disjoint_and_coalesces_near() {
        use super::merge_refresh_ranges;
        let mut spans = Vec::new();
        merge_refresh_ranges(&mut spans, 0x1000, 0x2000);
        merge_refresh_ranges(&mut spans, 0x0a00_0000, 0x0a00_1000);
        assert_eq!(spans, [(0x1000, 0x2000), (0x0a00_0000, 0x0a00_1000)]);

        // Nearby range within 256 KiB merges into the first span without bridging to 160 MiB.
        merge_refresh_ranges(&mut spans, 0x1_0000, 0x1_2000);
        assert_eq!(spans, [(0x1000, 0x1_2000), (0x0a00_0000, 0x0a00_1000)]);

        // Many far spans stay bounded at 32 entries.
        for i in 0..40usize {
            let base = (i + 1) * (1 << 20);
            merge_refresh_ranges(&mut spans, base, base + 0x1000);
        }
        assert!(spans.len() <= 32);
        assert!(spans.windows(2).all(|w| w[0].1 < w[1].0));
    }

    #[test]
    fn hot_hashes_vec_deduplicates_4k_subblocks_and_clears_ranges() {
        use super::{Card, HOT_HASH_BLOCK, HotHashes, PAGE, diff_hot_card_blocks};
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};

        let len = PAGE + 1536; // non-page-aligned length (64 KiB + 1.5 KiB)
        let slots = len.div_ceil(HOT_HASH_BLOCK);
        let card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::default(),
            sent: Mutex::default(),
            masked: Mutex::default(),
            sent_masked: Mutex::default(),
            hot_hashes: Mutex::new(HotHashes {
                slots: vec![0u128; slots],
                quiet: Vec::new(),
            }),
            has_hot_hashes: AtomicBool::new(false),
            size: len as u64,
        };
        let mut mem: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();

        // First scan populates every slot (including the partial tail slot) and marks all changed.
        let mut changed = Vec::new();
        {
            let mut hashes = card.hot_hashes.lock().unwrap();
            assert!(diff_hot_card_blocks(&mut hashes.slots, len, 0, &mem, |a, b| {
                changed.push((a, b));
            }));
            card.has_hot_hashes.store(true, Ordering::Release);
            assert!(hashes.slots.iter().all(|&h| h != 0 && (h & 1) == 1));
        }
        assert_eq!(changed.len(), slots);
        assert_eq!(changed.first(), Some(&(0, HOT_HASH_BLOCK)));
        assert_eq!(changed.last(), Some(&(PAGE, len)));

        // Unchanged second scan emits zero ranges.
        changed.clear();
        {
            let mut hashes = card.hot_hashes.lock().unwrap();
            assert!(!diff_hot_card_blocks(&mut hashes.slots, len, 0, &mem, |a, b| {
                changed.push((a, b));
            }));
        }
        assert!(changed.is_empty());

        // Single-byte edit in 4 KiB slot 5 emits ONLY slot 5 (4 KiB out of 64 KiB).
        mem[5 * HOT_HASH_BLOCK + 123] ^= 0x40;
        changed.clear();
        {
            let mut hashes = card.hot_hashes.lock().unwrap();
            assert!(diff_hot_card_blocks(&mut hashes.slots, len, 0, &mem, |a, b| {
                changed.push((a, b));
            }));
        }
        assert_eq!(changed, [(5 * HOT_HASH_BLOCK, 6 * HOT_HASH_BLOCK)]);

        // Partial sub-block refresh (256 bytes inside slot 2) clears slot 2's hash and emits only
        // the partial range; next full scan re-hashes slot 2 even if bytes reverted.
        let partial_off = 2 * HOT_HASH_BLOCK + 512;
        changed.clear();
        {
            let mut hashes = card.hot_hashes.lock().unwrap();
            diff_hot_card_blocks(
                &mut hashes.slots,
                len,
                partial_off,
                &mem[partial_off..partial_off + 256],
                |a, b| changed.push((a, b)),
            );
            assert_eq!(hashes.slots[2], 0);
            assert_ne!(hashes.slots[3], 0);
        }
        assert_eq!(changed, [(partial_off, partial_off + 256)]);

        // clear_hot_hashes zeroes only the overlapping slots.
        card.clear_hot_hashes(4 * HOT_HASH_BLOCK + 10, 6 * HOT_HASH_BLOCK + 1);
        {
            let hashes = card.hot_hashes.lock().unwrap();
            assert_ne!(hashes.slots[3], 0);
            assert_eq!(hashes.slots[4], 0);
            assert_eq!(hashes.slots[5], 0);
            assert_eq!(hashes.slots[6], 0);
            assert_ne!(hashes.slots[7], 0);
        }
        assert!(card.has_hot_hashes.load(Ordering::Acquire));

        // Full-range clear resets has_hot_hashes flag to false.
        card.clear_hot_hashes(0, len);
        assert!(!card.has_hot_hashes.load(Ordering::Acquire));
        assert!(card.hot_hashes.lock().unwrap().slots.iter().all(|&h| h == 0));
    }

    #[test]
    fn parallel_hot_card_diff_and_adaptive_cooling() {
        use super::{
            Card, HOT_HASH_BLOCK, PAGE, scan_card_pages_into,
        };
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };

        const N_PAGES: usize = 32; // >= PARALLEL_CARD_PAGES (16) -> exercises parallel wide_leaf path
        const LEN: usize = N_PAGES * PAGE;
        // SAFETY: 64 KiB-aligned allocation owned by this test and freed at the end.
        let ptr = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                LEN,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            ) as *mut u8
        };
        assert!(!ptr.is_null());
        let start = ptr as usize;
        assert_eq!(start % PAGE, 0);
        // SAFETY: committed allocation of `LEN` bytes.
        let mem = unsafe { std::slice::from_raw_parts_mut(ptr, LEN) };
        for (i, b) in mem.iter_mut().enumerate() {
            *b = (i.wrapping_mul(41).wrapping_add(7)) as u8;
        }

        let card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::default(),
            sent: Mutex::default(),
            masked: Mutex::default(),
            sent_masked: Mutex::default(),
            hot_hashes: Mutex::default(),
            has_hot_hashes: AtomicBool::new(false),
            size: LEN as u64,
        };
        let base = start / PAGE;
        let all_hot: Vec<(usize, bool)> = (0..N_PAGES).map(|i| (base + i, true)).collect();

        // Initial parallel pass hashes all 32 pages and emits one coalesced full span; nothing cools.
        let (spans, cool) =
            scan_card_pages_into(&card, start, LEN, start, start + LEN, &all_hot, 10, 4);
        assert_eq!(spans, [(start, start + LEN)]);
        assert!(cool.is_empty());

        // Multiple unchanged checks on the SAME frame (frame 10) emit 0 spans and do NOT cool.
        for _ in 0..5 {
            let (spans, cool) =
                scan_card_pages_into(&card, start, LEN, start, start + LEN, &all_hot, 10, 4);
            assert!(spans.is_empty());
            assert!(cool.is_empty());
        }

        // Modify adjacent 4 KiB blocks across a parallel chunk boundary (last 4 KiB of page 5 and
        // first 4 KiB of page 6) plus one isolated block in page 29 on frame 11.
        mem[6 * PAGE - 17] ^= 0x80;
        mem[6 * PAGE + 19] ^= 0x40;
        mem[29 * PAGE + 3 * HOT_HASH_BLOCK + 5] ^= 0x01;
        let (spans, cool) =
            scan_card_pages_into(&card, start, LEN, start, start + LEN, &all_hot, 11, 4);
        assert_eq!(
            spans,
            [
                (start + 6 * PAGE - HOT_HASH_BLOCK, start + 6 * PAGE + HOT_HASH_BLOCK),
                (
                    start + 29 * PAGE + 3 * HOT_HASH_BLOCK,
                    start + 29 * PAGE + 4 * HOT_HASH_BLOCK
                ),
            ]
        );
        assert!(cool.is_empty());

        // Frames 12 and 13 unchanged: quiet count reaches 3 for unchanged pages (< 4), 2 for pages 5,6,29.
        for frame in [12u64, 13] {
            let (spans, cool) =
                scan_card_pages_into(&card, start, LEN, start, start + LEN, &all_hot, frame, 4);
            assert!(spans.is_empty());
            assert!(cool.is_empty());
        }

        // Frame 14 unchanged: all pages except 5, 6, 29 have been quiet for 4 distinct frames -> cooled!
        let (spans, cool) =
            scan_card_pages_into(&card, start, LEN, start, start + LEN, &all_hot, 14, 4);
        assert!(spans.is_empty());
        let expected_cool: Vec<usize> = (0..N_PAGES)
            .filter(|&i| !matches!(i, 5 | 6 | 29))
            .map(|i| base + i)
            .collect();
        assert_eq!(cool, expected_cool);

        // Partial-page check on page 5 on frame 15 must NOT cool page 5.
        let partial_hot = [(base + 5, true)];
        let (spans, cool) = scan_card_pages_into(
            &card,
            start,
            LEN,
            start + 5 * PAGE,
            start + 5 * PAGE + HOT_HASH_BLOCK,
            &partial_hot,
            15,
            4,
        );
        assert!(spans.is_empty());
        assert!(cool.is_empty());

        // Full-page check on pages 5, 6, 29 on frame 15 cools them (4 quiet frames: 12,13,14,15).
        let remaining_hot = [(base + 5, true), (base + 6, true), (base + 29, true)];
        let (spans, cool) =
            scan_card_pages_into(&card, start, LEN, start, start + LEN, &remaining_hot, 15, 4);
        assert!(spans.is_empty());
        assert_eq!(cool, [base + 5, base + 6, base + 29]);

        // SAFETY: our allocation.
        unsafe { VirtualFree(ptr.cast(), 0, MEM_RELEASE) };
    }

    #[test]
    fn unwatched_card_diff_skips_unchanged_and_parallelizes_large_buffers() {
        use super::{Card, HOT_HASH_BLOCK, PAGE, scan_unwatched_card_into};
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;

        // 2 MiB + 1536 bytes (>= PARALLEL_UNWATCHED_BYTES, non-4 KiB aligned tail).
        let len = (2 << 20) + 1536;
        let mut mem: Vec<u8> = (0..len).map(|i| (i * 53 + 17) as u8).collect();
        let start = mem.as_ptr() as usize;
        let card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::default(),
            sent: Mutex::default(),
            masked: Mutex::default(),
            sent_masked: Mutex::default(),
            hot_hashes: Mutex::default(),
            has_hot_hashes: AtomicBool::new(false),
            size: len as u64,
        };

        // First scan hashes everything in parallel and emits one coalesced full span.
        let spans = scan_unwatched_card_into(&card, start, len, start, start + len);
        assert_eq!(spans, [(start, start + len)]);

        // Unchanged frame emits 0 ranges (0 bytes uploaded).
        let spans = scan_unwatched_card_into(&card, start, len, start, start + len);
        assert!(spans.is_empty());

        // Mutate one byte near the start, two adjacent 4 KiB blocks in the middle, and the tail.
        let mid_block = 200 * HOT_HASH_BLOCK;
        mem[42] ^= 0x10;
        mem[mid_block - 1] ^= 0x20;
        mem[mid_block + 3] ^= 0x04;
        mem[len - 1] ^= 0x01;
        let spans = scan_unwatched_card_into(&card, start, len, start, start + len);
        assert_eq!(
            spans,
            [
                (start, start + HOT_HASH_BLOCK),
                (start + mid_block - HOT_HASH_BLOCK, start + mid_block + HOT_HASH_BLOCK),
                (start + (2 << 20), start + len),
            ]
        );

        // Next unchanged frame emits 0 ranges again.
        assert!(scan_unwatched_card_into(&card, start, len, start, start + len).is_empty());

        // clear_hot_hashes invalidates only the cleared sub-range for the next unwatched scan.
        card.clear_hot_hashes(PAGE, PAGE + 2 * HOT_HASH_BLOCK);
        let spans = scan_unwatched_card_into(&card, start, len, start, start + len);
        assert_eq!(spans, [(start + PAGE, start + PAGE + 2 * HOT_HASH_BLOCK)]);

        // Sequential sub-1 MiB path also deduplicates unchanged reads.
        let small_len = 2 * PAGE;
        let mut small_mem: Vec<u8> = (0..small_len).map(|i| (i * 13 + 5) as u8).collect();
        let small_start = small_mem.as_ptr() as usize;
        let small_card = Card {
            before: Mutex::default(),
            old: Mutex::default(),
            has_old: AtomicBool::new(false),
            dirty: Mutex::default(),
            sent: Mutex::default(),
            masked: Mutex::default(),
            sent_masked: Mutex::default(),
            hot_hashes: Mutex::default(),
            has_hot_hashes: AtomicBool::new(false),
            size: small_len as u64,
        };
        assert_eq!(
            scan_unwatched_card_into(&small_card, small_start, small_len, small_start, small_start + small_len),
            [(small_start, small_start + small_len)]
        );
        assert!(
            scan_unwatched_card_into(&small_card, small_start, small_len, small_start, small_start + small_len)
                .is_empty()
        );
        small_mem[PAGE + 100] ^= 0xff;
        assert_eq!(
            scan_unwatched_card_into(&small_card, small_start, small_len, small_start, small_start + small_len),
            [(small_start + PAGE, small_start + PAGE + HOT_HASH_BLOCK)]
        );
    }

    #[test]
    fn copied_state_tracks_partial_ranges_lazy_floors_and_uncopied_holes() {
        use super::{COPIED_CHUNK, CopiedState, PAGE, full_page_range};

        let pages = COPIED_CHUNK * 3 + 10; // 202 pages spanning 4 chunks
        let mut st = CopiedState::new(pages);

        // Initially all uncopied -> (false, u64::MAX) so guarded watch can skip journal scan.
        assert_eq!(st.range_since(0, pages, 0), (false, u64::MAX));
        assert_eq!(st.range_since(10, 30, 0), (false, u64::MAX));

        // Copy chunk 1 completely (pages 64..128) at ts=10, plus 5 pages in chunk 0 at ts=7.
        for p in 64..128 {
            assert!(st.set_raw(p, 10));
        }
        st.recompute_chunk(1);
        for p in 10..15 {
            assert!(st.set_page(p, 7));
        }
        // Duplicate set returns false and never decreases timestamp.
        assert!(!st.set_page(10, 5));
        assert_eq!(st.get(10, 0), 7);

        // Fully copied sub-range inside a partially copied buffer reports all_copied=true!
        assert_eq!(st.range_since(64, 128, 0), (true, 10));
        assert_eq!(st.range_since(10, 15, 0), (true, 7));
        // Mixed range (copied + uncopied) reports all_copied=false and min non-zero ts.
        assert_eq!(st.range_since(0, 128, 0), (false, 7));
        assert_eq!(st.range_since(100, 140, 0), (false, 10));

        // advance_synced bumps full chunk 1 lazily via chunk_floor and bumps only non-zero pages
        // in partial chunk 0, never turning 0s into non-zero!
        st.advance_synced(10, 140, 25);
        assert_eq!(st.range_since(64, 128, 0), (true, 25));
        assert_eq!(st.range_since(10, 15, 0), (true, 25));
        assert_eq!(st.get(0, 0), 0);
        assert_eq!(st.get(128, 0), 0);
        assert_eq!(st.get(80, 0), 25);

        // stale_pages_in respects chunk_floor and global_synced.
        let base = 0x100usize;
        let at = |p: usize| p * PAGE;
        let written = [(base + 80, 24), (base + 81, 26), (base + 128, 1)];
        assert_eq!(
            st.stale_pages_in(base, 0, at(base + 80), at(base + 82), &written),
            vec![base + 81]
        );
        // With global_synced=30 (when uncopied==0), 26 is no longer stale.
        assert!(
            st.stale_pages_in(base, 30, at(base + 80), at(base + 82), &written)
                .is_empty()
        );
        // Uncopied page 128 is always stale even with high global_synced.
        assert_eq!(
            st.stale_pages_in(base, 30, at(base + 128), at(base + 129), &written),
            vec![base + 128]
        );

        // full_page_range excludes partially covered boundary pages.
        let start = 0x10000usize;
        let end = 0x40000usize;
        assert_eq!(full_page_range(start, end, start, end, 1, 3), (0, 3));
        assert_eq!(
            full_page_range(start, end, start + 0x100, end - 0x100, 1, 3),
            (1, 2)
        );
        assert_eq!(full_page_range(0, 0x20000, 0x100, 0x200, 0, 2), (0, 0));
    }

    #[test]
    fn hot_page_quiet_doubles_threshold_after_each_cool_and_survives_reset() {
        use super::{DEFAULT_HOT_COOL_FRAMES, HotPageQuiet};
        assert_eq!(DEFAULT_HOT_COOL_FRAMES, 4);

        let mut q = HotPageQuiet::default();
        // First cooling requires 4 quiet frames (frames 11..=14 after change on frame 10).
        assert!(!q.record(10, true, true, true, 4));
        for f in 11..=13 {
            assert!(!q.record(f, false, true, true, 4), "frame {f}");
        }
        assert!(q.record(14, false, true, true, 4));
        assert_eq!(q.cool_count, 1);

        // Resetting quiet state preserves cool_count backoff.
        q.reset_quiet();
        assert_eq!(q.cool_count, 1);

        // Second cooling requires 8 quiet frames (4 << 1 = 8).
        assert!(!q.record(20, true, true, true, 4));
        for f in 21..=27 {
            assert!(!q.record(f, false, true, true, 4), "frame {f}");
        }
        assert!(q.record(28, false, true, true, 4));
        assert_eq!(q.cool_count, 2);

        // Third cooling requires 16 quiet frames (4 << 2 = 16) and saturates at MAX_QUIET_COOL_BACKOFF (2).
        assert!(!q.record(40, true, true, true, 4));
        for f in 41..=55 {
            assert!(!q.record(f, false, true, true, 4), "frame {f}");
        }
        assert!(q.record(56, false, true, true, 4));
        assert_eq!(q.cool_count, 2);
    }

    #[test]
    fn scanned_submission_deduplicates_overlapping_ranges_and_invalidates_on_epoch_or_sub() {
        use super::ScannedSubmission;
        use crate::watch::{WriteStamp, wrote};
        let mut s = ScannedSubmission::default();
        let st0 = crate::watch::write_stamp();

        // First scan in submission 1 covers [100, 300).
        assert_eq!(s.uncovered(1, st0, 100, 300), vec![(100, 300)]);
        s.record(1, st0, 100, 300);

        // Exact and contained repeats in the same submission & stamp return empty!
        assert!(s.uncovered(1, st0, 100, 300).is_empty());
        assert!(s.uncovered(1, st0, 150, 250).is_empty());

        // Overlapping range returns only the uncovered wings and coalesces on record.
        assert_eq!(s.uncovered(1, st0, 50, 400), vec![(50, 100), (300, 400)]);
        s.record(1, st0, 50, 100);
        s.record(1, st0, 300, 400);
        assert!(s.uncovered(1, st0, 50, 400).is_empty());
        assert_eq!(s.spans, vec![(50, 400)]);

        // Non-overlapping ranged write preserves scanned spans completely.
        wrote(1000, 256);
        let st1 = crate::watch::write_stamp();
        assert!(s.uncovered(1, st1, 50, 400).is_empty());
        assert_eq!(s.spans, vec![(50, 400)]);

        // Overlapping ranged write subtracts ONLY the written subrange [150, 220).
        wrote(150, 70);
        let st2 = crate::watch::write_stamp();
        assert_eq!(s.uncovered(1, st2, 50, 400), vec![(150, 220)]);
        assert_eq!(s.spans, vec![(50, 150), (220, 400)]);
        s.record(1, st2, 150, 220);
        assert_eq!(s.spans, vec![(50, 400)]);

        // Global epoch bump invalidates the entire cache.
        let st3 = WriteStamp {
            global: st2.global + 1,
            ranged: st2.ranged,
        };
        assert_eq!(s.uncovered(1, st3, 50, 400), vec![(50, 400)]);
        s.record(1, st3, 50, 400);
        assert!(s.uncovered(1, st3, 50, 400).is_empty());

        // New submission invalidates cache.
        assert_eq!(s.uncovered(2, st3, 50, 400), vec![(50, 400)]);
    }

    #[test]
    fn unwatched_diff_helpers_merge_noncarded_8b_and_carded_pieces() {
        use super::{MergeDiff, PAGE, PIECE, changed_runs, diff_runs_or_masked};

        // 1. Non-carded 8-byte unwatched buffer (like 0x2e79cdf0000+0x8):
        // unchanged bytes skip, changed bytes copy only the changed run and update snapshot.
        let mut title_8 = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut host_8 = title_8;
        let mut before_8 = title_8;
        assert_eq!(title_8, before_8);
        title_8[4..8].copy_from_slice(&[9, 10, 11, 12]);
        let runs = changed_runs(&title_8, &before_8);
        assert_eq!(runs, vec![(4, 4)]);
        for (r, m) in runs {
            host_8[r..r + m].copy_from_slice(&title_8[r..r + m]);
            before_8[r..r + m].copy_from_slice(&title_8[r..r + m]);
        }
        assert_eq!(host_8, title_8);
        assert_eq!(before_8, title_8);

        // 2. Carded multi-page unwatched piece: equal pieces skip, changed piece emits runs
        // at the piece's exact buffer offset `pg_lo + r`.
        let mut old_piece = vec![0x22u8; PIECE];
        let mut new_piece = old_piece.clone();
        let piece_offset = 3 * PIECE;
        new_piece[4096 + 16..4096 + 24].copy_from_slice(&[0xaa; 8]);
        assert_ne!(new_piece, old_piece);
        match diff_runs_or_masked(piece_offset, 4 * PIECE, &new_piece, &mut old_piece, 16) {
            MergeDiff::Runs(runs) => {
                assert_eq!(runs, vec![(4096 + 16, 8)]);
                assert_eq!(piece_offset + runs[0].0, 3 * PIECE + 4096 + 16);
            }
            other => panic!("expected Runs, got {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(old_piece, new_piece);
        let _ = PAGE;
    }
}

pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers_read()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}
