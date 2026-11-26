use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::{HOST_WRITTEN, LET_GO, STAGING, committed};
use crate::d3d12::object::This;

/// Whether proactive LRU eviction of cold picked-by-index descriptor slots is
/// enabled (`OATHKEEP_TEXTURE_LRU_EVICT=0` disables; on by default).
fn lru_evict_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_LRU_EVICT").is_none_or(|v| v != "0"))
}

/// Tracked resident texture metadata for VRAM LRU eviction.
#[derive(Clone, Copy, Debug)]
struct LruEntry {
    resource: usize,
    start: u64,
    bytes: u64,
    last_used_ms: u64,
    /// 0 when uploaded only via picked-by-index sweep; non-zero timestamp when
    /// bound directly by a draw or decoration bake.
    draw_bound_ms: u64,
}

/// Generic fence-deferred drop queue: ensures evicted GPU objects are never
/// released until every submission that could have read them has finished plus
/// `lag` safety submissions.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct FenceRetireQueue<T> {
    pending: std::collections::VecDeque<(u64, T)>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<T> FenceRetireQueue<T> {
    pub(super) const fn new() -> Self {
        Self { pending: std::collections::VecDeque::new() }
    }

    pub(super) fn retire(&mut self, submitted_fence: u64, item: T) {
        self.pending.push_back((submitted_fence, item));
    }

    pub(super) fn drain_completed(&mut self, finished_fence: u64, lag: u64) -> Vec<T> {
        let safe = finished_fence.saturating_sub(lag);
        let mut ready = Vec::new();
        while let Some(&(fence, _)) = self.pending.front()
            && fence <= safe
        {
            if let Some((_, item)) = self.pending.pop_front() {
                ready.push(item);
            }
        }
        ready
    }

    pub(super) fn len(&self) -> usize {
        self.pending.len()
    }
}

/// LRU tracker for cold picked-by-index textures and cached view-heap slot
/// mappings so VRAM eviction unpins cold descriptor slots cleanly before
/// `resource::let_go_idle()` reclaims their host copies.
#[derive(Default)]
pub(super) struct LruTracker {
    entries: HashMap<u64, LruEntry>,
    by_resource: HashMap<usize, u64>,
    slots_by_start: HashMap<u64, Vec<u32>>,
    scan_cursor: u32,
}

impl LruTracker {
    const MAX_ENTRIES: usize = 65_536;

    pub(super) fn note_upload(&mut self, resource: usize, start: u64, bytes: u64, now_ms: u64, draw_bound: bool) {
        if start == 0 || bytes == 0 {
            return;
        }
        if let Some(old_start) = self.by_resource.insert(resource, start)
            && old_start != start
        {
            self.entries.remove(&old_start);
            self.slots_by_start.remove(&old_start);
        }
        let entry = self.entries.entry(start).or_insert(LruEntry {
            resource,
            start,
            bytes,
            last_used_ms: now_ms,
            draw_bound_ms: 0,
        });
        entry.resource = resource;
        entry.bytes = entry.bytes.max(bytes);
        entry.last_used_ms = now_ms;
        if draw_bound {
            entry.draw_bound_ms = now_ms.max(1);
        }
        if self.entries.len() > Self::MAX_ENTRIES
            && let Some((&oldest_start, &oldest_entry)) = self
                .entries
                .iter()
                .filter(|(_, e)| e.draw_bound_ms == 0)
                .min_by_key(|(_, e)| e.last_used_ms)
        {
            self.entries.remove(&oldest_start);
            self.slots_by_start.remove(&oldest_start);
            self.by_resource.remove(&oldest_entry.resource);
        }
    }

    fn seed_from_heap(&mut self, resource: usize, start: u64, bytes: u64, slot: u32, now_ms: u64) {
        if start == 0 || bytes == 0 {
            return;
        }
        let slots = self.slots_by_start.entry(start).or_default();
        if !slots.contains(&slot) {
            slots.push(slot);
        }
        if !self.entries.contains_key(&start) && self.entries.len() < Self::MAX_ENTRIES {
            // Seed discovered slots as slightly aged so already-resident cold
            // textures from earlier scenes can be evicted without waiting a
            // full fresh window when the card is already full.
            let seeded_ms = now_ms.saturating_sub(5_000);
            self.entries.insert(
                start,
                LruEntry { resource, start, bytes, last_used_ms: seeded_ms, draw_bound_ms: 0 },
            );
            self.by_resource.insert(resource, start);
        }
    }

    fn forget_resource(&mut self, resource: usize) {
        if let Some(start) = self.by_resource.remove(&resource) {
            self.entries.remove(&start);
            self.slots_by_start.remove(&start);
        }
    }

    fn remove_evicted(&mut self, start: u64) {
        if let Some(entry) = self.entries.remove(&start) {
            self.by_resource.remove(&entry.resource);
        }
        self.slots_by_start.remove(&start);
    }

    /// Selects cold eviction candidates ordered by priority:
    /// 1) unmapped dead textures first (zero visual impact),
    /// 2) cold picked-only textures (oldest `last_used_ms` first),
    /// 3) under Critical VRAM pressure only, long-idle draw-bound textures.
    pub(super) fn select_victims(
        &self,
        now_ms: u64,
        reclaim_bytes: u64,
        level: crate::memory::residency::VramLevel,
        is_staged: impl Fn(usize) -> bool,
        is_host_written: impl Fn(usize) -> bool,
        is_mapped: impl Fn(u64) -> bool,
    ) -> Vec<(u64, usize, u64, bool)> {
        let (min_idle_ms, draw_pin_ms) = match level {
            crate::memory::residency::VramLevel::Normal => (u64::MAX, u64::MAX),
            crate::memory::residency::VramLevel::High => (8_000, u64::MAX),
            crate::memory::residency::VramLevel::Critical => (4_000, 20_000),
        };
        // (tier, last_used_ms, start, resource, bytes, unmapped)
        let mut candidates: Vec<(u8, u64, u64, usize, u64, bool)> = Vec::new();
        for entry in self.entries.values() {
            if is_staged(entry.resource) || is_host_written(entry.resource) {
                continue;
            }
            let mapped = is_mapped(entry.start);
            if !mapped {
                candidates.push((0, entry.last_used_ms, entry.start, entry.resource, entry.bytes, true));
                continue;
            }
            if reclaim_bytes == 0 {
                continue;
            }
            let idle = now_ms.saturating_sub(entry.last_used_ms);
            if idle < min_idle_ms {
                continue;
            }
            if entry.draw_bound_ms == 0 {
                candidates.push((1, entry.last_used_ms, entry.start, entry.resource, entry.bytes, false));
            } else if now_ms.saturating_sub(entry.draw_bound_ms) >= draw_pin_ms {
                candidates.push((2, entry.last_used_ms, entry.start, entry.resource, entry.bytes, false));
            }
        }
        candidates.sort_unstable_by_key(|&(tier, last_used, start, ..)| (tier, last_used, start));
        let mut out = Vec::new();
        let mut planned = 0u64;
        const MAX_VICTIMS_PER_SWEEP: usize = 512;
        for (_, _, start, resource, bytes, unmapped) in candidates {
            if out.len() >= MAX_VICTIMS_PER_SWEEP {
                break;
            }
            if !unmapped && planned >= reclaim_bytes {
                break;
            }
            planned = planned.saturating_add(bytes);
            out.push((start, resource, bytes, unmapped));
        }
        out
    }
}

static PICKED_LRU: Mutex<Option<LruTracker>> = Mutex::new(None);
/// Cumulative LRU eviction counters: slots armed, slots unmade, bytes reclaimed.
static LRU_STATS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

pub(super) fn forget(resource: usize) {
    if let Some(lru) = PICKED_LRU.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        lru.forget_resource(resource);
    }
}

/// Records an uploaded picked-by-index texture in the LRU tracker.
pub(crate) fn note_picked_upload(resource: This, copy: &crate::d3d12::resource::HostTexture, staging: &super::Staging) {
    let now = crate::backend::now_ms();
    let bytes = copy.vram_bytes().max(staging.bytes).max(staging.read as u64);
    let _ = copy.serial;
    let mut lru = PICKED_LRU.lock().unwrap_or_else(|e| e.into_inner());
    lru.get_or_insert_with(LruTracker::default).note_upload(
        resource as usize,
        staging.memory as u64,
        bytes,
        now,
        false,
    );
}

pub(super) fn note_draw_upload(resource: This, start: u64, bytes: u64) {
    let now = crate::backend::now_ms();
    let mut lru = PICKED_LRU.lock().unwrap_or_else(|e| e.into_inner());
    lru.get_or_insert_with(LruTracker::default).note_upload(resource as usize, start, bytes, now, true);
}

/// Incrementally indexes made view-heap texture slots under VRAM pressure and
/// evicts cold picked-by-index slots via `HostHeap::texture_slot_gone`, whose
/// `unmake` path retires D3D12 resources behind in-flight GPU fences so
/// `resource::let_go_idle()` can safely reclaim VRAM without UAF or TDR.
fn evict_cold_picked() {
    use std::sync::atomic::Ordering::Relaxed;
    if !lru_evict_enabled() {
        return;
    }
    let plan = crate::memory::residency::sample_vram();
    let room_now = crate::d3d12::resource::room_lately();
    let level = if !room_now && plan.level != crate::memory::residency::VramLevel::Critical {
        crate::memory::residency::VramLevel::Critical
    } else {
        plan.level
    };
    let reclaim_bytes = if level == crate::memory::residency::VramLevel::Critical && plan.reclaim_bytes == 0 {
        256 << 20
    } else {
        plan.reclaim_bytes
    };
    if level == crate::memory::residency::VramLevel::Normal {
        return;
    }
    let Some(heap_ptr) = crate::descriptors::lent_heap() else {
        return;
    };
    let now_ms = crate::backend::now_ms();
    // Phase 1: Incrementally discover made view-heap texture slots (1 atomic
    // load per empty slot) without touching `r.host.used`.
    let scan_budget: u32 = if level == crate::memory::residency::VramLevel::Critical { 262_144 } else { 131_072 };
    let mut discovered: Vec<(usize, u64, u64, u32)> = Vec::new();
    let start_cursor = {
        let lru = PICKED_LRU.lock().unwrap_or_else(|e| e.into_inner());
        lru.as_ref().map_or(0, |l| l.scan_cursor)
    };
    let next_cursor = crate::descriptors::with_heap(heap_ptr, |heap| {
        let Some(host) = heap.host() else {
            return start_cursor;
        };
        let count = host.count();
        if count == 0 {
            return 0;
        }
        let steps = scan_budget.min(count);
        let mut cursor = start_cursor % count;
        for _ in 0..steps {
            let slot = cursor;
            cursor += 1;
            if cursor >= count {
                cursor = 0;
            }
            if host.shows_none(slot) {
                continue;
            }
            let Some(view) = heap.handle(slot).and_then(crate::descriptors::view) else {
                continue;
            };
            match view {
                crate::descriptors::View::Placed(false, res_desc, desc) if res_desc[0] != 1 => {
                    let start = u64::from(desc[10]) | (u64::from(desc[11]) << 32);
                    if let Some(res) = crate::d3d12::resource::placed_known(start, &res_desc)
                        && let Some((2, bytes, mem)) = crate::d3d12::resource::copy_state(res)
                    {
                        discovered.push((res as usize, mem, bytes, slot));
                    }
                }
                crate::descriptors::View::Shader(res, _) if crate::d3d12::resource::title_fills(res as This) => {
                    if let Some((2, bytes, mem)) = crate::d3d12::resource::copy_state(res as This) {
                        discovered.push((res, mem, bytes, slot));
                    }
                }
                _ => {}
            }
        }
        cursor
    })
    .unwrap_or(start_cursor);

    // Protect only actively busy/in-flight staging entries (held by a command
    // list, dirty, not yet uploaded, or touched within the last 2 seconds).
    let staged_set: std::collections::HashSet<usize> = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|m| {
            m.iter()
                .filter(|(_, s)| {
                    Arc::strong_count(s) > 1
                        || s.dirty.load(Relaxed)
                        || !s.uploaded.load(Relaxed)
                        || !crate::backend::long_ago(s.used.load(Relaxed), 2_000, 4)
                })
                .map(|(&res, _)| res)
                .collect()
        })
        .unwrap_or_default();
    let written_set: std::collections::HashSet<usize> = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .cloned()
        .unwrap_or_default();

    // Phase 2: Select coldest victims with their slot lists under `PICKED_LRU`,
    // then drop the lock before touching `HostHeap`.
    let victims: Vec<(u64, u64, bool, u64, Vec<u32>)> = {
        let mut guard = PICKED_LRU.lock().unwrap_or_else(|e| e.into_inner());
        let lru = guard.get_or_insert_with(LruTracker::default);
        lru.scan_cursor = next_cursor;
        for (res, start, bytes, slot) in discovered {
            lru.seed_from_heap(res, start, bytes, slot, now_ms);
        }
        let chosen = lru.select_victims(
            now_ms,
            reclaim_bytes,
            level,
            |res| staged_set.contains(&res),
            |res| written_set.contains(&res),
            |start| committed(start as *const u8, 1),
        );
        chosen
            .into_iter()
            .filter_map(|(start, _, bytes, unmapped)| {
                let slots = lru.slots_by_start.get(&start)?.clone();
                let idle_ms = lru
                    .entries
                    .get(&start)
                    .map_or(u64::MAX, |e| now_ms.saturating_sub(e.last_used_ms));
                (!slots.is_empty()).then_some((start, bytes, unmapped, idle_ms, slots))
            })
            .collect()
    };

    if victims.is_empty() {
        return;
    }

    // Phase 3: Evict selected slots behind in-flight GPU fences.
    let frame = crate::d3d12::frame::current();
    let mut evicted_starts = Vec::new();
    let (mut armed, mut unmade, mut freed_bytes) = (0u64, 0u64, 0u64);
    let _ = crate::descriptors::with_view_heap(|host| {
        for (start, bytes, unmapped, idle_ms, slots) in &victims {
            let mut all_gone = true;
            let mut any_unmade = false;
            let immediate = !*unmapped || *idle_ms >= 3_000;
            let requeue = (!*unmapped).then_some((frame, frame + 240));
            for &slot in slots {
                if host.shows_none(slot) {
                    continue;
                }
                let done = if immediate {
                    host.evict_texture_slot(slot, requeue)
                } else {
                    host.texture_slot_gone(slot)
                };
                if done {
                    unmade += 1;
                    any_unmade = true;
                } else {
                    armed += 1;
                    all_gone = false;
                }
            }
            if all_gone {
                evicted_starts.push(*start);
                if any_unmade {
                    freed_bytes = freed_bytes.saturating_add(*bytes);
                }
            }
        }
    });

    if !evicted_starts.is_empty() {
        let mut guard = PICKED_LRU.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lru) = guard.as_mut() {
            for start in evicted_starts {
                lru.remove_evicted(start);
            }
        }
    }

    if armed > 0 || unmade > 0 {
        let prev_unmade = LRU_STATS[1].fetch_add(unmade, Relaxed);
        LRU_STATS[0].fetch_add(armed, Relaxed);
        let total_bytes = LRU_STATS[2].fetch_add(freed_bytes, Relaxed) + freed_bytes;
        if unmade > 0 && (prev_unmade == 0 || (prev_unmade + unmade) / 256 > prev_unmade / 256) {
            tracing::info!(
                level = ?level,
                unmade_slots = unmade,
                armed_slots = armed,
                reclaimed_mib = freed_bytes >> 20,
                total_reclaimed_mib = total_bytes >> 20,
                "evicted cold picked-by-index texture slots to relieve VRAM pressure"
            );
        }
    }
}

/// Lets go of the staging buffers no list recorded a use of for `IDLE_MS`
/// and `IDLE_FRAMES` frames and no list holds.
/// Called before a submission starts, by the thread that submits.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    super::aliased::poll_aliased_mutations();
    evict_cold_picked();
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_mut() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    let idle_ms = crate::backend::ram_idle_ms(crate::memory::residency::headroom());
    let (mut went, mut bytes) = (0u64, 0u64);
    map.retain(|_, s| {
        let keep = Arc::strong_count(s) > 1
            || !crate::backend::long_ago(s.used.load(Relaxed), idle_ms, IDLE_FRAMES);
        if !keep {
            LET_GO[0].fetch_add(1, Relaxed);
            LET_GO[1].fetch_add(s.bytes, Relaxed);
            went += 1;
            bytes += s.bytes;
        }
        keep
    });
    if went > 0 {
        crate::backend::trail(|| format!("sweep: {went} staging buffers of textures let go ({} KiB), held by no list", bytes >> 10));
    }
}
