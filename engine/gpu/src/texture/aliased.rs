use std::collections::HashMap;
use std::sync::Mutex;

use super::{committed, digest};

/// Tracked aliased or previously mutated picked textures so post-upload writes
/// through secondary virtual mappings are caught even after `RECHECK_FRAMES`
/// finishes.
struct AliasedWatch {
    resource: usize,
    start: u64,
    len: u64,
    print: u64,
    holds: std::sync::Weak<crate::resource::Holds>,
}

#[derive(Default)]
struct AliasedMonitor {
    entries: HashMap<u64, AliasedWatch>,
    cursor: usize,
}

impl AliasedMonitor {
    const MAX_WATCHED: usize = 1024;
    const POLL_PER_SWEEP: usize = 128;

    fn watch(
        &mut self,
        resource: usize,
        start: u64,
        len: u64,
        print: u64,
        holds: std::sync::Weak<crate::resource::Holds>,
    ) {
        if start == 0 || len == 0 {
            return;
        }
        if !self.entries.contains_key(&start) && self.entries.len() >= Self::MAX_WATCHED {
            if let Some(&drop_key) = self.entries.keys().next() {
                self.entries.remove(&drop_key);
            }
        }
        self.entries.insert(start, AliasedWatch { resource, start, len, print, holds });
    }

    fn forget_resource(&mut self, resource: usize) {
        self.entries.retain(|_, w| w.resource != resource);
    }
}

static WATCHED_ALIASED: Mutex<Option<AliasedMonitor>> = Mutex::new(None);
static ALIASED_REFRESHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn forget(resource: usize) {
    if let Some(mon) = WATCHED_ALIASED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        mon.forget_resource(resource);
    }
}

pub(super) fn watch(
    resource: usize,
    start: u64,
    len: u64,
    print: u64,
    holds: std::sync::Weak<crate::resource::Holds>,
) {
    let mut guard = WATCHED_ALIASED.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(AliasedMonitor::default)
        .watch(resource, start, len, print, holds);
}

/// Polls watched aliased/mutating picked textures and wakes their descriptor
/// slots via `descriptor::host::remapped` whenever their bytes change.
pub(super) fn poll_aliased_mutations() {
    use std::sync::atomic::Ordering::Relaxed;
    let batch: Vec<(u64, u64, u64, std::sync::Weak<crate::resource::Holds>)> = {
        let mut guard = WATCHED_ALIASED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mon) = guard.as_mut() else { return };
        if mon.entries.is_empty() {
            return;
        }
        let keys: Vec<u64> = mon.entries.keys().copied().collect();
        let n = keys.len();
        let take = n.min(AliasedMonitor::POLL_PER_SWEEP);
        let start_idx = mon.cursor % n;
        mon.cursor = (start_idx + take) % n;
        let mut out = Vec::with_capacity(take);
        for offset in 0..take {
            let key = keys[(start_idx + offset) % n];
            if let Some(w) = mon.entries.get(&key) {
                out.push((w.start, w.len, w.print, w.holds.clone()));
            }
        }
        out
    };
    let mut dead = Vec::new();
    let mut updated = Vec::new();
    for (start, len, prev_print, weak_holds) in batch {
        let Some(holds) = weak_holds.upgrade() else {
            dead.push(start);
            continue;
        };
        if !committed(start as *const u8, 1) {
            dead.push(start);
            continue;
        }
        let now_print = fingerprint(start, len);
        if now_print != prev_print {
            // Invalidate the cached digest so `unchanged()` cannot short-circuit
            // the re-upload on the next texture pass.
            *holds.lock().unwrap_or_else(|e| e.into_inner()) = None;
            crate::descriptor::host::remapped(start as usize, len as usize);
            updated.push((start, now_print));
            let total = ALIASED_REFRESHED.fetch_add(1, Relaxed) + 1;
            if total == 1 || total.is_multiple_of(128) {
                tracing::info!(
                    refreshed = total,
                    address = format_args!("{start:#x}"),
                    bytes = len,
                    "refreshed mutated picked texture backed by aliased pages"
                );
            }
        }
    }
    if !dead.is_empty() || !updated.is_empty() {
        let mut guard = WATCHED_ALIASED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mon) = guard.as_mut() {
            for start in dead {
                mon.entries.remove(&start);
            }
            for (start, new_print) in updated {
                if let Some(w) = mon.entries.get_mut(&start) {
                    w.print = new_print;
                }
            }
        }
    }
}

/// Byte windows `(offset, len)` hashed by [`fingerprint`]: covers the whole
/// texture up to 48 KiB, otherwise the first 16 KiB (packed mip tail) plus 8
/// evenly distributed 4 KiB windows across the remainder (48 KiB total budget)
/// so mutations in any mip level are detected cleanly.
fn fingerprint_windows_into(len: usize, out: &mut [(usize, usize); 9]) -> usize {
    const HEAD: usize = 16 << 10;
    const SLICE: usize = 4 << 10;
    const SLICES: usize = 8;
    const WHOLE: usize = HEAD + SLICES * SLICE;
    if len == 0 {
        return 0;
    }
    if len <= WHOLE {
        out[0] = (0, len);
        return 1;
    }
    out[0] = (0, HEAD);
    let tail_start = HEAD;
    let tail_end = len - SLICE;
    let span = tail_end - tail_start;
    for i in 0..SLICES {
        let raw = if i + 1 == SLICES {
            tail_end
        } else {
            tail_start + span * i / (SLICES - 1)
        };
        let aligned = if i + 1 == SLICES { raw } else { (raw & !0xfff).clamp(tail_start, tail_end) };
        out[1 + i] = (aligned, SLICE);
    }
    1 + SLICES
}

#[cfg(test)]
pub(super) fn fingerprint_windows(len: usize) -> Vec<(usize, usize)> {
    let mut out = [(0usize, 0usize); 9];
    let n = fingerprint_windows_into(len, &mut out);
    out[..n].to_vec()
}

/// A fingerprint of the title memory `[at, at + len)`, never 0: of all of
/// it up to 48 KiB, else of 16 KiB at its start plus 8 evenly-spaced 4 KiB
/// windows spanning the rest of the mip chain. Read through the kernel, so
/// pages not there count as zeros.
pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
    const MAX_WINDOW: usize = 48 << 10;
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let len = len as usize;
    let mut windows = [(0usize, 0usize); 9];
    let count = fingerprint_windows_into(len, &mut windows);
    let mut print = 0u128;
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < MAX_WINDOW {
            read.resize(MAX_WINDOW, 0);
        }
        for &(from, bytes) in &windows[..count] {
            read[..bytes].fill(0);
            crate::host::copy_mapped(read.as_mut_ptr(), (at as usize + from) as *const u8, bytes, false);
            print = print.rotate_left(37) ^ digest(&read[..bytes]).0;
        }
    });
    let first = (print as u64 ^ (print >> 64) as u64).to_le_bytes();
    u64::from_le_bytes(first) | 1
}
