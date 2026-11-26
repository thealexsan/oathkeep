use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{PIECE, Shadow};
#[cfg(test)]
use super::wide::Job;

/// About this many bytes to a piece of work for [`wide`].
#[cfg(test)]
pub(super) const WIDE_JOB: usize = 1 << 20;
/// Granularity for parallel snapshot copies and piece diff scans (256 KiB = 4
/// 64 KiB pieces), giving balanced work distribution across 6 threads even on
/// 1–4 MiB per-submission ranges.
pub(super) const SNAP_CHUNK: usize = 256 << 10;
/// Target byte budget when grouping small coalesced snapshot copies into one
/// parallel work item so hundreds of tiny UAV ranges do not contend on atomic cursors.
pub(super) const SNAP_BATCH: usize = 128 << 10;

/// With [`wide_back`]: the snapshots noted while a submission is prepared
/// (from, to, bytes), taken by [`take_snapshots`].
pub(super) static SNAPS: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());

/// Reusable scratch buffers for [`take_snapshots`] so `SNAPS` keeps its
/// capacity and per-submission chunking/batching allocates nothing in steady state.
#[allow(clippy::type_complexity)]
static SNAP_SCRATCH: Mutex<(Vec<(usize, usize, usize)>, Vec<(usize, usize, usize)>, Vec<std::ops::Range<usize>>)> =
    Mutex::new((Vec::new(), Vec::new(), Vec::new()));

/// Deduplicates overlapping snapshot/copy ranges sharing the same `(src - dst)`
/// mapping delta and coalesces touching ranges into single contiguous copies.
pub(super) fn coalesce_copy_ranges(copies: &mut Vec<(usize, usize, usize)>) {
    copies.retain(|c| c.2 > 0);
    if copies.len() <= 1 {
        return;
    }
    copies.sort_unstable_by_key(|&(src, dst, _)| (dst, src));
    let mut w = 0usize;
    for i in 1..copies.len() {
        let (src, dst, len) = copies[i];
        let (psrc, pdst, plen) = copies[w];
        if psrc.wrapping_sub(pdst) == src.wrapping_sub(dst) && dst <= pdst + plen {
            let end = (pdst + plen).max(dst + len);
            copies[w].2 = end - pdst;
        } else {
            w += 1;
            copies[w] = (src, dst, len);
        }
    }
    copies.truncate(w + 1);
}

/// Splits `(src, dst, len)` copies into chunks of at most `chunk_bytes` so no
/// single range monopolizes one thread while helpers sit idle.
pub(super) fn split_copy_chunks(copies: Vec<(usize, usize, usize)>, chunk_bytes: usize) -> Vec<(usize, usize, usize)> {
    let mut chunks = Vec::with_capacity(copies.len());
    split_copy_chunks_into(&copies, chunk_bytes, &mut chunks);
    chunks
}

fn split_copy_chunks_into(
    copies: &[(usize, usize, usize)],
    chunk_bytes: usize,
    chunks: &mut Vec<(usize, usize, usize)>,
) {
    chunks.clear();
    for &(src, dst, len) in copies {
        let mut at = 0;
        while at < len {
            let n = chunk_bytes.min(len - at);
            chunks.push((src + at, dst + at, n));
            at += n;
        }
    }
}

/// Packs `chunks` into balanced index ranges of roughly `batch_bytes` each.
pub(super) fn pack_copy_batches(chunks: &[(usize, usize, usize)], batch_bytes: usize) -> Vec<std::ops::Range<usize>> {
    let mut batches = Vec::new();
    pack_copy_batches_into(chunks, batch_bytes, &mut batches);
    batches
}

fn pack_copy_batches_into(
    chunks: &[(usize, usize, usize)],
    batch_bytes: usize,
    batches: &mut Vec<std::ops::Range<usize>>,
) {
    batches.clear();
    let (mut start, mut bytes) = (0usize, 0usize);
    for (i, &(_, _, len)) in chunks.iter().enumerate() {
        if bytes > 0 && bytes + len > batch_bytes && i > start {
            batches.push(start..i);
            start = i;
            bytes = 0;
        }
        bytes += len;
        if bytes >= batch_bytes {
            batches.push(start..i + 1);
            start = i + 1;
            bytes = 0;
        }
    }
    if start < chunks.len() {
        batches.push(start..chunks.len());
    }
}

/// With [`wide_back`], when a submission's refreshes are done and before
/// its lists go to the card: takes the snapshots [`Shadow::writing`] noted.
/// Coalesces overlapping/adjacent ranges, reuses scratch buffers, copies small
/// totals (`<= 128 KiB`) inline, and fans larger totals out across
/// [`crate::memory::writeback_pool::each_under`] with zero per-submission heap allocations.
pub(crate) fn take_snapshots() {
    let mut scratch = SNAP_SCRATCH.lock().unwrap_or_else(|e| e.into_inner());
    let (raw, chunks, batches) = &mut *scratch;
    raw.clear();
    {
        let mut snaps = SNAPS.lock().unwrap_or_else(|e| e.into_inner());
        if snaps.is_empty() {
            return;
        }
        std::mem::swap(&mut *snaps, raw);
    }
    coalesce_copy_ranges(raw);
    let total_bytes: usize = raw.iter().map(|s| s.2).sum();
    if total_bytes <= 128 << 10 {
        for &(src, dst, len) in raw.iter() {
            // SAFETY: non-overlapping host copy mapping and snapshot buffer.
            unsafe { std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, len) };
        }
        return;
    }
    split_copy_chunks_into(raw, SNAP_CHUNK, chunks);
    pack_copy_batches_into(chunks, SNAP_BATCH, batches);
    let chunks_ref: &[(usize, usize, usize)] = chunks;
    let batches_ref: &[std::ops::Range<usize>] = batches;
    crate::memory::writeback_pool::each_under(batches_ref.len(), &|b| {
        for &(src, dst, len) in &chunks_ref[batches_ref[b].clone()] {
            // SAFETY: non-overlapping chunk of host copy mapping and snapshot buffer.
            unsafe { std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, len) };
        }
    });
}

/// `copies` (from, to, bytes) as pieces of work of about [`WIDE_JOB`]
/// bytes each: long ones cut, short ones together.
#[cfg(test)]
pub(super) fn copy_jobs(copies: Vec<(usize, usize, usize)>) -> Vec<Job> {
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

/// With [`wide_back`]: for each of `work` (a copy not in the card's memory
/// and a range `[address, end)` its shaders may have written, declared and
/// not yet done), the 64 KiB pieces that differ from the snapshot, as
/// [`Shadow::copy_back_with`] takes them; found on several threads.
pub(crate) fn find_all(work: &[(Arc<Shadow>, usize, usize)]) -> Vec<Vec<(usize, usize)>> {
    let scans = work
        .iter()
        .map(|(shadow, address, end)| {
            let (from, to) = shadow.inside(*address, end.saturating_sub(*address));
            // The snapshot's memory stays until the range is done with
            // (`copy_back_done`), which comes after this.
            match shadow.before.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                Some(before) => (shadow.data as usize, before.as_ptr() as usize, shadow.start as usize, from, to),
                None => (0, 0, 0, 0, 0),
            }
        })
        .collect();
    find_pieces(scans)
}

/// For each of `scans` (where a copy's bytes are, where its snapshot's
/// are, the title address the copy starts at, offsets from and to): the
/// 64 KiB pieces (cut at multiples of 64 KiB of title memory, as
/// [`Shadow::differing`] cuts them) of `[from, to)` whose bytes differ, as
/// (offset, bytes) in order.
///
/// Dispatched on [`crate::memory::writeback_pool::each`] (idle during `SCAN_BESIDE` and `SCAN`)
/// rather than [`wide`], so background relay diff scans never queue ahead of
/// or get stolen by the submitting thread's `take_snapshots` / refreshes, and
/// using [`crate::memory::safecopy::differ`] (4x unrolled AVX2, safe under concurrent
/// GPU writes) with zero per-job `Box` allocations or result sorting.
pub(super) fn find_pieces(scans: Vec<(usize, usize, usize, usize, usize)>) -> Vec<Vec<(usize, usize)>> {
    type Part = (usize, usize, usize, usize, usize, usize);
    let count = scans.len();
    let mut parts: Vec<Part> = Vec::new();
    for (which, (data, before, start, from, to)) in scans.into_iter().enumerate() {
        let mut at = from;
        while at < to {
            // Cut where pieces end, in ~256 KiB chunks (4 pieces per chunk).
            let mut next = (at + SNAP_CHUNK).min(to);
            if next < to {
                next -= (start + next) % PIECE;
                if next <= at {
                    next = (at + PIECE - (start + at) % PIECE).min(to);
                }
            }
            parts.push((data, before, start, at, next, which));
            at = next;
        }
    }
    if parts.is_empty() {
        return vec![Vec::new(); count];
    }
    let results: Vec<std::sync::OnceLock<Vec<(usize, usize)>>> =
        (0..parts.len()).map(|_| std::sync::OnceLock::new()).collect();
    crate::memory::writeback_pool::each(parts.len(), &|idx| {
        let (data, before, start, from, to, _) = parts[idx];
        let mut pieces = Vec::new();
        let mut at = from;
        while at < to {
            let n = (PIECE - (start + at) % PIECE).min(to - at);
            // SAFETY: inside the copy's mapping and its snapshot; `differ`
            // tolerates concurrent card writes under `OATHKEEP_RELAY`.
            if unsafe { crate::memory::safecopy::differ((data + at) as *const u8, (before + at) as *const u8, n) } {
                pieces.push((at, n));
            }
            at += n;
        }
        let _ = results[idx].set(pieces);
    });
    let mut out = vec![Vec::new(); count];
    for (part, res) in parts.into_iter().zip(results) {
        if let Some(pieces) = res.into_inner() {
            out[part.5].extend(pieces);
        }
    }
    out
}

/// Since the last frame-rate line, of the shader-written ranges looked for
/// on the CPU (copies not in the card's memory): bytes snapshotted in
/// windows outside buffers and in title buffers' copies; bytes compared in
/// windows, in buffers' copies, and of those in copies of 1 MiB or more;
/// ranges compared in windows and in buffers' copies.
pub(super) static BACK_CLASS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// For the frame-rate line: where the CPU's search for shader writes goes
/// (per frame). A copy of a title buffer can go to the card's memory,
/// where the card does the search (`OATHKEEP_CARD_BUFFERS_MIN_MB`); a
/// window outside buffers cannot as the code stands.
pub(crate) fn back_line(frames: f64) {
    let c: Vec<f64> = BACK_CLASS.iter().map(|c| c.swap(0, Ordering::Relaxed) as f64 / frames).collect();
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
