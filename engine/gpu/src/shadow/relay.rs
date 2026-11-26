use std::sync::atomic::{AtomicU64, Ordering};

use super::pool::keeps;

/// Whether the copy-back of what shaders wrote runs on a thread of its own,
/// beside the submitting thread and outside the submission lock
/// (`OATHKEEP_RELAY=1` or `2`; off by default; not with kept snapshots,
/// [`keeps`]). `crate::host::relay` says what it does; here it changes
/// what a copy keeps:
///
/// - the ranges shaders may be writing are counted per submission
///   ([`Flights`]): a range two flights declared stays closed to refreshes
///   until both have been copied back;
/// - the merge of what the card wrote never stores into memory the card
///   may be writing ([`merge_piece`]): what it learns goes into title
///   memory and into the copy's reference (the snapshot, or the CPU's side
///   of a copy in the card's memory), under that reference's lock;
/// - what goes up to a copy in the card's memory is numbered
///   ([`card_dirty`]), so that a merge can tell the title's bytes the card
///   had not received from bytes the card wrote.
pub(crate) fn relay() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default (stage 2, `crate::host::relay_exact`); `OATHKEEP_RELAY=0`
    // turns it off, `=1` asks for the first stage.
    // The crate's own tests run without it unless they ask: several of
    // them look at title memory right after a submission, which the
    // relay's landing thread may still be writing.
    *ON.get_or_init(|| std::env::var("OATHKEEP_RELAY").map_or(!cfg!(test), |v| v == "1" || v == "2") && !keeps())
}

/// The submission being prepared (its number among those started): what
/// [`Shadow::writing`] counts a declared range for ([`Flights`]).
pub(super) static PREPARED: AtomicU64 = AtomicU64::new(0);

/// A submission's preparing begins (`crate::host`, under the submission
/// lock).
pub(crate) fn preparing(number: u64) {
    PREPARED.store(number, Ordering::Relaxed);
}

/// With [`relay`]: the number of the last send of changed ranges to the
/// card (`crate::host`'s order of the queue) that every fetch still to be
/// merged came after. Notes of sends up to it are of no use any more.
pub(super) static RELAY_FETCHED: AtomicU64 = AtomicU64::new(0);

/// A fetch sent after send `number` is the oldest one that may still be
/// merged ([`RELAY_FETCHED`]).
pub(crate) fn relay_fetched(number: u64) {
    RELAY_FETCHED.fetch_max(number, Ordering::AcqRel);
}

/// With [`relay`]: how many submissions that are not copied back yet
/// declared each range shaders may be writing (range, count, the last
/// submission that declared it).
///
/// Without it a range declared by two flights was one entry, and the end
/// of the first flight's copy-back removed it while the second was still
/// writing: the next refresh would have put title bytes over what the
/// second flight's shaders were writing.
#[derive(Default)]
pub(super) struct RangeHasher(u64);

impl std::hash::Hasher for RangeHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

#[derive(Default)]
pub(super) struct Flights(
    pub(super) std::collections::HashMap<(usize, usize), (u32, u64), std::hash::BuildHasherDefault<RangeHasher>>,
);

impl Flights {
    /// Submission `number` declares `range`: counted once for it, however
    /// often it declares it.
    pub(super) fn declare(&mut self, range: (usize, usize), number: u64) {
        let f = self.0.entry(range).or_insert((0, number));
        if f.0 == 0 || f.1 != number {
            *f = (f.0 + 1, number);
        }
    }

    /// Decrements `ranges` and returns `None` when every active flight on this
    /// shadow has finished (`self.0.is_empty()`), letting `copy_back_done`
    /// clear `pending` in O(1) without allocating or sorting `free`.
    pub(super) fn done_or_clear(&mut self, ranges: &[(usize, usize)]) -> Option<Vec<(usize, usize)>> {
        if self.0.len() <= ranges.len() {
            let mut any_left = false;
            for range in ranges {
                if let Some(f) = self.0.get_mut(range) {
                    if f.0 > 1 {
                        f.0 -= 1;
                        any_left = true;
                    } else {
                        self.0.remove(range);
                    }
                }
            }
            if !any_left && self.0.is_empty() {
                return None;
            }
            // Fall through when another flight still holds at least one range:
            // any range removed above is no longer in `self.0`, so rebuild `free`
            // from `ranges` not present in `self.0`.
            let mut free: Vec<(usize, usize)> = ranges.iter().copied().filter(|r| !self.0.contains_key(r)).collect();
            free.sort_unstable();
            return Some(free);
        }
        Some(self.done(ranges))
    }

    /// One submission that declared `ranges` (each once) is copied back:
    /// the ranges no other submission still holds, in order.
    pub(super) fn done(&mut self, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let mut free = Vec::new();
        for range in ranges {
            match self.0.get_mut(range) {
                Some(f) if f.0 > 1 => f.0 -= 1,
                Some(_) => {
                    self.0.remove(range);
                    free.push(*range);
                }
                // Not counted (declared before anything counted): free.
                None => free.push(*range),
            }
        }
        free.sort_unstable();
        free
    }
}

/// The relay's merge of a stretch of a copy ([`relay`]). `now` is what the
/// card's memory holds, read once; `held` is the copy's reference for the
/// stretch (its snapshot, or the CPU's side of a copy in the card's
/// memory): what the card's memory held when it was last looked at;
/// `title` is title memory as just read. Every byte the card changed
/// (`now` differs from `held`) goes into `title` and into `held`; every
/// other byte of both stays as it is, so a byte the title wrote and the
/// card did not keeps the title's value. Whether `title` changed.
///
/// Nothing is stored into `now`'s source: the card may be writing it for a
/// later flight at this moment, and a store there, even of the value just
/// read, could undo that write. (The merge of a waited-for submission
/// stores every byte of a differing stretch back into the copy.)
pub(super) fn merge_piece(now: &[u8], held: &mut [u8], title: &mut [u8]) -> bool {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 is detected.
        return unsafe { merge_piece_avx2(now, held, title) };
    }
    merge_piece_scalar(now, held, title, 0)
}

pub(super) fn merge_piece_scalar(now: &[u8], held: &mut [u8], title: &mut [u8], from: usize) -> bool {
    let mut differs = 0u8;
    for ((n, h), t) in now[from..].iter().zip(held[from..].iter_mut()).zip(title[from..].iter_mut()) {
        let wrote = if *n != *h { 0xffu8 } else { 0 };
        let merged = (*t & !wrote) | (*n & wrote);
        differs |= merged ^ *t;
        *t = merged;
        *h = (*h & !wrote) | (*n & wrote);
    }
    differs != 0
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn merge_piece_avx2(now: &[u8], held: &mut [u8], title: &mut [u8]) -> bool {
    use std::arch::x86_64::*;
    let len = now.len().min(held.len()).min(title.len());
    let mut diff_acc = _mm256_setzero_si256();
    let mut i = 0;
    while i + 32 <= len {
        // SAFETY: `i + 32 <= len` for all three slices.
        unsafe {
            let vn = _mm256_loadu_si256(now.as_ptr().add(i).cast());
            let vh = _mm256_loadu_si256(held.as_ptr().add(i).cast());
            let same = _mm256_cmpeq_epi8(vn, vh);
            if _mm256_movemask_epi8(same) != -1 {
                let vt = _mm256_loadu_si256(title.as_ptr().add(i).cast());
                let merged = _mm256_blendv_epi8(vn, vt, same);
                diff_acc = _mm256_or_si256(diff_acc, _mm256_xor_si256(merged, vt));
                _mm256_storeu_si256(title.as_mut_ptr().add(i).cast(), merged);
                _mm256_storeu_si256(held.as_mut_ptr().add(i).cast(), vn);
            }
        }
        i += 32;
    }
    let vec_differs = _mm256_testz_si256(diff_acc, diff_acc) == 0;
    let tail_differs = merge_piece_scalar(&now[..len], &mut held[..len], &mut title[..len], i);
    vec_differs || tail_differs
}

/// Non-relay [`Shadow::card_merge`] inner merge: where `gpu` differs from `old`,
/// `gpu` wins; elsewhere `title` wins, and `old` takes the merged result (`*o = merged`).
pub(super) fn card_merge_bytes(gpu: &[u8], old: &mut [u8], title: &mut [u8]) -> bool {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 is detected.
        return unsafe { card_merge_bytes_avx2(gpu, old, title) };
    }
    card_merge_bytes_scalar(gpu, old, title, 0)
}

pub(super) fn card_merge_bytes_scalar(gpu: &[u8], old: &mut [u8], title: &mut [u8], from: usize) -> bool {
    let mut differs = 0u8;
    for ((t, g), o) in title[from..].iter_mut().zip(&gpu[from..]).zip(old[from..].iter_mut()) {
        let wrote = if *g != *o { 0xffu8 } else { 0 };
        let merged = (*t & !wrote) | (*g & wrote);
        differs |= merged ^ *t;
        *t = merged;
        *o = merged;
    }
    differs != 0
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn card_merge_bytes_avx2(gpu: &[u8], old: &mut [u8], title: &mut [u8]) -> bool {
    use std::arch::x86_64::*;
    let len = gpu.len().min(old.len()).min(title.len());
    let mut diff_acc = _mm256_setzero_si256();
    let mut i = 0;
    while i + 32 <= len {
        // SAFETY: `i + 32 <= len` for all three slices.
        unsafe {
            let vg = _mm256_loadu_si256(gpu.as_ptr().add(i).cast());
            let vo = _mm256_loadu_si256(old.as_ptr().add(i).cast());
            let vt = _mm256_loadu_si256(title.as_ptr().add(i).cast());
            let same = _mm256_cmpeq_epi8(vg, vo);
            let merged = _mm256_blendv_epi8(vg, vt, same);
            diff_acc = _mm256_or_si256(diff_acc, _mm256_xor_si256(merged, vt));
            _mm256_storeu_si256(title.as_mut_ptr().add(i).cast(), merged);
            _mm256_storeu_si256(old.as_mut_ptr().add(i).cast(), merged);
        }
        i += 32;
    }
    let vec_differs = _mm256_testz_si256(diff_acc, diff_acc) == 0;
    let tail_differs = card_merge_bytes_scalar(&gpu[..len], &mut old[..len], &mut title[..len], i);
    vec_differs || tail_differs
}

/// `[from, to)` without the ranges of `skip`, in order.
pub(super) fn outside(from: usize, to: usize, mut skip: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
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
