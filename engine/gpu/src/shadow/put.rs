use std::sync::atomic::{AtomicU64, Ordering};

use super::diff::changed_runs;
#[cfg(test)]
use super::pool::{first_difference, last_difference};
#[cfg(test)]
use super::relay::merge_piece;

/// Whether a refresh looks if what counts as written is only what a landing
/// put there, and then sends nothing of it up (`OATHKEEP_LAND_ECHO=0`: it copies
/// and sends it, as before).
///
/// What shaders wrote comes down, goes into title memory and into the CPU's
/// side of the copy alike. In memory the kernel watches that is a write like
/// any other (it cannot be told as the guards' blocks can, `crate::land`), and
/// the next refresh of it copied the pages and sent them up again: in the dorm
/// 14.4 MB a frame of one copy of 90 MB whose 263 pieces were all fetched and
/// all sent, 28 of 72 MB over the bus in all (run submit-2). Only pages a
/// landing was noted in are compared, 4 KiB at a time: a page that is not the
/// same goes up whole, as before, and so does every other.
pub(super) fn land_echo() -> bool {
    crate::toggled!("OATHKEEP_LAND_ECHO")
}

/// Since the last frame-rate line: bytes found to be a landing's alone, and
/// bytes a landing was noted in that were the title's too.
pub(crate) static ECHO: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether a refresh of a copy in the card's memory takes, and sends up,
/// the dwords that title memory changed and no others
/// (`OATHKEEP_SEND_CHANGED=1`; off: the 64 KiB pages written since it last
/// took them, whole). Off since run tile1-0: the room lost its indirect light
/// and geometry exploded; run bis1-0, the same but for this, was right.
///
/// A page that counts as written is mostly not the title's doing, or not all
/// of it: a word the title wrote, and around it what shaders wrote and the
/// title never touched; or only what the relay put into title memory, which
/// the card and the CPU's side hold already ([`Shadow::card_merge_relay`]).
/// Sent up whole, such a page goes over whatever the card holds there that
/// did not come back yet, or never will (a write through a view nobody
/// declared): what stood on the card, newer, was gone.
///
/// What it rests on: the CPU's side is what the card's side was last known
/// to hold, for every range no shader may be writing (no refresh touches
/// the others). So a dword goes up only if title memory holds another value
/// of it than the CPU's side, which then holds it too; title memory is read
/// once, what is compared is what is stored and sent. Dwords, not bytes:
/// no shader writes less, and the title's bytes beside a changed one in its
/// dword are those the CPU's side holds.
///
/// A page never taken goes up whole as long as no shader's write was ever
/// declared for the copy. After that it is compared as any other: the CPU's
/// side and the card's began as zeros, both.
///
/// What it cannot know: that the title wrote a value over the same value.
/// Where a shader wrote in between, through a view nobody declared, the
/// shader's stays.
pub(super) fn send_changed() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        #[cfg(feature = "diagnostics")]
        if crate::copystat::mode() != 0 {
            return false;
        }
        std::env::var_os("OATHKEEP_SEND_CHANGED").is_some_and(|v| v != "0")
    })
}

/// Since the last frame-rate line, of pages taken into copies in the
/// card's memory ([`send_changed`]): bytes compared, those of pages that
/// held the same, bytes to go up, and the runs they are.
pub(super) static SEND: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// [`SEND`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn send_stats() -> [u64; 4] {
    [0, 1, 2, 3].map(|i| SEND[i].swap(0, Ordering::Relaxed))
}

/// Whether what shaders wrote goes into title memory run by run
/// ([`put_changed`]; on by default, `OATHKEEP_EXACT_BACK=0` writes the whole
/// stretch from the first changed byte to the last, as before).
pub(crate) fn exact_back() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_EXACT_BACK").is_none_or(|v| v != "0"))
}

/// Since the last frame-rate line: pieces put back by [`put_changed`], the
/// writes they took, the bytes written, and the pieces whose runs were
/// joined over bytes that had not changed.
pub(crate) static PUT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The dwords (of title memory from `at` on) where `merged` differs from
/// `title`, as (offset, bytes) in order, joined where at most `gap` bytes
/// lie between.
pub(super) fn dword_runs(at: usize, title: &[u8], merged: &[u8], gap: usize) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (from, n) in changed_runs(merged, title) {
        // Shaders write dwords: the whole of each.
        let lo = ((at + from) & !3).max(at) - at;
        let hi = ((at + from + n + 3) & !3).min(at + title.len()) - at;
        match runs.last_mut() {
            Some((a, len)) if lo <= *a + *len + gap => *len = (*len).max(hi - *a),
            _ => runs.push((lo, hi - lo)),
        }
    }
    runs
}

/// [`dword_runs`] with no gap, the fast way where it can be: a frame compares
/// 12 MB this way and finds 80,000 runs.
pub(super) fn word_runs(at: usize, title: &[u8], held: &[u8]) -> Vec<(usize, usize)> {
    #[cfg(target_arch = "x86_64")]
    if at % 4 == 0 && title.len() % 4 == 0 && title.len() == held.len() && std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: the processor has AVX2, and the lengths are the same.
        return unsafe { word_runs_avx2(title, held) };
    }
    dword_runs(at, title, held, 0)
}

/// # Safety
/// AVX2, and two slices of one length, whole words.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn word_runs_avx2(title: &[u8], held: &[u8]) -> Vec<(usize, usize)> {
    use std::arch::x86_64::*;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut push = |from: usize, len: usize| match runs.last_mut() {
        Some((a, n)) if *a + *n == from => *n += len,
        _ => runs.push((from, len)),
    };
    let n = title.len();
    let mut i = 0;
    while i + 256 <= n {
        let mut differ = 0u64;
        for k in 0..8 {
            // SAFETY: inside both slices (`i + 256 <= n`).
            let same = unsafe {
                _mm256_cmpeq_epi32(
                    _mm256_loadu_si256(title.as_ptr().add(i + 32 * k).cast()),
                    _mm256_loadu_si256(held.as_ptr().add(i + 32 * k).cast()),
                )
            };
            differ |= u64::from(!_mm256_movemask_ps(_mm256_castsi256_ps(same)) as u8) << (8 * k);
        }
        while differ != 0 {
            let first = differ.trailing_zeros();
            let count = (differ >> first).trailing_ones();
            push(i + 4 * first as usize, 4 * count as usize);
            if first + count >= 64 {
                break;
            }
            differ &= u64::MAX << (first + count);
        }
        i += 256;
    }
    while i + 4 <= n {
        if title[i..i + 4] != held[i..i + 4] {
            push(i, 4);
        }
        i += 4;
    }
    runs
}

/// Whether the pages of [`Shadow::merge_under_writes`] are compared on several
/// threads (`OATHKEEP_UNDER_WIDE=0`: on the submitting one).
pub(super) fn under_wide() -> bool {
    crate::toggled!("OATHKEEP_UNDER_WIDE")
}

/// What shaders wrote goes into title memory from `at` on: `title` is what
/// was read there, `merged` the same with their bytes put in. Only the
/// dwords that differ are written. The whole stretch from the first changed
/// byte to the last was: between the read and the write the title's own
/// threads, or data streamed in, may have written the bytes between (a heap
/// holds what shaders write beside what the CPU does), and those writes were
/// undone. Beside running title threads (the relay) that is a piece of
/// memory going back in time now and then: runs pool1, blame1 and seriess2
/// ended in the title decoding garbage.
///
/// shortcut: a piece with very many runs has them joined over small gaps,
/// where the old race is left.
pub(super) fn put_changed(at: usize, title: &[u8], merged: &[u8]) -> usize {
    crate::watch::landing(at, title.len());
    if !exact_back() {
        crate::host::write_mapped(at as *mut u8, merged.as_ptr(), merged.len());
        return merged.len();
    }
    if crate::safecopy::on() {
        PUT[0].fetch_add(1, Ordering::Relaxed);
        PUT[2].fetch_add(merged.len() as u64, Ordering::Relaxed);
        let since = crate::watch::changes();
        // SAFETY: title memory; a page of it that is not there fails the
        // put, and the rest is put page by page.
        if unsafe { crate::safecopy::put_changed(at as *mut u8, title, merged) } {
            return merged.len();
        }
        let mut from = 0;
        while from < merged.len() {
            let n = (4096 - (at + from) % 4096).min(merged.len() - from);
            let (was, now) = (&title[from..from + n], &merged[from..from + n]);
            // SAFETY: as above.
            let put = || unsafe { crate::safecopy::put_changed((at + from) as *mut u8, was, now) };
            // A page that is away only while the runtime changes a mapping
            // beside it gets its bytes when that is done.
            if !put() && !crate::watch::again_after_change(since, put) {
                crate::watch::lost("what shaders wrote", at + from, n);
            }
            from += n;
        }
        return merged.len();
    }
    let mut runs = dword_runs(at, title, merged, 0);
    if runs.len() > 64 {
        runs = dword_runs(at, title, merged, 32);
        PUT[3].fetch_add(1, Ordering::Relaxed);
    }
    let mut written = 0;
    for &(from, n) in &runs {
        crate::host::write_mapped((at + from) as *mut u8, merged[from..].as_ptr(), n);
        written += n;
    }
    PUT[0].fetch_add(1, Ordering::Relaxed);
    PUT[1].fetch_add(runs.len() as u64, Ordering::Relaxed);
    PUT[2].fetch_add(written as u64, Ordering::Relaxed);
    written
}

/// For `crate::land`'s measurement: a piece as [`Shadow::card_merge_relay`]
/// lands it without `OATHKEEP_LAND`: `n` bytes at `now`, `held` what the CPU's
/// side held, both the caller's alone.
#[cfg(test)]
pub(crate) fn land_as_before(start: usize, now: usize, held: usize, n: usize, title: &mut Vec<u8>) {
    // SAFETY: the caller's.
    let (gpu, held) = unsafe { (std::slice::from_raw_parts(now as *const u8, n), std::slice::from_raw_parts_mut(held as *mut u8, n)) };
    let Some(first) = first_difference(gpu, held) else {
        return;
    };
    let last = last_difference(gpu, held).unwrap_or(first);
    let n = last + 1 - first;
    title.clear();
    title.resize(n, 0);
    assert!(crate::host::read_into((start + first) as *const u8, title));
    let was = title.clone();
    if merge_piece(&gpu[first..=last], &mut held[first..=last], title) {
        put_changed(start + first, &was, title);
    }
}
