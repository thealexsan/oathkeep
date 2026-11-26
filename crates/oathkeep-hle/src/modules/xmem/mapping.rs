use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use windows_sys::Win32::System::Memory::{
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile3, PAGE_READWRITE, UnmapViewOfFile2, VirtualFree,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use super::physical::{PAGE, Physical, Slice, page_table, physical, shown_mut};
use super::pool::{Busy, GROUP, Op, change, side_by_side};

/// What the title's mapping calls cost since the last asking, for the
/// streaming log, counted only while that log is on: calls, pages,
/// milliseconds in all, the longest one's and waiting for the lock; the same
/// of the GPU's page-mapping copies ([`map_alias`]; calls and milliseconds);
/// and by system call (unmap, split, map, commit) how many and their
/// milliseconds.
static MAPPING: [AtomicU64; 15] = [const { AtomicU64::new(0) }; 15];

pub(crate) fn map_costs() -> Option<String> {
    let n: [u64; 15] = std::array::from_fn(|i| MAPPING[i].swap(0, Ordering::Relaxed));
    let time = |i: usize| matches!(i, 2 | 3 | 4 | 6 | 8 | 10 | 12 | 14);
    (n[0] + n[5] > 0).then(|| {
        let all: Vec<String> = n
            .iter()
            .enumerate()
            .map(|(i, &n)| if time(i) { n / 1_000_000 } else { n })
            .map(|n| n.to_string())
            .collect();
        all.join("\t")
    })
}

/// The system calls that change a mapping, as [`MAPPING`] counts them.
#[derive(Clone, Copy)]
pub(super) enum Call {
    Unmap = 7,
    Split = 9,
    Map = 11,
    Commit = 13,
}

/// Runs `f`, a system call, and counts it while the streaming log is on.
pub(super) fn counted<T>(call: Call, f: impl FnOnce() -> T) -> T {
    if !super::super::dstorage::stream_log::on() {
        return f();
    }
    let started = std::time::Instant::now();
    let done = f();
    MAPPING[call as usize].fetch_add(1, Ordering::Relaxed);
    MAPPING[call as usize + 1].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    done
}

/// Counts a mapping call when dropped.
struct Costed {
    started: std::time::Instant,
    /// `None`: a copy of mappings.
    pages: Option<usize>,
    waited: u64,
}

impl Costed {
    fn begin(pages: Option<usize>) -> Option<Costed> {
        super::super::dstorage::stream_log::on().then(|| Costed {
            started: std::time::Instant::now(),
            pages,
            waited: 0,
        })
    }
}

impl Drop for Costed {
    fn drop(&mut self) {
        let ns = self.started.elapsed().as_nanos() as u64;
        let Some(pages) = self.pages else {
            MAPPING[5].fetch_add(1, Ordering::Relaxed);
            MAPPING[6].fetch_add(ns, Ordering::Relaxed);
            return;
        };
        for (i, n) in [1, pages as u64, ns].into_iter().enumerate() {
            MAPPING[i].fetch_add(n, Ordering::Relaxed);
        }
        MAPPING[3].fetch_max(ns, Ordering::Relaxed);
        MAPPING[4].fetch_add(self.waited, Ordering::Relaxed);
    }
}

/// Whether a mapping change asks the host only for what it needs: no split
/// of a placeholder that is already the view's own, no commit of section
/// pages that are committed (`OATHKEEP_MAP_LEAN=0`: both every time, as
/// before).
///
/// All of the title's streaming goes through one thread, which maps the
/// pages of a batch and then submits it. Each call waits for the process's
/// one address-space lock, behind the guards' and the driver's: 200 to 400
/// us a page in the world against 10 in a menu, 2 to 4 s of every 5 s of
/// that thread (runs stream-1 and -2; all six stacks taken of it in the
/// opening were in these calls), so that models came as fast as their pages
/// could be mapped. A page took up to four calls (unmap, split, map,
/// commit), twice: the title maps a page into its pool and then has the GPU
/// map it into its tiled heap.
pub(super) fn lean() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("OATHKEEP_MAP_LEAN").is_none_or(|v| v != "0"));
    *ON
}

/// The title's streaming thread runs below normal priority, as do sixteen
/// workers of ours when pipelines are made; on the console nothing else
/// wants its core. Called where that thread maps and submits: lifts a
/// thread that is below normal to above it (`OATHKEEP_STREAM_PRIORITY=0`:
/// left as the title set it).
pub(crate) fn lift_thread() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("OATHKEEP_STREAM_PRIORITY").is_none_or(|v| v != "0"));
    if !*ON {
        return;
    }
    // SAFETY: plain calls on this thread.
    unsafe {
        let thread = GetCurrentThread();
        if matches!(GetThreadPriority(thread), -2..0) {
            SetThreadPriority(thread, THREAD_PRIORITY_ABOVE_NORMAL);
        }
    }
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    // Read before any lock is taken: a fault in title memory may wait for
    // the mapping call that holds them.
    // SAFETY: the title passes `n` page numbers, or null.
    let pages = (!pages.is_null()).then(|| unsafe { std::slice::from_raw_parts(pages, n) }.to_vec());
    let mut costed = Costed::begin(Some(n));
    let _called = oathkeep_gpu::timeline::span("the title's XMemMapPhysicalPages");
    let _busy = Busy::begin();
    let mut physical = physical();
    if let Some(costed) = &mut costed {
        costed.waited = costed.started.elapsed().as_nanos() as u64;
    }
    // Until the pages are mapped again: `unmap` takes whole views away and
    // maps what stays of them again, and a mapped view is a reservation
    // until it is committed. A read of title memory in place must not fall
    // into that gap (`oathkeep_gpu::title_memory_remapping`).
    let _changing = oathkeep_gpu::title_memory_remapping();
    unmap(&mut physical, base, n * PAGE);
    let Some(pages) = pages else {
        return address;
    };
    let slices: Vec<Option<Slice>> = {
        let table = page_table();
        pages.iter().map(|page| table.pages.get(page).cloned()).collect()
    };
    let mut i = 0;
    while i < n {
        let Some(first) = slices[i].clone() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + slices[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(slice, k)| {
                slice.as_ref().is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    lift_thread();
    if side_by_side() > 1 && n > 1 {
        // Read before any lock is taken, as in `XMemMapPhysicalPages`.
        let wanted: Vec<(usize, usize)> = (0..n)
            // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
            .map(|i| unsafe { (*addresses.add(i) as usize, if pages.is_null() { 0 } else { *pages.add(i) }) })
            .collect();
        if wanted.iter().all(|w| w.0 % PAGE == 0) {
            return scatter(&wanted).into();
        }
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`oathkeep_gpu`, queue `CopyPageMappingsBatchX`):
/// the title maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let _costed = Costed::begin(None);
    let _busy = Busy::begin();
    let mut physical = physical();
    // As in `XMemMapPhysicalPages`: no read in place meets the gap.
    let _changing = oathkeep_gpu::title_memory_remapping();
    unmap(&mut physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let at = from + k;
        let Some(slice) = physical.slice_at(at) else {
            return false;
        };
        if !map(&mut physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// [`map_alias`] of many ranges, in order.
pub fn map_aliases(ranges: &[(u64, u64, u64)]) -> bool {
    let _costed = Costed::begin(None);
    lift_thread();
    if side_by_side() <= 1 {
        return ranges.iter().all(|&(to, from, len)| map_alias(to, from, len));
    }
    // By the page: the address, and the one whose page it is to show.
    let pages: Vec<(usize, Option<usize>)> = ranges
        .iter()
        .flat_map(|&(to, from, len)| {
            (0..len as usize).step_by(PAGE).map(move |k| (to as usize + k, (from != 0).then_some(from as usize + k)))
        })
        .collect();
    if pages.iter().any(|p| p.0 % PAGE != 0 || p.1.is_some_and(|from| from % PAGE != 0)) {
        return ranges.iter().all(|&(to, from, len)| map_alias(to, from, len));
    }
    let mut ok = true;
    let mut at = 0;
    while at < pages.len() {
        let _busy = Busy::begin();
        let mut physical = physical();
        let mut ops: Vec<Op> = Vec::new();
        let mut touched = std::collections::HashSet::new();
        for &(to, from) in &pages[at..] {
            // A page the group changes already, or one it would be mapped
            // from: after it.
            if ops.len() == GROUP || touched.contains(&to) || from.is_some_and(|from| touched.contains(&from)) {
                break;
            }
            let slice = from.map(|from| physical.slice_at(from));
            ok &= !matches!(slice, Some(None));
            touched.insert(to);
            ops.push(Op::new(to, slice.flatten()));
            at += 1;
        }
        let _changing = oathkeep_gpu::title_memory_remapping();
        ok &= change(&mut physical, ops);
    }
    ok
}

/// Maps (page number not 0) or unmaps each address; false if a page is not
/// the title's or the host refuses one.
fn scatter(wanted: &[(usize, usize)]) -> bool {
    let mut ok = true;
    let mut at = 0;
    while at < wanted.len() {
        let _busy = Busy::begin();
        let mut costed = Costed::begin(None);
        let mut ops: Vec<Op> = Vec::new();
        let mut physical = physical();
        if let Some(costed) = &mut costed {
            costed.waited = costed.started.elapsed().as_nanos() as u64;
        }
        {
            let table = page_table();
            let mut touched = std::collections::HashSet::new();
            for &(to, page) in &wanted[at..] {
                if ops.len() == GROUP || !touched.insert(to) {
                    break;
                }
                let slice = table.pages.get(&page).cloned();
                if page != 0 && slice.is_none() {
                    tracing::warn!(page, "XMemMapPhysicalPagesScatter: unknown page");
                    ok = false;
                }
                ops.push(Op::new(to, slice));
                at += 1;
            }
        }
        if let Some(costed) = &mut costed {
            costed.pages = Some(ops.len());
        }
        let _changing = oathkeep_gpu::title_memory_remapping();
        ok &= change(&mut physical, ops);
    }
    ok
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    let own = physical.holes.remove(&base) == Some(len) && lean();
    if !show(base, len, &slice, own) {
        return false;
    }
    // Noted before the GPU side is told: bytes DirectStorage put in earlier,
    // without notice at these addresses ([`pages_written`]), are there by
    // the time the copies of them go.
    shown_mut().note(base, len, &slice);
    // New contents, read-write: the GPU side's copies no longer guard it.
    oathkeep_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    oathkeep_gpu::title_memory_view(base, len, slice.section.handle as usize, (slice.index * PAGE) as u64);
    physical.views.insert(base, (len, slice));
    true
}

/// Cuts `[base, base + len)` out of the placeholder it is in (which fails
/// harmlessly when the range is the whole placeholder).
pub(super) fn split(base: usize, len: usize) {
    // SAFETY: acts only on a placeholder.
    counted(Call::Split, || unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    });
}

/// The system calls of [`map`]; `own`: the placeholder at `base` is `len`
/// long already.
pub(super) fn show(base: usize, len: usize, slice: &Slice, own: bool) -> bool {
    let map_view = || {
        // SAFETY: maps the section over a placeholder; acts only on one.
        counted(Call::Map, || unsafe {
            MapViewOfFile3(
                slice.section.handle,
                GetCurrentProcess(),
                base as *const c_void,
                (slice.index * PAGE) as u64,
                len,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        })
    };
    let mut view = if own { map_view() } else { MEMORY_MAPPED_VIEW_ADDRESS { Value: std::ptr::null_mut() } };
    if view.Value as usize != base {
        split(base, len);
        view = map_view();
    }
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !slice.section.commit_shown(slice.index, base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    true
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
pub(super) fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        shown_mut().forget(at, size, &slice);
        if at < base || at + size > end {
            // Parts of it stay, and are mapped again below.
            oathkeep_gpu::title_memory_splitting(at, size);
        }
        oathkeep_gpu::title_memory_unmapping(at, size);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        let gone = counted(Call::Unmap, || unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        });
        if gone != 0 && at >= base && at + size <= end {
            physical.holes.insert(at, size);
        }
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}
