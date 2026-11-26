use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use crate::watch::PAGE;

use super::Shadow;
#[cfg(feature = "diagnostics")]
use super::LIVE;
use super::cbv_slab::let_go_idle_aligned_cbv;
use super::loose::LOOSE;
#[cfg(feature = "diagnostics")]
use super::pool::{PINNED_SNAP_BYTES, POOL};
use super::pool::trim_snapshot_pool;
use super::registry::buffers_read;

/// Copies of windows outside buffers let go since the start, and their
/// bytes.
static LET_GO: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// For the memory line: the windows let go since the start and their
/// bytes, and of the bytes of the windows alive, how many lie between the
/// lowest and the highest address ever asked for in each.
#[cfg(feature = "diagnostics")]
pub(crate) fn windows() -> [u64; 3] {
    let asked: u64 = {
        let loose = LOOSE.write().unwrap_or_else(|e| e.into_inner());
        loose
            .values()
            .map(|s| {
                let low = s.asked[0].load(Ordering::Relaxed);
                let high = s.asked[1].load(Ordering::Relaxed);
                high.saturating_sub(low)
            })
            .sum()
    };
    [LET_GO[0].load(Ordering::Relaxed), LET_GO[1].load(Ordering::Relaxed), asked]
}

/// For the memory line, of the windows alive: the bytes of their pages
/// ever copied from title memory (the rest of each window is memory the
/// host zeroed and nothing read: what a window made no bigger than what is
/// read would not hold); the windows a descriptor slot shows and their
/// bytes (they stay until the title writes the slot again); the windows a
/// list or a pass over the heap holds and their bytes; the windows asked
/// for without a length (root views, which keep the 4 MiB grid) and their
/// bytes; and the window grid in bytes ([`window`]).
#[cfg(feature = "diagnostics")]
pub(crate) fn window_use() -> [u64; 8] {
    let mut out = [0u64; 8];
    let loose = LOOSE.write().unwrap_or_else(|e| e.into_inner());
    for s in loose.values() {
        let copied = s.copied.read().unwrap_or_else(|e| e.into_inner());
        let pages = copied.pages.iter().filter(|&&at| at != 0).count() as u64;
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

/// Lets go of the copies of windows outside buffers that nothing names any
/// more. They were never freed, and the world's streaming keeps asking for
/// new ones: 648 windows (2.8 GB) when the cutscene began, 974 (4.3 GB)
/// eight minutes later (run 24). A window goes when
/// - only the table of windows holds its copy: no list not yet reset reads
///   or writes it, no pass over the heap keeps its range, no submission is
///   comparing it;
/// - only the copy holds its host buffer: no descriptor slot views it (a
///   slot keeps the buffer it views, `HostHeap::kept`), so no draw can read
///   it through a slot made earlier;
/// - nothing asked for it for `IDLE_MS` and `IDLE_FRAMES` frames.
///
/// The next read of that memory makes a new copy and copies the window
/// again. Called before a submission starts, by the thread that submits.
/// Whether `(start, len)` in a sorted, unique list of `(start, len)` window
/// keys is strictly covered by another window in `keys`.
pub(super) fn superseded_window_flags(keys: &[(u64, u64)]) -> Vec<bool> {
    let mut out = vec![false; keys.len()];
    let mut max_end = 0u64;
    for (i, &(start, len)) in keys.iter().enumerate() {
        let end = start.saturating_add(len);
        let covered_by_earlier = end <= max_end;
        let covered_by_same_start = keys.get(i + 1).is_some_and(|&(next_start, next_len)| {
            next_start == start && next_len > len
        });
        out[i] = covered_by_earlier || covered_by_same_start;
        max_end = max_end.max(end);
    }
    out
}

pub(crate) fn let_go_idle() {
    trim_snapshot_pool();
    let (idle_ms, idle_frames) = if crate::residency::critical_commit() {
        (crate::host::ram_idle_ms(crate::host::available_physical_ram()).min(500), 2)
    } else if crate::residency::pressured() {
        (crate::host::ram_idle_ms(crate::host::available_physical_ram()).min(1_000), 3)
    } else {
        (crate::host::ram_idle_ms(crate::host::available_physical_ram()), 4)
    };
    let_go_idle_aligned_cbv(idle_ms, idle_frames);
    let gone: Vec<Arc<Shadow>> = {
        // Held from the look to the removal: `loose` hands a copy out
        // under the same lock, so none is handed out in between.
        let mut map = LOOSE.write().unwrap_or_else(|e| e.into_inner());
        if map.is_empty() {
            return;
        }
        let all_keys: Vec<(u64, u64)> = map.keys().copied().collect();
        let superseded = superseded_window_flags(&all_keys);
        let idle: Vec<(u64, u64)> = map
            .iter()
            .zip(superseded)
            .filter(|((_, s), is_superseded)| {
                Arc::strong_count(s) == 1
                    && (*is_superseded
                        || crate::host::long_ago(
                            s.used.load(Ordering::Relaxed),
                            idle_ms,
                            idle_frames,
                        ))
                    && crate::host::references(&s.buffer) == 1
            })
            .map(|((key, _), _)| *key)
            .collect();
        idle.into_iter().filter_map(|key| map.remove(&key)).collect()
    };
    if !gone.is_empty() {
        let (went, bytes) = (gone.len(), gone.iter().map(|s| s.len).sum::<u64>());
        crate::host::trail(|| {
            format!("sweep: {went} host copies of windows of title memory let go ({} KiB), held by no list and no slot", bytes >> 10)
        });
    }
    for shadow in gone {
        LET_GO[0].fetch_add(1, Ordering::Relaxed);
        LET_GO[1].fetch_add(shadow.len, Ordering::Relaxed);
        // Nothing runs now and nothing names it: freed at once unless a
        // submission is running after all.
        crate::host::retire(shadow);
    }
}

/// For the memory line: host copies alive and their bytes, of them the
/// windows of memory outside buffers and their bytes, and the snapshot
/// buffers kept for reuse and the bytes they reserve.
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 6] {
    let (windows, window_bytes) = {
        let loose = LOOSE.write().unwrap_or_else(|e| e.into_inner());
        // Each copy once: a window may stand under several keys.
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
            pool.bufs.len() as u64,
            (pool.bytes as u64).saturating_add(PINNED_SNAP_BYTES.load(Ordering::Relaxed)),
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

/// Titles also use memory outside any buffer object as GPU memory (
/// the title's interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches.
/// A window nothing names any more is let go ([`let_go_idle`]).
/// shortcut: a window is 4 MiB however little of it is read (the memory
/// line says how much was ever asked for, and how much was ever copied);
/// [`window`] cuts the windows of asks that give a length on a finer grid.
pub(super) const WINDOW: u64 = 4 << 20;

/// The grid the windows of asks with a length are cut on, in bytes: 512 KiB
/// on hosts with `<= 32 GiB` RAM (`limited_host_ram`), 1 MiB on larger hosts,
/// unless `OATHKEEP_WINDOW_KB` names another power of two from 64 KiB to 4 MiB.
/// A window is the PC's memory, all of it, however little of it is read:
/// 853 windows on the 4 MiB grid committed 3.57 GB while only 1.78 GB lay
/// between the lowest and highest address ever asked for in each. On a finer
/// grid a window ends near what is read while keeping nearby constant/buffer
/// descriptors coalesced. Unsized asks (root SRVs/UAVs) use tail windows
/// ([`tails`]) clamped to their `MEM_PRIVATE` virtual allocation where bounded.
pub(super) fn window() -> u64 {
    static GRID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *GRID.get_or_init(|| {
        let kb = std::env::var("OATHKEEP_WINDOW_KB").ok().and_then(|v| v.trim().parse::<u64>().ok());
        grid_for_ram(kb, crate::residency::tight_host_ram(), crate::residency::limited_host_ram())
    })
}

/// Default sized-window grid in bytes when `OATHKEEP_WINDOW_KB` is unset or
/// invalid: 512 KiB on `<= 20 GiB` hosts (16 GB RAM machines, preventing >3 GB
/// of loose-window commit slack and OS page-file thrashing), 1 MiB on
/// `20..=32 GiB` hosts, and 2 MiB on `> 32 GiB` hosts.
fn default_grid_for(tight_host_ram: bool, limited_host_ram: bool) -> u64 {
    if tight_host_ram {
        512 << 10
    } else if limited_host_ram {
        1 << 20
    } else {
        2 << 20
    }
}

/// The window grid in bytes for a setting in KiB: a power of two from
/// 64 KiB to 4 MiB, or [`default_grid_for`] for anything else.
pub(super) fn grid_for_ram(kb: Option<u64>, tight_host_ram: bool, limited_host_ram: bool) -> u64 {
    match kb {
        Some(kb) if kb.is_power_of_two() && (64..=4096).contains(&kb) => kb << 10,
        _ => default_grid_for(tight_host_ram, limited_host_ram),
    }
}

#[cfg(test)]
pub(super) fn grid_for(kb: Option<u64>, limited_host_ram: bool) -> u64 {
    grid_for_ram(kb, false, limited_host_ram)
}

#[cfg(test)]
pub(super) fn grid(kb: Option<u64>) -> u64 {
    grid_for_ram(kb, true, true)
}

/// The window (start, bytes) of title memory for an ask of `[address,
/// end)` on a grid of `grid` bytes.
pub(super) fn window_of(address: u64, end: u64, grid: u64) -> (u64, u64) {
    let start = address & !(grid - 1);
    (start, end.next_multiple_of(grid) - start)
}

/// Whether the window of an ask without a length begins at the ask instead
/// of at the 4 MiB line below it (`OATHKEEP_WINDOW_TAILS=0` disables it; on
/// by default). Such an ask (a root SRV/UAV) reads from its address forward;
/// what lies below the lowest address ever asked for in the cell is memory
/// the host zeroed and nothing reads. With this on, the window is the cell's
/// tail from the ask's step on ([`TAIL_STEP`]), clamped to the end of its
/// `MEM_PRIVATE` virtual allocation when smaller than the 4 MiB cell; an ask
/// lower in the cell later makes a longer tail, which every ask from then on
/// takes, and the shorter one goes as soon as nothing holds it ([`let_go_idle`]).
pub(super) fn tails() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WINDOW_TAILS").is_none_or(|v| v != "0"))
}

/// Tails begin on multiples of this. A longer tail is a new host buffer,
/// filled from title memory from its first byte to its last: begun on the
/// 64 KiB page of each ask, a cell whose asks came from the top down made
/// up to 63 of them, one after the other (part of run 71's copies). On
/// this step it is 15 at most, and a tail holds up to 192 KiB it did not
/// need.
const TAIL_STEP: u64 = 256 << 10;

/// The window (start, bytes) a tail ask at `address` makes: from the ask's
/// step ([`TAIL_STEP`]) to the end of its 4 MiB cell.
pub(super) fn tail_of(address: u64, end: u64) -> (u64, u64) {
    let (cell, len) = window_of(address, end, WINDOW);
    let start = address & !(TAIL_STEP - 1);
    (start, cell + len - start)
}

/// Bounds `(step_start, reach_end)` of a tail ask `[address, end)` given an
/// optional enclosing `MEM_PRIVATE` virtual allocation `(alloc_base, vad_end)`.
/// Both `step_start` and `reach_end - step_start` are multiples of `PAGE` (64 KiB).
pub(super) fn tail_bounds(address: u64, end: u64, vad: Option<(u64, u64)>) -> (u64, u64) {
    let (cell, cell_len) = window_of(address, end, WINDOW);
    let cell_end = cell + cell_len;
    let page_mask = !(PAGE as u64 - 1);
    if let Some((alloc_base, vad_end)) = vad
        && alloc_base <= address
        && end <= vad_end
    {
        let reach = vad_end.next_multiple_of(PAGE as u64).min(cell_end);
        let floor = (alloc_base & page_mask).max(cell);
        let start = (address & !(TAIL_STEP - 1)).max(floor);
        return (start, reach);
    }
    let (start, len) = tail_of(address, end);
    (start, start + len)
}

/// Queries the enclosing `MEM_PRIVATE` `VirtualAlloc` bounds `(alloc_base, vad_end)`
/// (clamped at `cap_end`) when `address` lies in committed private memory.
/// Returns `None` for `MEM_MAPPED` placeholder views so streamed placeholder
/// arenas keep their full cell reach.
pub(super) fn private_vad_bounds(address: u64, cap_end: u64) -> Option<(u64, u64)> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_FREE, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    if address == 0 || address >= cap_end {
        return None;
    }
    // SAFETY: plain data filled in by `VirtualQuery`.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid output struct size.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    if n == 0 || info.State != MEM_COMMIT || info.Type != MEM_PRIVATE {
        return None;
    }
    let alloc_base = info.AllocationBase as u64;
    let mut cur = (info.BaseAddress as u64).saturating_add(info.RegionSize as u64);
    // Walk sub-regions of the same `VirtualAlloc` (`MEM_COMMIT` or `MEM_RESERVE`)
    // up to `cap_end` so uncommitted tail pages inside the same reservation stay covered.
    for _ in 0..16 {
        if cur >= cap_end {
            return Some((alloc_base, cap_end));
        }
        // SAFETY: valid output struct size.
        let n = unsafe { VirtualQuery(cur as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State == MEM_FREE || info.AllocationBase as u64 != alloc_base {
            return Some((alloc_base, cur));
        }
        let next = (info.BaseAddress as u64).saturating_add(info.RegionSize as u64);
        if next <= cur {
            break;
        }
        cur = next;
    }
    None
}

/// Of `windows` (start, bytes), the one an ask of `[address, end)` that
/// may take any window takes ([`find_any`]): the longest that holds the
/// whole range.
pub(super) fn holder(windows: impl Iterator<Item = (u64, u64)>, address: u64, end: u64) -> Option<(u64, u64)> {
    windows
        .filter(|&(start, bytes)| start <= address && start + bytes >= end)
        .max_by_key(|&(start, bytes)| (bytes, std::cmp::Reverse(start)))
}

/// Whether windows may be smaller than their 4 MiB cell (a finer grid, or
/// tails): only then is there more than one window to choose from.
pub(super) fn fine() -> bool {
    tails() || window() < WINDOW
}

/// For the memory line: the bytes of title memory the windows alive lie
/// over (each byte once), beside the windows' own bytes in
/// [`memory`]: the difference is memory that is in more than one window,
/// which is copied once for each.
#[cfg(feature = "diagnostics")]
pub(crate) fn window_cover() -> u64 {
    let loose = LOOSE.write().unwrap_or_else(|e| e.into_inner());
    cover(loose.keys().copied())
}

/// The bytes `windows` (start, bytes; in order of start) lie over, each
/// byte once.
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
pub(super) fn cover(windows: impl Iterator<Item = (u64, u64)>) -> u64 {
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

/// Of `windows` (start, bytes; in order), the longest window a tail ask at
/// `address` reaching `reach` (from step `step`) can reuse: the longest
/// window starting at or below `step` (within `[floor, step]`) that covers
/// at least up to `reach`.
pub(super) fn longest_tail_to(
    windows: impl Iterator<Item = (u64, u64)>,
    floor: u64,
    step: u64,
    reach: u64,
) -> Option<(u64, u64)> {
    windows
        .filter(|&(start, bytes)| start >= floor && start <= step && start + bytes >= reach)
        .max_by_key(|&(start, bytes)| (bytes, std::cmp::Reverse(start)))
}

/// Of `windows` (start, bytes; in order), the longest tail a tail ask at
/// `address` can take: the first that begins in the ask's cell at or below
/// the ask's page and ends where the cell ends.
#[cfg(test)]
pub(super) fn longest_tail(windows: impl Iterator<Item = (u64, u64)>, address: u64, end: u64) -> Option<(u64, u64)> {
    let (cell, len) = window_of(address, end, WINDOW);
    let step = address & !(TAIL_STEP - 1);
    windows
        .filter(|&(start, bytes)| start >= cell && start <= step && start + bytes == cell + len)
        .min()
}

/// Whether `[address, address + len)` lies in one title buffer.
pub(crate) fn in_buffer(address: u64, len: u64) -> Option<(u64, u64)> {
    let end = address.checked_add(len)?;
    let map = buffers_read();
    let found = map.range(..=(address, usize::MAX)).rev().take(64).find(|(_, e)| e.end >= end);
    found.map(|(k, e)| (k.0, e.end - k.0))
}

/// Whether a title buffer of at most `most` bytes lies in `[address, address
/// + len)`, all of it or in part.
pub(crate) fn small_buffer_in(address: u64, len: u64, most: u64) -> bool {
    let map = buffers_read();
    map.range((address.saturating_sub(most), 0)..(address + len, 0))
        .any(|(key, entry)| entry.end > address && entry.end - key.0 <= most)
}
