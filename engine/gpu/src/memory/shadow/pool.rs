use std::sync::atomic::AtomicU64;
use std::sync::Mutex;
use crate::memory::watch::PAGE;

use super::PIECE;

pub(super) struct SnapshotPool {
    pub(super) bufs: Vec<Vec<u8>>,
    pub(super) bytes: usize,
}

/// Snapshot memory kept for reuse (already touched, so no page faults).
pub(super) static POOL: Mutex<SnapshotPool> = Mutex::new(SnapshotPool {
    bufs: Vec::new(),
    bytes: 0,
});

/// Resident snapshot bytes kept pinned directly on active [`Shadow`]s across
/// submissions so `Shadow::writing` and `take_snapshots` avoid `POOL` mutex
/// churn, 1024-element sorts, and Windows demand-zero page faults.
pub(super) static PINNED_SNAP_BYTES: AtomicU64 = AtomicU64::new(0);

pub(super) fn pinned_snap_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        let default_mb = if crate::memory::residency::tight_host_ram() { 32 } else { 64 };
        std::env::var("OATHKEEP_PINNED_SNAPSHOT_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(default_mb)
            << 20
    })
}

/// An empty buffer with room for `n` bytes, from the pool if it has one
/// of about that size: a small copy's snapshot took the 174 MB buffer kept
/// for the big heap's, which then got another, until eight of them were
/// reserved (1.4 GB in run 24).
pub(super) fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let most = n.saturating_mul(4).max(1 << 20);
    let fit = pool
        .bufs
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n && v.capacity() <= most)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => {
            let v = pool.bufs.swap_remove(i);
            pool.bytes = pool.bytes.saturating_sub(v.capacity());
            v
        }
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

/// How many snapshot buffers are kept (`OATHKEEP_SNAPSHOT_POOL`; `8`: as
/// before).
///
/// A frame gives some 330 ranges of copies in the PC's memory to shaders, and
/// most are of copies of their own; with two or three frames in the air eight
/// buffers were the eight largest, and every other snapshot was memory fresh
/// from the kernel and given back to it: 11 % of the submitting thread in the
/// opening (profile cpu-1: `NtAllocateVirtualMemory` under
/// [`Shadow::writing`]), and the page faults of filling it.
fn snapshots_kept() -> usize {
    static MOST: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| std::env::var("OATHKEEP_SNAPSHOT_POOL").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1024))
}

fn snapshot_pool_budget() -> usize {
    if crate::memory::residency::critical_commit() {
        0
    } else if crate::memory::residency::pressured() {
        16 << 20
    } else if crate::memory::residency::tight_host_ram() {
        64 << 20
    } else {
        192 << 20
    }
}

/// Drops pooled snapshot buffers down to `snapshot_pool_budget()` under host
/// RAM / commit pressure, releasing committed pages back to Windows outside
/// the pool lock.
pub(super) fn trim_snapshot_pool() {
    let budget = snapshot_pool_budget();
    let gone = {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        if pool.bytes <= budget {
            return;
        }
        if budget == 0 {
            pool.bytes = 0;
            std::mem::take(&mut pool.bufs)
        } else {
            pool.bufs.sort_unstable_by_key(|v| std::cmp::Reverse(v.capacity()));
            let (mut kept, mut total) = (0, 0usize);
            for v in &pool.bufs {
                let next = total.saturating_add(v.capacity());
                if next > budget {
                    break;
                }
                total = next;
                kept += 1;
            }
            pool.bytes = total;
            pool.bufs.split_off(kept)
        }
    };
    drop(gone);
}

/// Returns a snapshot buffer to the pool: O(1) push in steady state, sorting
/// and trimming only when count or total capacity exceeds the pool budget.
pub(super) fn give_back(v: Vec<u8>) {
    let cap = v.capacity();
    if cap == 0 {
        return;
    }
    let kept_budget = snapshot_pool_budget();
    if kept_budget == 0 {
        drop(v);
        return;
    }
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.bytes = pool.bytes.saturating_add(cap);
    pool.bufs.push(v);
    let limit = snapshots_kept();
    if pool.bufs.len() <= limit && (pool.bytes <= kept_budget || pool.bufs.len() == 1) {
        return;
    }
    pool.bufs.sort_unstable_by_key(|v| std::cmp::Reverse(v.capacity()));
    let (mut kept, mut total) = (0, 0usize);
    for v in pool.bufs.iter().take(limit) {
        let next = total.saturating_add(v.capacity());
        if kept > 0 && next > kept_budget {
            break;
        }
        total = next;
        kept += 1;
    }
    pool.bytes = total;
    // (Given back to the kernel without the lock: milliseconds.)
    let gone = pool.bufs.split_off(kept);
    drop(pool);
    drop(gone);
}

// Fixed-size equality compiles to two SIMD comparisons on Windows x64.
// Only the first or last unequal chunk needs a byte-wise search.
const DIFF_CHUNK: usize = 32;

/// Where `a` and `b` first differ within their common length.
pub(super) fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    let len = a.len().min(b.len());
    let (a_chunks, a_tail) = a[..len].as_chunks::<DIFF_CHUNK>();
    let (b_chunks, b_tail) = b[..len].as_chunks::<DIFF_CHUNK>();
    let (offset, a, b) = match a_chunks.iter().zip(b_chunks).position(|(x, y)| x != y) {
        Some(i) => (i * DIFF_CHUNK, &a_chunks[i][..], &b_chunks[i][..]),
        None => (len - a_tail.len(), a_tail, b_tail),
    };
    a.iter().zip(b).position(|(x, y)| x != y).map(|i| offset + i)
}

/// Where `a` and `b` last differ within their common length.
pub(super) fn last_difference(a: &[u8], b: &[u8]) -> Option<usize> {
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

/// Calls `f` with `[from, to)` cut at multiples of [`PIECE`].
pub(super) fn for_pieces_of(from: usize, to: usize, mut f: impl FnMut(usize, usize)) {
    let mut at = from;
    while at < to {
        let next = ((at / PIECE + 1) * PIECE).min(to);
        f(at, next);
        at = next;
    }
}

/// Whether a copy's snapshot is kept from one submission to the next and
/// only the pieces that changed in between are taken again
/// (`OATHKEEP_KEEP_SNAPSHOTS=1`; off by default).
///
/// A snapshot is the copy's bytes before a submission, to find what its
/// shaders wrote. Taken anew for every submission it is 1,050 MB of copying
/// a frame (72-86 ms in gameplay, runs 61 and 62) for buffers of which
/// 100 MB changed. Kept, a piece of it is taken again only after something
/// changed the copy there: a refresh from title memory ([`Shadow::copy`]),
/// shader writes found after a submission, or the bytes put back then
/// ([`Shadow::copy_back`]).
///
/// What it assumes, and why it is not on by default: that nothing else
/// writes a copy. A shader that writes a buffer through a view we did not
/// declare as written (we declare the unordered-access views of shaders
/// that can store) changes the copy unseen; with a fresh snapshot that
/// write is simply never put back, with a kept one it would be taken for a
/// write of a later submission and put back then, over whatever the title
/// wrote there since.
pub(super) fn keeps() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_KEEP_SNAPSHOTS").is_some_and(|v| v == "1"))
}

/// Bytes of the snapshots kept, and the most that may be.
pub(super) static KEPT_BYTES: AtomicU64 = AtomicU64::new(0);
/// `OATHKEEP_KEEP_SNAPSHOTS_MB`, 256 unless set (it was 768: run 80, with
/// kept snapshots beside copies in the card's memory, ran the PC out of
/// memory in 27 s; kept snapshots are the PC's memory, all of it resident).
pub(super) fn kept_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_KEEP_SNAPSHOTS_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(256) << 20
    })
}

/// The 64 KiB pieces, cut at multiples of 64 KiB of title memory, that
/// hold offsets `[from, to)` of a copy of memory from `start` on
/// (`from < to`); piece 0 holds offset 0.
pub(super) fn pieces_of(start: usize, from: usize, to: usize) -> std::ops::Range<usize> {
    let first = start / PIECE;
    (start + from) / PIECE - first..(start + to - 1) / PIECE - first + 1
}

/// The offsets that piece `piece` holds of a copy of `len` bytes of memory
/// from `start` on.
pub(super) fn piece_at(start: usize, len: usize, piece: usize) -> (usize, usize) {
    let first = start / PIECE;
    let lo = ((first + piece) * PIECE).saturating_sub(start);
    let hi = ((first + piece + 1) * PIECE - start).min(len);
    (lo, hi)
}

/// The pages of `[a, e)` (title addresses, `a < e`) that a copy whose
/// first page is `base` must take from title memory before it holds the
/// title's bytes: those it never took (`copied` 0), whether or not the
/// title wrote them since tracking began, and those `written` (as
/// `crate::memory::watch::writes` gives it, by page in order) shows written after
/// the copy took them. As [`Shadow::copy_stale`] decides it.
///
/// [`Shadow::stale_under_writes`] and [`Shadow::merge_under_writes`] looked
/// at `written` alone, so a page of a range shaders may be writing that the
/// copy never took, and the title had not written lately, was neither
/// merged nor a reason to settle; the refresh leaves such ranges alone, and
/// shaders read zeros there (fxceless/victory-fork#206: missing geometry
/// with `OATHKEEP_LATE_WAIT`/`OATHKEEP_RELAY`).
pub(super) fn stale_pages_in(base: usize, copied: &[u64], a: usize, e: usize, written: &[(usize, u64)]) -> Vec<usize> {
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

/// `runs` (from, to, whether the copy held it before), cut in those of pages
/// held before (`pages`: page number and asking, in order) to the parts of
/// each page that were written since (`parts`: a bit for each 4 KiB).
pub(super) fn by_parts(runs: Vec<(usize, usize, bool)>, pages: &[(usize, u64)], parts: &[u16]) -> Vec<(usize, usize, bool)> {
    const PART: usize = PAGE / 16;
    if parts.iter().all(|&p| p == u16::MAX) {
        return runs;
    }
    let mut out: Vec<(usize, usize, bool)> = Vec::with_capacity(runs.len() + pages.len());
    let mut pages = pages.iter().zip(parts).peekable();
    for (from, to, known) in runs {
        if !known {
            out.push((from, to, known));
            continue;
        }
        while let Some((&(page, _), &parts)) = pages.next_if(|((page, _), _)| page * PAGE < to) {
            for k in (0..16).filter(|k| parts >> k & 1 != 0) {
                let (a, b) = ((page * PAGE + k * PART).max(from), (page * PAGE + (k + 1) * PART).min(to));
                if a >= b {
                    continue;
                }
                match out.last_mut() {
                    Some((_, end, true)) if *end == a => *end = b,
                    _ => out.push((a, b, true)),
                }
            }
        }
    }
    out
}
