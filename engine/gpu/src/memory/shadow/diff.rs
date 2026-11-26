use std::sync::atomic::{AtomicU64, Ordering};

use super::pool::{first_difference, last_difference};

/// What [`Shadow::copy_back`] does with a piece that differs from the
/// snapshot: `host` is the copy's bytes there, `old` the snapshot's. From
/// the first byte that differs to the last, title memory is read into
/// `title` (scratch, sized to that stretch) by `read` (the stretch's offset
/// in the piece, where to), the GPU's bytes are put in and `host` takes the
/// result. The stretch's offset in the piece and its length if that changed
/// title memory; none if nothing differs or title memory is not there.
/// `runs` takes the runs of the stretch (offsets into `title`, lengths)
/// whose bytes are the GPU's and differ from the title's as read: only
/// those are to be written, so a title write that lands between the read
/// and the write elsewhere in the stretch stays (upstream Oathkeep #2009).
/// `mask` is scratch.
pub(super) fn back_piece(
    host: &mut [u8],
    old: &[u8],
    title: &mut Vec<u8>,
    mask: &mut Vec<u8>,
    runs: &mut Vec<(usize, usize)>,
    read: impl FnOnce(usize, &mut [u8]) -> bool,
) -> Option<(usize, usize)> {
    runs.clear();
    // From the first byte that differs to the last: title memory is read
    // and written there only (a shader that sets a few words of a piece
    // cost a read, a walk and a write of all 64 KiB of it).
    let first = first_difference(host, old)?;
    let last = last_difference(host, old).unwrap_or(first);
    let n = last + 1 - first;
    let (host, old) = (&mut host[first..=last], &old[first..=last]);
    title.clear();
    title.resize(n, 0);
    if !read(first, title) {
        return None;
    }
    // Title memory with the GPU's bytes put in; the copy takes the title's
    // bytes in turn.
    // Without a branch, so the loop runs many bytes at a time: the GPU's
    // byte where the copy's byte is not what it was before the submission
    // and the title left its byte as it was, else the title's. (With the
    // two searches above this was 44 ms a frame for 970 pieces.)
    // Where the shader and the title both changed a byte while the GPU ran,
    // the title's stays: the shader computed its byte from the copy's older
    // data (one cleared a bit of a record the title had already replaced,
    // and copying it back broke the title's object: an access violation
    // through 0xffffffff00000000; PalmVIDev e0b70476, Oathkeep f39c6c2).
    mask.clear();
    mask.resize(n, 0);
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 is detected, and `title`, `host`, `old`, and `mask` all have length `n`.
        unsafe { back_piece_avx2(title, host, old, mask, runs) };
        return (!runs.is_empty()).then_some((first, n));
    }
    let mut differs = 0u8;
    for (((t, h), o), m) in title.iter_mut().zip(host.iter_mut()).zip(old).zip(mask.iter_mut()) {
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

#[inline]
fn push_byte_run(runs: &mut Vec<(usize, usize)>, start: usize, len: usize) {
    match runs.last_mut() {
        Some((a, l)) if *a + *l == start => *l += len,
        _ => runs.push((start, len)),
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn back_piece_avx2(
    title: &mut [u8],
    host: &mut [u8],
    old: &[u8],
    mask: &mut [u8],
    runs: &mut Vec<(usize, usize)>,
) {
    use std::arch::x86_64::*;
    let n = title.len();
    let zero = _mm256_setzero_si256();
    let mut i = 0;
    while i + 32 <= n {
        // SAFETY: `i + 32 <= n` for all four slices.
        let mut diff = unsafe {
            let vt = _mm256_loadu_si256(title.as_ptr().add(i).cast());
            let vh = _mm256_loadu_si256(host.as_ptr().add(i).cast());
            let vo = _mm256_loadu_si256(old.as_ptr().add(i).cast());
            let h_eq_o = _mm256_cmpeq_epi8(vh, vo);
            let t_eq_o = _mm256_cmpeq_epi8(vt, vo);
            let gpu_mask = _mm256_andnot_si256(h_eq_o, t_eq_o);
            let merged = _mm256_blendv_epi8(vt, vh, gpu_mask);
            let vm = _mm256_xor_si256(merged, vt);
            _mm256_storeu_si256(title.as_mut_ptr().add(i).cast(), merged);
            _mm256_storeu_si256(host.as_mut_ptr().add(i).cast(), merged);
            _mm256_storeu_si256(mask.as_mut_ptr().add(i).cast(), vm);
            !(_mm256_movemask_epi8(_mm256_cmpeq_epi8(vm, zero)) as u32)
        };
        while diff != 0 {
            let first = diff.trailing_zeros();
            let count = (diff >> first).trailing_ones();
            push_byte_run(runs, i + first as usize, count as usize);
            if first + count >= 32 {
                break;
            }
            diff &= u32::MAX << (first + count);
        }
        i += 32;
    }
    let tail_start = i;
    for k in tail_start..n {
        let gpu = if (host[k] != old[k]) & (title[k] == old[k]) { 0xffu8 } else { 0 };
        let merged = (title[k] & !gpu) | (host[k] & gpu);
        mask[k] = merged ^ title[k];
        title[k] = merged;
        host[k] = merged;
    }
    while i < n {
        while i < n && mask[i] == 0 {
            i += 1;
        }
        let start = i;
        while i < n && mask[i] != 0 {
            i += 1;
        }
        if i > start {
            push_byte_run(runs, start, i - start);
        }
    }
}

/// Since the last frame-rate line, of [`Shadow::merge_under_writes`]: pages
/// compared, changed runs put in, and their bytes.
pub(super) static MERGED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Why [`Shadow::merge_under_writes`] last left the caller to wait.
static STALE_WHY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(super) fn note_stale(why: usize) {
    STALE_WHY.store(why, Ordering::Relaxed);
}

/// [`STALE_WHY`] in words (diagnostics).
#[cfg(feature = "diagnostics")]
pub(crate) fn stale_why() -> &'static str {
    const WHY: [&str; 4] = [
        "the memory is not watched",
        "the journal does not say what was written",
        "more changed runs than a submission sends up",
        "no snapshot",
    ];
    WHY[STALE_WHY.load(Ordering::Relaxed).min(3)]
}

/// [`MERGED`], taken.
pub(crate) fn merged_stats() -> [u64; 3] {
    [0, 1, 2].map(|i| MERGED[i].swap(0, Ordering::Relaxed))
}

/// The runs of bytes where `now` differs from `was` (of one length), as
/// (offset, bytes) in order: every differing byte is in a run and no equal
/// byte is.
pub(super) fn changed_runs(now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 is detected.
        return unsafe { changed_runs_avx2(now, was) };
    }
    changed_runs_scalar(now, was)
}

pub(super) fn changed_runs_scalar(now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
    let n = now.len().min(was.len());
    let mut runs = Vec::new();
    let mut i = 0;
    while i < n {
        // Equal bytes, many at a time while they last (a page looked at
        // under a flight's writes hardly ever differs at all).
        while i + 512 <= n && now[i..i + 512] == was[i..i + 512] {
            i += 512;
        }
        while i + 8 <= n && now[i..i + 8] == was[i..i + 8] {
            i += 8;
        }
        while i < n && now[i] == was[i] {
            i += 1;
        }
        if i >= n {
            break;
        }
        let first = i;
        while i < n && now[i] != was[i] {
            i += 1;
        }
        runs.push((first, i - first));
    }
    runs
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn changed_runs_avx2(now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
    use std::arch::x86_64::*;
    let n = now.len().min(was.len());
    let mut runs = Vec::new();
    let mut i = 0;
    while i + 256 <= n {
        // SAFETY: `i + 256 <= n` in both slices.
        let equal_256 = unsafe {
            let mut any = _mm256_setzero_si256();
            for k in 0..8 {
                let x = _mm256_loadu_si256(now.as_ptr().add(i + 32 * k).cast());
                let y = _mm256_loadu_si256(was.as_ptr().add(i + 32 * k).cast());
                any = _mm256_or_si256(any, _mm256_xor_si256(x, y));
            }
            _mm256_testz_si256(any, any) != 0
        };
        if equal_256 {
            i += 256;
            continue;
        }
        for _ in 0..8 {
            // SAFETY: `i + 32 <= n` across all 8 sub-blocks.
            let mut diff = unsafe {
                let x = _mm256_loadu_si256(now.as_ptr().add(i).cast());
                let y = _mm256_loadu_si256(was.as_ptr().add(i).cast());
                !(_mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32)
            };
            while diff != 0 {
                let first = diff.trailing_zeros();
                let count = (diff >> first).trailing_ones();
                push_byte_run(&mut runs, i + first as usize, count as usize);
                if first + count >= 32 {
                    break;
                }
                diff &= u32::MAX << (first + count);
            }
            i += 32;
        }
    }
    while i + 32 <= n {
        // SAFETY: `i + 32 <= n` in both slices.
        let mut diff = unsafe {
            let x = _mm256_loadu_si256(now.as_ptr().add(i).cast());
            let y = _mm256_loadu_si256(was.as_ptr().add(i).cast());
            !(_mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32)
        };
        while diff != 0 {
            let first = diff.trailing_zeros();
            let count = (diff >> first).trailing_ones();
            push_byte_run(&mut runs, i + first as usize, count as usize);
            if first + count >= 32 {
                break;
            }
            diff &= u32::MAX << (first + count);
        }
        i += 32;
    }
    while i < n {
        while i < n && now[i] == was[i] {
            i += 1;
        }
        if i >= n {
            break;
        }
        let first = i;
        while i < n && now[i] != was[i] {
            i += 1;
        }
        push_byte_run(&mut runs, first, i - first);
    }
    runs
}
