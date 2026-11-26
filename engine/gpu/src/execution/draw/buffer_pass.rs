//! Bindless buffer view sweep and stride-table binding.

use std::sync::atomic::{AtomicU64, Ordering};

use oathkeep_shader::ResourceType;

use crate::descriptors::View;
use crate::backend::HostList;
use crate::d3d12::object::This;

use super::heap_ranges::{
    BufferSweep, HEAP_ROUND, SLOT_FLAG_IN_PLACE, SLOT_FLAG_OBJECT, SLOT_FLAG_UAV, heap_spread,
};
use super::readable::view_kind;
use super::state::State;

/// Buffer views of the heap whose slot showed another host buffer than
/// the copy the pass brought up to date for them, since the start
/// (diagnostics; for the memory line). It stays 0 while the pass and the
/// slots agree; with `OATHKEEP_VIEW_EXTENT=0` it counts the views that were
/// read stale.
#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_VIEWS_ELSEWHERE: AtomicU64 = AtomicU64::new(0);
/// Passes over the view heap's buffer views for shaders that pick them by
/// index ([`State::heap_buffers`]), and the microseconds they took.
pub(crate) static HEAP_BUFFER_PASSES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_BUFFER_MICROS: AtomicU64 = AtomicU64::new(0);
/// Of the passes, nanoseconds since the last frame line: looking at slots,
/// the spans and refreshes of their copies, the texture side.
#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_SPLIT: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The pass over the buffer views of `heap` that shaders pick by index
/// ([`State::heap_buffers`]): their host slots are made, and the host copies
/// they show are to be refreshed.
fn heap_buffer_pass(
    heap: &crate::descriptors::Heap,
    host: &crate::descriptors::host_heap::HostHeap,
    frame: u64,
) -> Vec<crate::backend::heap_pass_async::Refresh> {
    let started = std::time::Instant::now();
    // Per host heap: the host copy, title range, and 12-byte descriptor record
    // each buffer slot reads, kept across passes. Modified slots come from
    // `host.buffer_changes()`; periodic background sweeps (`1/HEAP_ROUND` per
    // pass) validate cached 12-byte records in-place and skip unchanged slots
    // without re-mirroring or re-decoding, while dropping `copied_tabled`
    // slots immediately so stale `Shadow` windows are never pinned.
    let mut kept = host.buffer_sweep.lock().unwrap_or_else(|e| e.into_inner());
    let BufferSweep { passes, ranges, unwatched } = &mut *kept;
    let first_pass = *passes == 0;
    let all = first_pass || (*passes % HEAP_ROUND == 0 && !heap_spread());
    let picked_on = crate::diagnostics::picked_writes::on();
    let mut changed = host.buffer_changes();
    changed.sort_unstable();
    changed.dedup();
    let periodic = if first_pass {
        ranges.clear();
        host.buffer_slots()
    } else if all {
        let live = host.buffer_slots();
        ranges.prune_missing_in_part(0, 1, &live);
        live
    } else if heap_spread() {
        let (part, parts) = ((*passes % HEAP_ROUND) as u32, HEAP_ROUND as u32);
        let live = host.buffer_slots_of(part, parts);
        ranges.prune_missing_in_part(part, parts, &live);
        live
    } else {
        Vec::new()
    };
    *passes += 1;

    let forgot = crate::descriptors::views_forgot() as u32;
    let mut unordered = Vec::new();
    let mut slots = Vec::with_capacity(changed.len() + 64);

    // Pre-filter `changed` and `periodic` (both sorted ascending) without
    // duplicating slots or re-decoding unchanged periodic entries.
    let (mut ci, mut pi) = (0usize, 0usize);
    while ci < changed.len() || pi < periodic.len() {
        let c = changed.get(ci).copied();
        let p = periodic.get(pi).copied();
        let (index, was_changed) = match (c, p) {
            (Some(c), Some(p)) if c == p => {
                ci += 1;
                pi += 1;
                (c, true)
            }
            (Some(c), Some(p)) if c < p => {
                ci += 1;
                (c, true)
            }
            (_, Some(p)) => {
                pi += 1;
                (p, false)
            }
            (Some(c), None) => {
                ci += 1;
                (c, true)
            }
            (None, None) => break,
        };
        // A draw's table's: the draw brings what it reads up to date; release
        // any cached reference immediately so windows can be evicted.
        if host.copied_tabled(index, frame) {
            ranges.clear_slot(index);
            continue;
        }
        if let Some(cached) = ranges.get(index)
            && !cached.in_place()
            && let Some(handle) = heap.handle(index)
            && crate::descriptors::record_at(handle) == Some(cached.words)
            && {
                let key = u64::from(cached.words[2]) << 32 | u64::from(cached.words[1]);
                if was_changed {
                    host.shows_key(index, key)
                } else {
                    !host.shows_none(index)
                }
            }
            && (!cached.has_object()
                || cached.forgot == forgot
                || crate::descriptors::view_exists(handle))
        {
            if cached.has_object() && cached.forgot != forgot {
                ranges.touch_forgot(index, forgot);
            }
            if picked_on
                && cached.is_uav()
                && let Some(shadow) = ranges.copy_of(cached.copy)
            {
                unordered.push((shadow.clone(), cached.from, cached.to));
            }
            continue;
        }
        slots.push(index);
    }

    // Whether the device was there before the pass: a view that removes
    // it is named below.
    static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let alive = !slots.is_empty()
        && !GONE.load(Ordering::Relaxed)
        && crate::descriptors::host_heap::device_removed().is_none();
    /// The title's heap, for the threads that look.
    struct Shared<'a>(&'a crate::descriptors::Heap);
    // SAFETY: a heap is free-threaded (its methods are called by any thread of
    // the title's), and it lives until all of them have looked.
    unsafe impl Sync for Shared<'_> {}
    impl Shared<'_> {
        fn handle(&self, index: u32) -> Option<u64> {
            self.0.handle(index)
        }
    }
    let heap = Shared(heap);
    let look = |index: u32| {
        let handle = heap.handle(index)?;
        let (view, made_copy) = host.mirror_for_pass(index, handle)?;
        let words = crate::descriptors::record_at(handle).unwrap_or([0; 3]);
        let (shadow, address, len) = match made_copy {
            Some(copy) => copy,
            None => crate::descriptors::host_heap::buffer_copy(&view)?,
        };
        #[cfg(feature = "diagnostics")]
        if host.shows(index, &shadow.buffer) == Some(false) {
            HEAP_VIEWS_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
        }
        let is_uav = view_kind(&view) == Some(ResourceType::Uav);
        let has_object = matches!(view, View::Shader(..) | View::Unordered(..));
        let mut flags = 0u8;
        if is_uav {
            flags |= SLOT_FLAG_UAV;
        }
        if has_object {
            flags |= SLOT_FLAG_OBJECT;
        }
        if shadow.in_place() {
            flags |= SLOT_FLAG_IN_PLACE;
        }
        let copy_key = std::sync::Arc::as_ptr(&shadow) as usize;
        Some((copy_key, shadow, address, address + len, words, flags))
    };
    let report_removal = |slice: &[u32], why: String| {
        if !GONE.swap(true, Ordering::Relaxed) {
            let last: Vec<String> = slice
                .iter()
                .filter_map(|&i| {
                    crate::descriptors::view(heap.handle(i)?).map(|v| format!("{i}: {v:?}"))
                })
                .map(|s| s.chars().take(200).collect())
                .collect();
            tracing::error!(
                why,
                "the host device went while buffer views of the heap were made; perhaps one of: {last:#?}"
            );
            crate::backend::log_removal_context();
        }
    };
    // Run small delta batches inline to avoid thread-pool wakeup overhead;
    // fan out across `each_view` with a flat output buffer to avoid per-chunk
    // `OnceLock<Vec<_>>` allocation churn.
    const AT_A_TIME: usize = 32;
    if !slots.is_empty() {
        if slots.len() <= AT_A_TIME {
            for &index in &slots {
                let shows = look(index);
                if let Some((_, ref shadow, from, to, _, flags)) = shows
                    && picked_on
                    && flags & SLOT_FLAG_UAV != 0
                {
                    unordered.push((shadow.clone(), from, to));
                }
                ranges.set_with_forgot(index, shows, forgot);
            }
            if alive && let Some(why) = crate::descriptors::host_heap::device_removed() {
                report_removal(&slots, why);
            }
        } else {
            struct DisjointOut<T>(Vec<std::cell::UnsafeCell<Option<T>>>);
            // SAFETY: each worker `n` writes exclusively to indices
            // `n * AT_A_TIME .. ((n + 1) * AT_A_TIME).min(len)`, and `each_view`
            // joins all workers before `DisjointOut` is read.
            unsafe impl<T: Send> Sync for DisjointOut<T> {}
            impl<T> DisjointOut<T> {
                /// SAFETY: `pos` must be uniquely owned by the calling worker.
                #[inline]
                unsafe fn write(&self, pos: usize, value: Option<T>) {
                    unsafe { *self.0[pos].get() = value };
                }
            }
            let mut out = Vec::with_capacity(slots.len());
            out.resize_with(slots.len(), || std::cell::UnsafeCell::new(None));
            let found = DisjointOut(out);
            let parts = slots.len().div_ceil(AT_A_TIME);
            crate::memory::writeback_pool::each_view(false, parts, &|n| {
                let start = n * AT_A_TIME;
                let end = (start + AT_A_TIME).min(slots.len());
                for pos in start..end {
                    let shows = look(slots[pos]);
                    // SAFETY: `pos` is uniquely owned by worker `n`.
                    unsafe { found.write(pos, shows) };
                }
                if alive && let Some(why) = crate::descriptors::host_heap::device_removed() {
                    report_removal(&slots[start..end], why);
                }
            });
            for (&index, cell) in slots.iter().zip(found.0) {
                let shows = cell.into_inner();
                if let Some((_, ref shadow, from, to, _, flags)) = shows
                    && picked_on
                    && flags & SLOT_FLAG_UAV != 0
                {
                    unordered.push((shadow.clone(), from, to));
                }
                ranges.set_with_forgot(index, shows, forgot);
            }
        }
    }
    if picked_on {
        crate::diagnostics::picked_writes::shown(unordered, all);
    }
    #[cfg(feature = "diagnostics")]
    let looked = std::time::Instant::now();
    // Per host copy, the union span is maintained incrementally in `ranges`:
    // `O(#copies)` instead of scanning `80,000` slots and rebuilding a hash map.
    static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let every = *EVERY.get_or_init(|| {
        std::env::var("OATHKEEP_UNWATCHED_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
    });
    let views = ranges.views_count();
    let copies_count = ranges.copies_len();
    let mut bytes = 0u64;
    let mut found = Vec::with_capacity(copies_count);
    let mut next_unwatched = std::collections::HashMap::new();
    for (shadow, from, to) in ranges.copy_spans() {
        bytes += to - from;
        if every > 1 && shadow.unwatched() {
            let id = shadow.id();
            if let Some(&(last, low, high)) = unwatched.get(&id)
                && frame >= last
                && frame - last < every
                && low <= from
                && to <= high
            {
                next_unwatched.insert(id, (last, low, high));
                continue;
            }
            next_unwatched.insert(id, (frame, from, to));
        }
        found.push((shadow.clone(), from, to.min(from + shadow.left(from))));
    }
    *unwatched = next_unwatched;
    drop(kept);
    #[cfg(feature = "diagnostics")]
    let refreshed = std::time::Instant::now();
    crate::memory::watch::look_at_holes();
    #[cfg(feature = "diagnostics")]
    for (sum, time) in HEAP_SPLIT.iter().zip([looked - started, refreshed - looked]) {
        sum.fetch_add(time.as_nanos() as u64, Ordering::Relaxed);
    }
    static LOGGED: AtomicU64 = AtomicU64::new(0);
    // The first pass and every growth of the set by half.
    let seen = LOGGED.load(Ordering::Relaxed);
    if u64::from(views) > seen + seen / 2 {
        LOGGED.store(u64::from(views), Ordering::Relaxed);
        tracing::info!(
            slots = slots.len(),
            views,
            copies = copies_count,
            mb = bytes >> 20,
            ms = started.elapsed().as_millis(),
            "buffer views of the heap made ready for shaders that pick them by index"
        );
    }
    found
}

impl State {
    /// For a shader that picks buffers from the view heap by an index it
    /// computes (SM 6.6 dynamic resources): the GPU address of the heap's
    /// table of element strides, with every buffer view of the heap given
    /// its host slot and its memory brought up to date when `list` is
    /// submitted. Tables tell us which views a draw reads; nothing tells
    /// us which of the heap's a computed index names, so all of them are
    /// made ready, once per list.
    /// shortcut: the buffers are read, not written: what such a shader
    /// writes through a picked unordered access view stays in the host
    /// copy. A list refreshes them where it first runs such a shader;
    /// title writes recorded later in the same list are not seen.
    pub(super) fn heap_buffers(&self, list: &HostList) -> Option<u64> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptors::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        // Stride buffers have aligned GPU addresses. Tag the list key so it
        // is distinct from aligned guest resource pointers.
        if !list.first_use(gpu as usize | 1) {
            return Some(gpu);
        }
        // The list may outlive the title's heap object, which owns the table.
        if let Some(buffer) = host.strides_buffer() {
            list.keep(buffer);
        }
        // The pass itself once per frame: per list it ran up to 51 times a
        // frame over 81,000 views and 4,100 host copies, 18 s of every
        // frame (2026-11-24, run 11).
        // shortcut: what the title writes into such buffers between lists
        // of one frame is seen a frame late.
        let frame = crate::d3d12::frame::current();
        if host.buffer_frame.swap(frame, Ordering::Relaxed) == frame {
            return Some(gpu);
        }
        let started = std::time::Instant::now();
        let mut stage = crate::diagnostics::timeline::span("the heap pass: begun, or done here");
        if crate::backend::heap_pass_async::on() {
            let at = std::ptr::from_ref(heap) as usize;
            list.refreshes_coming(move || {
                crate::descriptors::with_heap(at, |heap| {
                    // The list retains the original stride buffer. A newly
                    // allocated heap at the same CPU address has another one.
                    heap.host()
                        .filter(|h| h.strides_gpu() == Some(gpu))
                        .map(|h| heap_buffer_pass(heap, h, frame))
                })
                .flatten()
                .unwrap_or_default()
            });
        } else {
            for (shadow, from, to) in heap_buffer_pass(heap, host, frame) {
                shadow.refresh(list, from, to - from);
            }
        }
        #[cfg(feature = "diagnostics")]
        let refreshed = std::time::Instant::now();
        stage.next("the heap pass: textures");
        let _ = self.heap_textures(list, heap, host, None);
        stage.end();
        #[cfg(feature = "diagnostics")]
        HEAP_SPLIT[2].fetch_add(refreshed.elapsed().as_nanos() as u64, Ordering::Relaxed);
        HEAP_BUFFER_PASSES.fetch_add(1, Ordering::Relaxed);
        HEAP_BUFFER_MICROS.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        Some(gpu)
    }

    /// The GPU address of the bound view heap's stride/kind table without
    /// running the bindless heap buffer/texture sweep (for shaders that only
    /// inspect descriptors already mirrored via their descriptor tables).
    pub(super) fn heap_strides(&self, list: &HostList) -> Option<u64> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptors::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        // Tag bit 1 (distinct from `heap_buffers`'s `| 1` and aligned guest pointers).
        if list.first_use(gpu as usize | 2)
            && let Some(buffer) = host.strides_buffer()
        {
            list.keep(buffer);
        }
        Some(gpu)
    }
}
