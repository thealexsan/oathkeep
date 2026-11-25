use std::sync::atomic::{AtomicU64, Ordering};

use oathkeep_shader::resource::RootIndexedView;
use oathkeep_shader::{ResourceType, RootType, UsedResource};

use crate::descriptor::View;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

#[derive(Debug, Clone, Default)]
enum Root {
    #[default]
    None,
    Table(u64),
    Constants(Vec<u32>),
    View(u64),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VertexView {
    address: u64,
    size: u32,
    stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexView {
    address: u64,
    size: u32,
    format: u32,
}

const DEPTH_WRITE: u32 = 0x10;
const DEPTH_READ: u32 = 0x20;
const SHADER_RESOURCE: u32 = 0x40 | 0x80;

#[derive(Clone, Copy)]
struct PassBudget {
    ms: u64,
    bytes: u64,
    bake: bool,
}

thread_local! {
    static TESTED_DEPTH: std::cell::Cell<(usize, bool)> =
        const { std::cell::Cell::new((0, false)) };
    static STORES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static PASS_BUDGET: std::cell::Cell<Option<PassBudget>> = const { std::cell::Cell::new(None) };
    static PLACED_BYTES_CACHE: std::cell::RefCell<std::collections::HashMap<[u32; 14], Option<u64>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn placed_bytes_cached(resource: &[u32; 14]) -> Option<u64> {
    PLACED_BYTES_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(&known) = cache.get(resource) {
            return known;
        }
        let bytes = crate::resource::placed_bytes(resource);
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(*resource, bytes);
        bytes
    })
}

fn texture_stage_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("OATHKEEP_TEXTURE_STAGE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(4)
    })
}

fn early_budgets() -> (u64, u64) {
    static MS: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default)
        };
        (
            ms("OATHKEEP_EARLY_TEXTURES_MS", texture_stage_ms()),
            ms("OATHKEEP_BAKE_TEXTURES_MS", 32),
        )
    })
}

struct FrameTextureBudget {
    frame: AtomicU64,
    used_us: AtomicU64,
}

impl FrameTextureBudget {
    const fn new() -> Self {
        Self {
            frame: AtomicU64::new(u64::MAX),
            used_us: AtomicU64::new(0),
        }
    }

    fn begin_frame(&self, frame: u64) {
        if self.frame.load(Ordering::Relaxed) != frame
            && self.frame.swap(frame, Ordering::Relaxed) != frame
        {
            self.used_us.store(0, Ordering::Relaxed);
        }
    }

    fn remaining_us(&self, frame: u64, budget_us: u64) -> u64 {
        self.begin_frame(frame);
        budget_us.saturating_sub(self.used_us.load(Ordering::Relaxed))
    }

    fn record(&self, frame: u64, elapsed_us: u64) {
        self.begin_frame(frame);
        self.used_us.fetch_add(elapsed_us, Ordering::Relaxed);
    }
}

static FRAME_TEX_BUDGET: FrameTextureBudget = FrameTextureBudget::new();

const BAKE_PIXEL_SHADERS: [&str; 4] = [
    "c0a7fe77b1862e10",
    "0d1575a396dfe513",
    "b7cfd8dda91ebb6b",
    "280967ad158cee0b",
];

pub(crate) static EARLY_TEXTURE_PASSES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

fn write_skip() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WRITE_SKIP").is_none_or(|v| v != "0"))
}

#[derive(Clone)]
struct Window {
    copy: u64,
    kind: u32,
    stride: u32,
    step: u32,
    base: u64,
    records: u32,
    epoch: u64,
    dst_offset: u64,
    scratch: ID3D12Resource,
}

const WINDOW_MOST: u64 = 1024;
const WINDOW_STEP_MAX: u64 = 4096;
const MAX_WINDOWS: usize = 16;

fn windows() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_INDIRECT_WINDOW").is_none_or(|v| v != "0"))
}

#[derive(Default)]
pub(crate) struct State {
    pipeline: usize,
    heaps: [usize; 2],
    graphics: Vec<Root>,
    compute: Vec<Root>,
    vertex: [VertexView; 32],
    index: IndexView,
    targets: Vec<usize>,
    depth: usize,
    depth_state: u32,
    topology: u32,
    topology_set: bool,
    target_handles: Vec<u64>,
    depth_handle: u64,
    targets_set: bool,
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
    depth_bounds: Option<[f32; 2]>,
    windows: Vec<Window>,
    host_pipeline: usize,
    host_roots: [usize; 2],
    host_heaps: [usize; 2],
    fresh_tables: std::collections::HashMap<(u64, u32, u128), u32>,
    dynamic: Vec<(usize, u32, u32)>,
    root_srv_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    root_uav_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    tables_gone: bool,
    scratch_fresh: Vec<u8>,
    scratch_made: Vec<(u8, u32)>,
    scratch_entries: Vec<usize>,
    scratch_selected: Vec<u32>,
    scratch_addresses: Vec<u64>,
    scratch_constants: Vec<u32>,
}

impl Drop for State {
    fn drop(&mut self) {
        for &(heap, first, n) in &self.dynamic {
            crate::descriptor::release_dynamic(heap, first, n);
        }
    }
}

impl State {
    pub(crate) fn tables_done(&mut self) {
        if self.tables_gone && self.dynamic.is_empty() {
            return;
        }
        if self.tables_gone {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!("a list ran again after its descriptor tables were given back")
            });
        }
        for (heap, first, n) in self.dynamic.drain(..) {
            crate::descriptor::release_dynamic(heap, first, n);
        }
        self.fresh_tables.clear();
        self.root_srv_slots.clear();
        self.root_uav_slots.clear();
        self.tables_gone = true;
    }
}

fn scissor_test() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OATHKEEP_SCISSOR_TEST").as_deref() == Ok("full"))
}

fn mesh_test() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("OATHKEEP_MESH_TEST").as_deref() {
        Ok("skip") => 1,
        Ok("scissor") => 2,
        _ => 0,
    })
}

fn mesh_kept(pipeline: usize) -> bool {
    static KEEP: std::sync::OnceLock<Option<(String, String)>> = std::sync::OnceLock::new();
    let Some((last, before)) = KEEP.get_or_init(|| {
        let value = std::env::var("OATHKEEP_MESH_TEST").ok()?;
        let mut parts = value.strip_prefix("keep:")?.split(':');
        Some((
            parts.next()?.to_owned(),
            parts.next().unwrap_or("").to_owned(),
        ))
    }) else {
        return true;
    };
    let Some(p) = crate::pipeline::host_pipeline(pipeline as This) else {
        return true;
    };
    let Some(at) = p.name.find("mesh ") else {
        return true;
    };
    let hash = p.name[at + 5..].as_bytes();
    if hash.len() < 16 {
        return true;
    }
    last.as_bytes().contains(&hash[15])
        && (before.is_empty() || before.as_bytes().contains(&hash[14]))
}

static DRAWN: AtomicU64 = AtomicU64::new(0);

pub(crate) fn drawn() -> u64 {
    DRAWN.load(Ordering::Relaxed)
}

#[cfg(feature = "diagnostics")]
static FRAME: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[cfg(feature = "diagnostics")]
pub(crate) fn take_frame() -> Vec<String> {
    std::mem::take(&mut *FRAME.lock().unwrap_or_else(|e| e.into_inner()))
}

pub(crate) fn take_dropped() -> u64 {
    DROPPED.swap(0, Ordering::Relaxed)
}
type RootSlot = (u32, Option<std::sync::Arc<crate::shadow::Shadow>>);

#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_VIEWS_ELSEWHERE: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_BUFFER_PASSES: AtomicU64 = AtomicU64::new(0);
static HEAP_BUFFER_LAST_FRAME: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_BUFFER_MICROS: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURE_MICROS: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_WAITING: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_FULL: AtomicU64 = AtomicU64::new(0);

// Private periodic breakdown accumulators (logged every ~5s from `draw.rs`; never
// consume `HEAP_BUFFER_MICROS` / `HEAP_BUFFER_PASSES`, which `host.rs` swaps).
static LOG_BUF_PASSES: AtomicU64 = AtomicU64::new(0);
static LOG_BUF_US: AtomicU64 = AtomicU64::new(0);
static LOG_BUF_MIRROR_US: AtomicU64 = AtomicU64::new(0);
static LOG_BUF_SPANS_US: AtomicU64 = AtomicU64::new(0);
static LOG_BUF_REFRESH_US: AtomicU64 = AtomicU64::new(0);
static LOG_BUF_REBUILDS: AtomicU64 = AtomicU64::new(0);
static LOG_BUF_REBUILT_SHADOWS: AtomicU64 = AtomicU64::new(0);
static LOG_BUF_SLOTS: AtomicU64 = AtomicU64::new(0);
static LOG_TEX_PASSES: AtomicU64 = AtomicU64::new(0);
static LOG_TEX_US: AtomicU64 = AtomicU64::new(0);
static LOG_TEX_PREP_US: AtomicU64 = AtomicU64::new(0);
static LOG_TEX_DUE_US: AtomicU64 = AtomicU64::new(0);
static LOG_TEX_LOOP_US: AtomicU64 = AtomicU64::new(0);
static LOG_TEX_BATCHES: AtomicU64 = AtomicU64::new(0);
static LOG_TEX_SEEN: AtomicU64 = AtomicU64::new(0);
static LOG_EARLY_PASSES: AtomicU64 = AtomicU64::new(0);
static LOG_EARLY_US: AtomicU64 = AtomicU64::new(0);

fn maybe_log_heap_breakdown() {
    static NEXT_LOG_MS: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms().max(1);
    let next = NEXT_LOG_MS.load(Ordering::Relaxed);
    if next == 0 {
        let _ = NEXT_LOG_MS.compare_exchange(0, now + 5000, Ordering::Relaxed, Ordering::Relaxed);
        return;
    }
    if now < next
        || NEXT_LOG_MS
            .compare_exchange(next, now + 5000, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let buf_passes = LOG_BUF_PASSES.swap(0, Ordering::Relaxed);
    let tex_passes = LOG_TEX_PASSES.swap(0, Ordering::Relaxed);
    let early_passes = LOG_EARLY_PASSES.swap(0, Ordering::Relaxed);
    if buf_passes == 0 && tex_passes == 0 && early_passes == 0 {
        return;
    }
    let buf_us = LOG_BUF_US.swap(0, Ordering::Relaxed);
    let buf_mirror_us = LOG_BUF_MIRROR_US.swap(0, Ordering::Relaxed);
    let buf_spans_us = LOG_BUF_SPANS_US.swap(0, Ordering::Relaxed);
    let buf_refresh_us = LOG_BUF_REFRESH_US.swap(0, Ordering::Relaxed);
    let buf_rebuilds = LOG_BUF_REBUILDS.swap(0, Ordering::Relaxed);
    let buf_rebuilt_shadows = LOG_BUF_REBUILT_SHADOWS.swap(0, Ordering::Relaxed);
    let buf_slots = LOG_BUF_SLOTS.swap(0, Ordering::Relaxed);
    let tex_us = LOG_TEX_US.swap(0, Ordering::Relaxed);
    let tex_prep_us = LOG_TEX_PREP_US.swap(0, Ordering::Relaxed);
    let tex_due_us = LOG_TEX_DUE_US.swap(0, Ordering::Relaxed);
    let tex_loop_us = LOG_TEX_LOOP_US.swap(0, Ordering::Relaxed);
    let tex_batches = LOG_TEX_BATCHES.swap(0, Ordering::Relaxed);
    let tex_seen = LOG_TEX_SEEN.swap(0, Ordering::Relaxed);
    let early_us = LOG_EARLY_US.swap(0, Ordering::Relaxed);
    let buf_div = buf_passes.max(1) as f64;
    let tex_div = tex_passes.max(1) as f64;
    let early_div = early_passes.max(1) as f64;
    tracing::info!(
        "heap pass breakdown (5s): buffers {buf_passes} passes avg {:.2} ms (mirror {:.2} ms, spans {:.2} ms, refresh {:.2} ms, {buf_rebuilds} span rebuilds / {buf_rebuilt_shadows} shadows, {buf_slots} slots); \
         heap_textures {tex_passes} passes avg {:.2} ms (prep {:.2} ms, due {:.2} ms, loop {:.2} ms, {tex_batches} batches, {tex_seen} slots); \
         early_textures {early_passes} passes avg {:.2} ms",
        buf_us as f64 / 1e3 / buf_div,
        buf_mirror_us as f64 / 1e3 / buf_div,
        buf_spans_us as f64 / 1e3 / buf_div,
        buf_refresh_us as f64 / 1e3 / buf_div,
        tex_us as f64 / 1e3 / tex_div,
        tex_prep_us as f64 / 1e3 / tex_div,
        tex_due_us as f64 / 1e3 / tex_div,
        tex_loop_us as f64 / 1e3 / tex_div,
        early_us as f64 / 1e3 / early_div,
    );
}

fn speculative_room_cached(force: bool) -> bool {
    static LAST_AT_AND_ROOM: AtomicU64 = AtomicU64::new(0);
    static LAST_BYTES: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let staged = HEAP_TEXTURE_BYTES.load(Ordering::Relaxed);
    if !force {
        let prev = LAST_AT_AND_ROOM.load(Ordering::Relaxed);
        if prev != 0 {
            let prev_ms = (prev >> 1).saturating_sub(1);
            let prev_bytes = LAST_BYTES.load(Ordering::Relaxed);
            if now.saturating_sub(prev_ms) < 100 && staged.saturating_sub(prev_bytes) < (16 << 20) {
                return (prev & 1) != 0;
            }
        }
    }
    let room = crate::resource::speculative_room();
    LAST_BYTES.store(staged, Ordering::Relaxed);
    LAST_AT_AND_ROOM.store(((now + 1) << 1) | u64::from(room), Ordering::Relaxed);
    room
}

struct ShadowSlots<T> {
    item: T,
    slots: std::collections::HashMap<u32, (u64, u64)>,
    spans: Vec<(usize, usize)>,
    bytes: u64,
    dirty: bool,
}

struct BufferSpanCache<T> {
    passes: u64,
    slot_owner: std::collections::HashMap<u32, usize>,
    slot_keys: std::collections::HashMap<u32, u64>,
    shadows: std::collections::HashMap<usize, ShadowSlots<T>>,
    dirty_keys: Vec<usize>,
    bytes: u64,
    rebuilds: u64,
}

impl<T> Default for BufferSpanCache<T> {
    fn default() -> Self {
        Self {
            passes: 0,
            slot_owner: std::collections::HashMap::new(),
            slot_keys: std::collections::HashMap::new(),
            shadows: std::collections::HashMap::new(),
            dirty_keys: Vec::new(),
            bytes: 0,
            rebuilds: 0,
        }
    }
}

impl<T> BufferSpanCache<T> {
    fn clear(&mut self) {
        if !self.slot_owner.is_empty()
            || !self.slot_keys.is_empty()
            || !self.shadows.is_empty()
            || !self.dirty_keys.is_empty()
        {
            self.slot_owner.clear();
            self.slot_keys.clear();
            self.shadows.clear();
            self.dirty_keys.clear();
            self.bytes = 0;
        }
    }

    fn slot_key(&self, index: u32) -> Option<u64> {
        self.slot_keys.get(&index).copied()
    }

    fn set_slot_key(&mut self, index: u32, key: u64) {
        if key == 0 {
            self.slot_keys.remove(&index);
        } else {
            self.slot_keys.insert(index, key);
        }
    }

    fn retain_slots(&mut self, keep: &std::collections::HashSet<u32>, key_of: impl Fn(&T) -> usize) {
        let stale: Vec<u32> = self
            .slot_owner
            .keys()
            .copied()
            .filter(|idx| !keep.contains(idx))
            .collect();
        for index in stale {
            self.set(index, None, &key_of);
            self.slot_keys.remove(&index);
        }
        self.slot_keys.retain(|idx, _| keep.contains(idx));
    }

    fn slot_count(&self) -> usize {
        self.slot_owner.len()
    }

    fn collect_stale_slots(
        &self,
        shadow_matches: impl Fn(&T) -> bool,
        range_matches: impl Fn(u64, u64) -> bool,
    ) -> Vec<u32> {
        let mut stale = Vec::new();
        for entry in self.shadows.values() {
            if !shadow_matches(&entry.item) {
                continue;
            }
            for (&index, &(from, to)) in &entry.slots {
                if range_matches(from, to) {
                    stale.push(index);
                }
            }
        }
        stale
    }

    fn set(&mut self, index: u32, entry: Option<(T, u64, u64)>, key_of: impl Fn(&T) -> usize) {
        match entry {
            Some((item, from, to)) => {
                let new_key = key_of(&item);
                if let Some(&old_key) = self.slot_owner.get(&index) {
                    if old_key == new_key {
                        if let Some(shadow) = self.shadows.get_mut(&new_key) {
                            if shadow.slots.get(&index) == Some(&(from, to)) {
                                return;
                            }
                            shadow.slots.insert(index, (from, to));
                            if !shadow.dirty {
                                shadow.dirty = true;
                                self.dirty_keys.push(new_key);
                            }
                        }
                        return;
                    }
                    if let Some(old_shadow) = self.shadows.get_mut(&old_key) {
                        old_shadow.slots.remove(&index);
                        if !old_shadow.dirty {
                            old_shadow.dirty = true;
                            self.dirty_keys.push(old_key);
                        }
                    }
                }
                self.slot_owner.insert(index, new_key);
                let shadow = self
                    .shadows
                    .entry(new_key)
                    .or_insert_with(|| ShadowSlots {
                        item,
                        slots: std::collections::HashMap::new(),
                        spans: Vec::new(),
                        bytes: 0,
                        dirty: false,
                    });
                shadow.slots.insert(index, (from, to));
                if !shadow.dirty {
                    shadow.dirty = true;
                    self.dirty_keys.push(new_key);
                }
            }
            None => {
                if let Some(old_key) = self.slot_owner.remove(&index)
                    && let Some(old_shadow) = self.shadows.get_mut(&old_key)
                {
                    old_shadow.slots.remove(&index);
                    if !old_shadow.dirty {
                        old_shadow.dirty = true;
                        self.dirty_keys.push(old_key);
                    }
                }
            }
        }
    }

    fn ensure_spans(&mut self) -> (&std::collections::HashMap<usize, ShadowSlots<T>>, u64, usize) {
        if self.dirty_keys.is_empty() {
            return (&self.shadows, self.bytes, 0);
        }
        let mut rebuilt = 0usize;
        for key in self.dirty_keys.drain(..) {
            let Some(entry) = self.shadows.get_mut(&key) else {
                continue;
            };
            if !entry.dirty {
                continue;
            }
            self.bytes -= entry.bytes;
            if entry.slots.is_empty() {
                self.shadows.remove(&key);
            } else {
                entry.spans.clear();
                for &(from, to) in entry.slots.values() {
                    crate::shadow::merge_refresh_ranges(&mut entry.spans, from as usize, to as usize);
                }
                entry.bytes = entry
                    .spans
                    .iter()
                    .map(|&(from, to)| (to - from) as u64)
                    .sum();
                self.bytes += entry.bytes;
                entry.dirty = false;
            }
            rebuilt += 1;
        }
        if rebuilt > 0 {
            self.rebuilds += 1;
        }
        (&self.shadows, self.bytes, rebuilt)
    }
}

#[inline]
fn hash_table_words(words: &[u32; 3], h1: &mut u64, h2: &mut u64) {
    let a = u64::from(words[0]) | (u64::from(words[1]) << 32);
    let b = u64::from(words[2]);
    *h1 = (*h1 ^ a).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    *h1 = h1.rotate_left(27) ^ b;
    *h2 = (*h2 ^ b.wrapping_mul(0xbf58_476d_1ce4_e5b9)).wrapping_mul(0x94d0_49bb_1331_11eb);
    *h2 = h2.rotate_left(31) ^ a;
}

#[inline]
fn finalize_table_hash(mut h1: u64, mut h2: u64) -> u128 {
    h1 ^= h1 >> 30;
    h1 = h1.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h1 ^= h1 >> 27;
    h2 ^= h2 >> 30;
    h2 = h2.wrapping_mul(0x94d0_49bb_1331_11eb);
    h2 ^= h2 >> 31;
    (u128::from(h2) << 64) | u128::from(h1)
}

const RECHECK_FRAMES: [u64; 5] = [4, 12, 40, 120, 400];

pub(crate) static HEAP_TEXTURES_RECHECKED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

pub(crate) static HEAP_TEXTURES_REUPLOADED_FULL: AtomicU64 = AtomicU64::new(0);

fn full_card_pass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_FULL_CARD_PASS").is_none_or(|v| v != "0"))
}

fn reupload_when_full() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_REUPLOAD_WHEN_FULL").is_none_or(|v| v != "0"))
}

fn heap_recheck() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_HEAP_RECHECK").is_none_or(|v| v != "0"))
}
pub(crate) static HEAP_TEXTURES_AWAY: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_TABLED: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_QUEUE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
pub(crate) static HEAP_TEXTURES_PARTLY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static DROPPED: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
fn dropped(pipeline: usize) {
    crate::pipeline::note_drop(pipeline as This);
    if let Some(last) = FRAME.lock().unwrap_or_else(|e| e.into_inner()).last_mut() {
        last.push_str(" DROPPED");
    }
}

static DISPATCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dispatched() -> u64 {
    DISPATCHED.load(Ordering::Relaxed)
}

fn direct_cbv() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_DIRECT_CBV").is_some_and(|v| v == "1"))
}

fn root_view(list: &HostList, address: u64, srv: bool) -> Option<u64> {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("OATHKEEP_ROOT_VIEW_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(u64::MAX, |mb| mb << 20)
    });
    let shadow = crate::shadow::find(address, 1)?;
    let len = shadow.left(address).min(if srv { limit } else { 1 << 16 });
    let gpu = shadow.gpu(address);
    if srv || gpu % 256 == 0 {
        shadow.refresh(list, address, len);
        return Some(gpu);
    }
    let (gpu, to) = list.scratch(len)?;
    list.defer_fill(to, address as *const u8, len as usize);
    Some(gpu)
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

fn set(roots: &mut Vec<Root>, slot: u32, root: Root) {
    let slot = slot as usize;
    if roots.len() <= slot {
        roots.resize(slot + 1, Root::None);
    }
    roots[slot] = root;
}

impl State {
    pub(crate) fn reset(&mut self, pipeline: This) {
        self.tables_done();
        self.tables_gone = false;
        self.pipeline = pipeline as usize;
        self.heaps = [0; 2];
        self.graphics.clear();
        self.compute.clear();
        self.vertex = [VertexView::default(); 32];
        self.index = IndexView::default();
        self.targets.clear();
        self.depth = 0;
        self.depth_state = 0;
        self.topology = 0;
        self.topology_set = false;
        self.target_handles.clear();
        self.depth_handle = 0;
        self.targets_set = false;
        self.viewports.clear();
        self.scissors.clear();
        self.blend = None;
        self.stencil = None;
        self.depth_bounds = None;
        self.windows.clear();
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [0; 2];
    }

    pub(crate) fn set_topology(&mut self, list: &HostList, topology: u32) {
        self.topology = topology;
        self.topology_set = true;
        let host = match topology {
            RECTANGLES => 4,
            TRIANGLE_FAN | LINE_LOOP | QUADS | QUAD_STRIP => {
                once!(topology, "console topology drawn as a triangle list");
                4
            }
            t => t,
        };
        // SAFETY: an open list.
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
        };
    }

    pub(crate) fn set_pipeline(&mut self, pipeline: This) {
        self.pipeline = pipeline as usize;
    }

    pub(crate) fn set_heaps(&mut self, heaps: &[This]) {
        self.heaps = [0; 2];
        for &h in heaps {
            if !h.is_null() {
                let samplers = crate::descriptor::heap(h).samplers();
                self.heaps[usize::from(samplers)] = h as usize;
            }
        }
    }

    fn roots(&mut self, compute: bool) -> &mut Vec<Root> {
        if compute {
            &mut self.compute
        } else {
            &mut self.graphics
        }
    }

    pub(crate) fn set_table(&mut self, compute: bool, slot: u32, handle: u64) {
        set(self.roots(compute), slot, Root::Table(handle));
    }

    pub(crate) fn set_view(&mut self, compute: bool, slot: u32, address: u64) {
        set(self.roots(compute), slot, Root::View(address));
    }

    pub(crate) fn set_constants(&mut self, compute: bool, slot: u32, values: &[u32], offset: u32) {
        let roots = self.roots(compute);
        let s = slot as usize;
        if !matches!(roots.get(s), Some(Root::Constants(_))) {
            set(roots, slot, Root::Constants(Vec::new()));
        }
        if let Root::Constants(c) = &mut roots[s] {
            let end = offset as usize + values.len();
            if c.len() < end {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(values);
        }
    }

    pub(crate) fn set_vertex_buffers(&mut self, start: u32, views: &[VertexView]) {
        for (i, v) in views.iter().enumerate() {
            if let Some(slot) = self.vertex.get_mut(start as usize + i) {
                *slot = *v;
            }
        }
    }

    pub(crate) fn clear_vertex_buffers(&mut self, start: u32, count: u32) {
        for slot in self
            .vertex
            .iter_mut()
            .skip(start as usize)
            .take(count as usize)
        {
            *slot = VertexView::default();
        }
    }

    pub(crate) fn set_index_buffer(&mut self, view: Option<&IndexView>) {
        self.index = view.copied().unwrap_or_default();
    }

    pub(crate) fn set_targets(&mut self, list: &HostList, handles: &[u64], depth: u64) {
        let Some(host) = crate::host::host() else {
            return;
        };
        self.target_handles.clear();
        self.target_handles.extend_from_slice(handles);
        self.depth_handle = depth;
        self.targets_set = true;
        self.targets.clear();
        let mut rtv_stack = [D3D12_CPU_DESCRIPTOR_HANDLE::default(); 8];
        let mut rtv_count = 0usize;
        let mut rtv_overflow: Vec<D3D12_CPU_DESCRIPTOR_HANDLE> = Vec::new();
        for &h in handles {
            let Some(record) = crate::descriptor::rtv_record(h) else {
                once!(
                    handle = format_args!("{h:#x}"),
                    "render target without a recorded view"
                );
                continue;
            };
            let handle = if let Some(texture) = crate::resource::host_resource(record.resource) {
                self.targets.push(record.resource as usize);
                #[cfg(feature = "diagnostics")]
                crate::capture::note_target(record.resource);
                host.rtv(Some(texture), record.desc().as_ref())
            } else {
                let Some(desc) = record
                    .desc()
                    .filter(|d| d.Format.0 != 0 && d.ViewDimension.0 != 0)
                else {
                    continue;
                };
                host.rtv(None, Some(&desc))
            };
            if rtv_overflow.is_empty() && rtv_count < rtv_stack.len() {
                rtv_stack[rtv_count] = handle;
                rtv_count += 1;
            } else {
                if rtv_overflow.is_empty() {
                    rtv_overflow.extend_from_slice(&rtv_stack[..rtv_count]);
                }
                rtv_overflow.push(handle);
            }
        }
        let rtvs: &[D3D12_CPU_DESCRIPTOR_HANDLE] = if rtv_overflow.is_empty() {
            &rtv_stack[..rtv_count]
        } else {
            &rtv_overflow
        };
        self.depth = 0;
        self.depth_state = DEPTH_WRITE;
        let dsv = crate::descriptor::dsv_record(depth).and_then(|record| {
            let texture = crate::resource::host_resource(record.resource)?;
            self.depth = record.resource as usize;
            self.depth_state = depth_state(texture, record.depth_desc().as_ref());
            if crate::resource::depth_and_unordered(record.resource) {
                crate::resource::DEPTH_UNORDERED[1].fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            Some(host.dsv(texture, record.depth_desc().as_ref()))
        });
        // SAFETY: an open list; the views were just made.
        unsafe {
            list.list.OMSetRenderTargets(
                rtvs.len() as u32,
                (!rtvs.is_empty()).then_some(rtvs.as_ptr()),
                false,
                dsv.as_ref().map(std::ptr::from_ref),
            )
        };
    }

    pub(crate) fn set_viewports(&mut self, list: &HostList, viewports: &[D3D12_VIEWPORT]) {
        self.viewports.clear();
        self.viewports.extend_from_slice(viewports);
        crate::draw::viewports(list, viewports);
    }

    pub(crate) fn set_scissors(&mut self, list: &HostList, rects: &[RECT]) {
        self.scissors.clear();
        self.scissors.extend_from_slice(rects);
        if scissor_test() {
            let full = RECT {
                left: 0,
                top: 0,
                right: 16384,
                bottom: 16384,
            };
            crate::draw::scissors(list, &vec![full; rects.len()]);
            return;
        }
        crate::draw::scissors(list, rects);
    }

    pub(crate) fn set_blend_factor(&mut self, list: &HostList, factor: Option<&[f32; 4]>) {
        self.blend = factor.copied();
        crate::draw::blend_factor(list, factor);
    }

    pub(crate) fn set_stencil_ref(&mut self, list: &HostList, reference: u32) {
        self.stencil = Some(reference);
        crate::draw::stencil_ref(list, reference);
    }

    pub(crate) fn set_depth_bounds(&mut self, list: &HostList, min: f32, max: f32) {
        self.depth_bounds = Some([min, max]);
        crate::draw::depth_bounds(list, min, max);
    }

    pub(crate) fn replay(&mut self, list: &HostList) {
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [0; 2];
        if self.topology_set {
            self.set_topology(list, self.topology);
        }
        if self.targets_set {
            let handles = std::mem::take(&mut self.target_handles);
            self.set_targets(list, &handles, self.depth_handle);
        }
        if !self.viewports.is_empty() {
            crate::draw::viewports(list, &self.viewports);
        }
        if !self.scissors.is_empty() {
            let rects = std::mem::take(&mut self.scissors);
            self.set_scissors(list, &rects);
        }
        if let Some(factor) = self.blend {
            crate::draw::blend_factor(list, Some(&factor));
        }
        if let Some(reference) = self.stencil {
            crate::draw::stencil_ref(list, reference);
        }
        if let Some([min, max]) = self.depth_bounds {
            crate::draw::depth_bounds(list, min, max);
        }
    }

    fn user_value(&self, compute: bool, slot: u8, root: RootType, dword: u32) -> u32 {
        match (self.roots_ref(compute).get(usize::from(slot)), root) {
            (Some(Root::Table(handle)), RootType::Table) => match self.heap_of(*handle) {
                Some(heap) => (handle - heap.start) as u32,
                None => {
                    once!(
                        handle = format_args!("{handle:#x}"),
                        "descriptor table outside the bound heaps"
                    );
                    0
                }
            },
            (Some(Root::Constants(c)), RootType::Constants) => {
                c.get(dword as usize).copied().unwrap_or(0)
            }
            (_, RootType::Other(_)) => 0,
            _ => {
                once!(
                    slot,
                    ?root,
                    "a shader reads a root argument the title did not set"
                );
                0
            }
        }
    }

    fn host_word(
        &self,
        compute: bool,
        kind: u32,
        slot: u8,
        dword: u32,
        memory: Option<&crate::memory::Ready>,
    ) -> u32 {
        use oathkeep_shader::emit::{USER_MEMORY_BASE, USER_VIEW_ADDRESS, USER_VIEW_BIAS};
        if kind == USER_MEMORY_BASE {
            return memory.map_or(0, |m| m.base);
        }
        if kind == USER_VIEW_BIAS {
            return match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::View(address)) => crate::shadow::find(address, 1).map_or(0, |s| {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    ((s.gpu(address) - base) & 15) as u32
                }),
                _ => 0,
            };
        }
        if kind != USER_VIEW_ADDRESS && kind != USER_VIEW_ADDRESS + 1 {
            once!(kind, "a shader reads a host value this build does not know");
            return 0;
        }
        let address = match self.roots_ref(compute).get(usize::from(slot)) {
            Some(&Root::Table(table)) => crate::descriptor::view(table + u64::from(dword) * 4)
                .and_then(|view| crate::descriptor::host::buffer_range(&view))
                .map_or(0, |range| range.0),
            _ => 0,
        };
        if address == 0 {
            once!(
                slot,
                dword,
                "a shader takes the address of a buffer view that is not there: it reads 0"
            );
        }
        if kind == USER_VIEW_ADDRESS {
            address as u32
        } else {
            (address >> 32) as u32
        }
    }

    fn memory_ready(
        &mut self,
        list: &HostList,
        compute: bool,
        writes: bool,
    ) -> Option<crate::memory::Ready> {
        let heap = match self.heaps[0] {
            0 => None,
            h => crate::descriptor::heap(h as This).host(),
        }?;
        self.scratch_addresses.clear();
        self.scratch_constants.clear();
        let roots = if compute {
            &self.compute
        } else {
            &self.graphics
        };
        for root in roots {
            match root {
                Root::View(address) => self.scratch_addresses.push(*address),
                Root::Constants(values) => self.scratch_constants.extend_from_slice(values),
                _ => {}
            }
        }
        crate::memory::ready(
            list,
            heap,
            &self.scratch_addresses,
            &self.scratch_constants,
            writes,
        )
    }

    fn heap_of(&self, handle: u64) -> Option<&'static crate::descriptor::Heap> {
        self.heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| h.index(handle).is_some())
    }

    fn prepare(&mut self, list: &HostList, indexed: bool) -> bool {
        self.prepare_for(list, indexed, false)
    }

    fn prepare_for(&mut self, list: &HostList, indexed: bool, mesh: bool) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            return false;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        if p.compute || p.mesh != mesh {
            once!(
                mesh,
                "a draw or mesh dispatch with another kind of pipeline"
            );
            return false;
        }
        if p.rects && self.topology != RECTANGLES {
            once!("a rectangle pipeline drawn with another topology");
        }
        if !self.bind(list, p, false) {
            return false;
        }
        let l = &list.list;
        if p.slots != 0 {
            let limit = 32 - p.slots.leading_zeros() as usize;
            let mut views = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
            for (slot, (view, &v)) in views[..limit]
                .iter_mut()
                .zip(&self.vertex[..limit])
                .enumerate()
            {
                if p.slots & (1 << slot) == 0 || v.address == 0 || v.size == 0 {
                    continue;
                }
                let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                    continue;
                };
                shadow.refresh(list, v.address, u64::from(v.size));
                *view = D3D12_VERTEX_BUFFER_VIEW {
                    BufferLocation: shadow.gpu(v.address),
                    SizeInBytes: v.size,
                    StrideInBytes: v.stride,
                };
            }
            // SAFETY: an open list; the views name live host buffers.
            unsafe {
                l.IASetVertexBuffers(0, Some(&views[..limit]));
            }
        }
        if indexed {
            let v = self.index;
            let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                return false;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            let view = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                Format: DXGI_FORMAT(v.format as i32),
            };
            // SAFETY: an open list; the view names a live host buffer.
            unsafe { l.IASetIndexBuffer(Some(&view)) };
        }
        const RENDER_TARGET: u32 = 0x4;
        for &t in &self.targets {
            crate::list::ensure_state(list, t as This, RENDER_TARGET);
        }
        if self.depth != 0 {
            list.twin_use(self.depth as This, false, true);
            let state = if self.depth_state == DEPTH_READ {
                let (tested, read) = TESTED_DEPTH.get();
                if read && tested == self.depth {
                    DEPTH_READ | SHADER_RESOURCE
                } else {
                    DEPTH_READ
                }
            } else {
                DEPTH_WRITE
            };
            crate::list::ensure_state(list, self.depth as This, state);
        }
        true
    }

    pub(crate) fn bind(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        let l = &list.list;
        if compute || p.stages.iter().any(|s| s.translated().stores) {
            list.mark_uav_dirty();
        }
        self.early_textures(list, p, compute);
        if crate::host::walls_trace() {
            self.walls_census(p, compute);
        }
        if self.host_heaps != self.heaps {
            let mut heaps: [Option<ID3D12DescriptorHeap>; 2] = [None, None];
            let mut heap_count = 0usize;
            for &h in &self.heaps {
                if h != 0
                    && let Some(host) = crate::descriptor::heap(h as This).host()
                {
                    list.keep(&host.heap);
                    heaps[heap_count] = Some(host.heap.clone());
                    heap_count += 1;
                }
            }
            // SAFETY: an open list and shader-visible heaps.
            unsafe { l.SetDescriptorHeaps(&heaps[..heap_count]) };
            self.host_heaps = self.heaps;
            self.host_roots = [0; 2];
        }
        let tested = if !compute && self.depth_state == DEPTH_READ {
            self.depth
        } else {
            0
        };
        TESTED_DEPTH.set((tested, false));
        let root = p.root.as_raw() as usize;
        if self.host_roots[usize::from(compute)] != root {
            // SAFETY: an open list and a live root signature.
            unsafe {
                if compute {
                    l.SetComputeRootSignature(&p.root);
                } else {
                    l.SetGraphicsRootSignature(&p.root);
                }
            }
            self.host_roots[usize::from(compute)] = root;
        }
        if self.host_pipeline != p.pso.as_raw() as usize {
            // SAFETY: an open list and a live pipeline.
            unsafe { l.SetPipelineState(&p.pso) };
            self.host_pipeline = p.pso.as_raw() as usize;
            list.used_pipeline(&p.name);
            list.keep(&p.pso);
            list.keep(&p.root);
        }
        for stage in &p.stages {
            let t = stage.translated();
            STORES.set(t.stores);
            self.tables_into(list, &t.used, &t.host.root_indexed_views, compute);
            STORES.set(true);
            if self.scratch_made.iter().any(|m| m.1 == u32::MAX) {
                once!("a draw whose descriptor table found no free slots is dropped");
                return false;
            }
            let writes = (t.stores || !write_skip())
                && !crate::pipeline::builder_quarantined(self.pipeline as This);
            let memory = if t
                .host
                .root_srvs
                .contains(&oathkeep_shader::emit::SLOT_MEMORY)
            {
                let Some(ready) = self.memory_ready(list, compute, writes) else {
                    once!(
                        "a shader that reads title memory by address without a view heap to hold its views: the draw is dropped"
                    );
                    return false;
                };
                Some(ready)
            } else {
                None
            };
            if let Some((param, dwords)) = stage.user_data {
                let mut stack_values = [0u32; 64];
                let mut heap_values = Vec::new();
                let values: &mut [u32] = if (dwords as usize) <= stack_values.len() {
                    &mut stack_values[..dwords as usize]
                } else {
                    heap_values.resize(dwords as usize, 0);
                    &mut heap_values
                };
                for u in &t.host.user_data {
                    if let RootType::Other(kind) = u.root {
                        values[u.index as usize] =
                            self.host_word(compute, kind, u.slot, u.dword, memory.as_ref());
                        continue;
                    }
                    let fresh = if u.root == RootType::Table {
                        self.scratch_made.iter().find(|m| m.0 == u.slot)
                    } else {
                        None
                    };
                    values[u.index as usize] = match fresh {
                        Some(&(_, offset)) => offset,
                        None => self.user_value(compute, u.slot, u.root, u.dword),
                    };
                }
                // SAFETY: an open list; `values` covers the parameter.
                unsafe {
                    if compute {
                        l.SetComputeRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    } else {
                        l.SetGraphicsRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    }
                }
            }
            for (views, srv) in [(&stage.root_cbvs, false), (&stage.root_srvs, true)] {
                for &(slot, param) in views {
                    if srv && slot == oathkeep_shader::emit::SLOT_MEMORY {
                        if let Some(m) = &memory {
                            // SAFETY: an open list; the directory lives as
                            // long as the process and the list holds it.
                            unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, m.directory);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, m.directory);
                                }
                            }
                        }
                        continue;
                    }
                    if srv && slot == oathkeep_shader::emit::SLOT_STRIDES {
                        match self.heap_buffers(list) {
                            // SAFETY: an open list; the table lives as long
                            // as the heap the list holds.
                            Some(gpu) => unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, gpu);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, gpu);
                                }
                            },
                            None => {
                                once!("a shader picks buffers from a view heap that is not set")
                            }
                        }
                        continue;
                    }
                    let tabled = srv && crate::pipeline::host::bounded_root_srvs();
                    let Some(&Root::View(address)) = self.roots_ref(compute).get(usize::from(slot))
                    else {
                        once!(slot, "a shader reads a root view the title did not set");
                        if tabled && !self.root_srv_table(list, None, param, compute, false) {
                            return false;
                        }
                        continue;
                    };
                    let biased = srv
                        && t.host.user_data.iter().any(|u| {
                            u.slot == slot
                                && u.root == RootType::Other(oathkeep_shader::emit::USER_VIEW_BIAS)
                        });
                    if tabled {
                        if !self.root_srv_table(list, Some(address), param, compute, biased) {
                            return false;
                        }
                        continue;
                    }
                    let Some(mut gpu) = root_view(list, address, srv) else {
                        continue;
                    };
                    if biased {
                        gpu -= u64::from(self.host_word(
                            compute,
                            oathkeep_shader::emit::USER_VIEW_BIAS,
                            slot,
                            0,
                            None,
                        ));
                    }
                    // SAFETY: an open list; the address is in live memory.
                    unsafe {
                        match (compute, srv) {
                            (false, false) => l.SetGraphicsRootConstantBufferView(param, gpu),
                            (false, true) => l.SetGraphicsRootShaderResourceView(param, gpu),
                            (true, false) => l.SetComputeRootConstantBufferView(param, gpu),
                            (true, true) => l.SetComputeRootShaderResourceView(param, gpu),
                        }
                    }
                }
            }
            for &(slot, param) in &stage.root_uavs {
                if slot == oathkeep_shader::emit::SLOT_FAULTS {
                    if let Some(m) = &memory {
                        // SAFETY: an open list; the buffer lives as long as
                        // the process and the list holds it.
                        unsafe {
                            if compute {
                                l.SetComputeRootUnorderedAccessView(param, m.faults);
                            } else {
                                l.SetGraphicsRootUnorderedAccessView(param, m.faults);
                            }
                        }
                    }
                    continue;
                }
                let address = match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::View(address)) => Some(address),
                    _ => {
                        once!(
                            slot,
                            "a shader writes through a root view the title did not set"
                        );
                        None
                    }
                };
                if !self.root_uav(list, address, param, compute, writes) {
                    return false;
                }
            }
        }
        list.twins_written();
        true
    }

    fn root_uav(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        writes: bool,
    ) -> bool {
        const UNORDERED: i32 = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0;
        static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let limit = *LIMIT.get_or_init(|| {
            std::env::var("OATHKEEP_ROOT_UAV_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(u64::MAX, |mb| mb << 20)
        });
        let find = |a: u64| {
            let s = crate::shadow::find(a, 1)?;
            // SAFETY: a live buffer.
            let flags = unsafe { s.buffer.GetDesc() }.Flags;
            if flags.0 & UNORDERED == 0 {
                once!("a root UAV of a buffer whose host copy shaders cannot write");
                return None;
            }
            Some(s)
        };
        let ready = |a: u64, s: &std::sync::Arc<crate::shadow::Shadow>| {
            let len = s.left(a).min(limit);
            s.refresh(list, a, len);
            if writes {
                s.gpu_writes(list, a, len);
            }
        };
        if !crate::pipeline::host::bounded_root_srvs() {
            let Some((a, s)) = address.and_then(|a| Some((a, find(a)?))) else {
                once!("a root UAV with nothing to bind: the draw is dropped");
                return false;
            };
            ready(a, &s);
            // SAFETY: a live buffer.
            let base = unsafe { s.buffer.GetGPUVirtualAddress() };
            let gpu = base + ((s.gpu(a) - base) & !15);
            // SAFETY: an open list; the address is in a live buffer.
            unsafe {
                if compute {
                    list.list.SetComputeRootUnorderedAccessView(param, gpu);
                } else {
                    list.list.SetGraphicsRootUnorderedAccessView(param, gpu);
                }
            }
            return true;
        }
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root UAV without a bound view heap: the draw is dropped");
            return false;
        };
        let key = (std::ptr::from_ref(heap) as usize, address.unwrap_or(0));
        let index = if let Some((index, shadow)) = self.root_uav_slots.get(&key) {
            if let (Some(a), Some(s)) = (address, shadow.as_ref()) {
                ready(a, s);
            }
            *index
        } else {
            let shadow = address.and_then(|a| find(a).map(|s| (a, s)));
            let Some(index) = host.dynamic(1) else {
                return false;
            };
            self.dynamic.push((key.0, index, 1));
            if let Some((a, s)) = &shadow {
                // SAFETY: a live buffer.
                let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                let offset = s.gpu(*a) - base;
                let bias = offset & 15;
                host.raw_uav_at(
                    index,
                    &s.buffer,
                    offset - bias,
                    s.left(*a).min(limit) + bias,
                );
                ready(*a, s);
            } else {
                host.null_raw_uav_at(index);
            }
            self.root_uav_slots
                .insert(key, (index, shadow.map(|(_, s)| s)));
            index
        };
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    fn root_srv_table(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        biased: bool,
    ) -> bool {
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root SRV without a bound view heap: the draw is dropped");
            return false;
        };
        let key = (
            std::ptr::from_ref(heap) as usize,
            address.unwrap_or(0) | u64::from(biased) << 63,
        );
        let index = if let Some((index, shadow)) = self.root_srv_slots.get(&key) {
            if let (Some(a), Some(s)) = (address, shadow.as_ref()) {
                s.refresh(list, a, s.left(a));
            }
            *index
        } else {
            let shadow = address.and_then(|a| crate::shadow::find(a, 1).map(|s| (a, s)));
            let Some(index) = host.dynamic(1) else {
                return false;
            };
            self.dynamic.push((key.0, index, 1));
            match &shadow {
                Some((a, s)) => {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    let offset = s.gpu(*a) - base;
                    if biased {
                        let bias = offset & 15;
                        host.raw_at(index, &s.buffer, offset - bias, s.left(*a) + bias);
                    } else if offset.is_multiple_of(16) {
                        host.raw_at(index, &s.buffer, offset, s.left(*a));
                    } else {
                        once!(
                            address = format_args!("{a:#x}"),
                            "a root SRV not on 16 bytes reads zeros"
                        );
                        host.null_raw_at(index);
                    }
                    s.refresh(list, *a, s.left(*a));
                }
                None => host.null_raw_at(index),
            }
            self.root_srv_slots
                .insert(key, (index, shadow.map(|(_, s)| s)));
            index
        };
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    fn heap_buffers(&self, list: &HostList) -> Option<u64> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        const ONCE: usize = usize::MAX - 0x5e;
        if !list.first_use(ONCE) {
            return Some(gpu);
        }
        if let Some(buffer) = host.strides_buffer() {
            list.keep(buffer);
        }
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        let frame = crate::frame::current();
        if FRAME.load(Ordering::Relaxed) == frame || FRAME.swap(frame, Ordering::Relaxed) == frame {
            return Some(gpu);
        }
        HEAP_BUFFER_LAST_FRAME.store(frame, Ordering::Relaxed);
        let started = std::time::Instant::now();
        type Cache = BufferSpanCache<std::sync::Arc<crate::shadow::Shadow>>;
        static RANGES: std::sync::Mutex<Option<std::collections::HashMap<usize, Cache>>> =
            std::sync::Mutex::new(None);
        let mut kept = RANGES.lock().unwrap_or_else(|e| e.into_inner());
        let cache = kept
            .get_or_insert_with(Default::default)
            .entry(std::ptr::from_ref(host) as usize)
            .or_default();
        let changed = host.buffer_changes();
        static REBUILD_EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let rebuild_every = *REBUILD_EVERY.get_or_init(|| {
            std::env::var("OATHKEEP_HEAP_REBUILD_FRAMES")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(64, |n| n.max(1))
        });
        let evicted = crate::shadow::take_in_place_evicted();
        let key_of = |s: &std::sync::Arc<crate::shadow::Shadow>| std::sync::Arc::as_ptr(s) as usize;
        let full_rebuild = cache.passes % rebuild_every == 0;
        let mut slots = if full_rebuild {
            let all = host.buffer_slots();
            if !evicted.is_empty() {
                cache.clear();
                for &index in &all {
                    host.unmake_slot(index);
                }
            } else if cache.slot_count() > 0 {
                let keep: std::collections::HashSet<u32> = all.iter().copied().collect();
                cache.retain_slots(&keep, key_of);
            }
            all
        } else {
            changed
        };
        if !evicted.is_empty() && !full_rebuild {
            let stale = cache.collect_stale_slots(
                |shadow| shadow.is_in_place(),
                |from, to| {
                    evicted
                        .iter()
                        .any(|&(ev_start, ev_end)| from < ev_end && to > ev_start)
                },
            );
            for index in stale {
                host.unmake_slot(index);
                cache.set_slot_key(index, 0);
                slots.push(index);
            }
        }
        cache.passes += 1;
        static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let mut alive =
            !GONE.load(Ordering::Relaxed) && crate::descriptor::host::device_removed().is_none();
        let mirror_started = std::time::Instant::now();
        let mut todo: Vec<u32> = Vec::new();
        for &index in &slots {
            if let Some(handle) = heap.handle(index)
                && let Some(key) = host.unchanged_slot_key(index, handle)
                && cache.slot_key(index) == Some(key)
            {
                continue;
            }
            todo.push(index);
        }
        if !evicted.is_empty() && todo.len() > 1 {
            todo.sort_unstable();
            todo.dedup();
        }
        if todo.len() >= 64 {
            let worker_count = (todo.len() / 32).clamp(2, 4);
            let chunk_size = todo.len().div_ceil(worker_count);
            let heap_ptr = std::ptr::from_ref(heap) as usize;
            let host_ptr = std::ptr::from_ref(host) as usize;
            type SlotResult = (
                u32,
                Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)>,
                u64,
            );
            let mut results: Vec<std::sync::Arc<std::sync::Mutex<Vec<SlotResult>>>> =
                Vec::with_capacity(worker_count);
            let mut jobs: Vec<Box<dyn FnOnce() + Send>> = Vec::with_capacity(worker_count);
            for chunk in todo.chunks(chunk_size) {
                let chunk_vec: Vec<u32> = chunk.to_vec();
                let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::with_capacity(
                    chunk_vec.len(),
                )));
                results.push(out.clone());
                jobs.push(Box::new(move || {
                    // SAFETY: `heap` and `host` live for the entire `heap_buffers` call,
                    // `wide_heap_run` joins all workers before returning, every slot index in
                    // `todo` is distinct, and D3D12 `Create*View` is free-threaded on
                    // distinct CPU descriptor handles.
                    let heap = unsafe { &*(heap_ptr as *const crate::descriptor::Heap) };
                    let host = unsafe { &*(host_ptr as *const crate::descriptor::host::HostHeap) };
                    let mut local = Vec::with_capacity(chunk_vec.len());
                    let mut retired = Vec::new();
                    for index in chunk_vec {
                        let resolved = heap.handle(index).and_then(|handle| {
                            let mirrored = host.mirror_buffer_into(index, handle, &mut retired)?;
                            let (shadow, address, len) = mirrored.copy?;
                            #[cfg(feature = "diagnostics")]
                            if mirrored.shows == Some(false) {
                                HEAP_VIEWS_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
                            }
                            Some((shadow, address, address + len))
                        });
                        let key = if resolved.is_some() {
                            host.made_key(index)
                        } else {
                            0
                        };
                        local.push((index, resolved, key));
                    }
                    crate::host::retire_batch(retired);
                    *out.lock().unwrap_or_else(|e| e.into_inner()) = local;
                }));
            }
            crate::shadow::wide_heap_run(jobs);
            if alive && let Some(why) = crate::descriptor::host::device_removed() {
                GONE.store(true, Ordering::Relaxed);
                tracing::error!(
                    why,
                    slots = todo.len(),
                    "the host device went while buffer views of the heap were made"
                );
                crate::host::log_removal_context();
            }
            for bucket in results {
                let items = std::mem::take(&mut *bucket.lock().unwrap_or_else(|e| e.into_inner()));
                for (index, resolved, key) in items {
                    cache.set(index, resolved, key_of);
                    cache.set_slot_key(index, key);
                }
            }
        } else {
            let mut retired = Vec::new();
            for (n, &index) in todo.iter().enumerate() {
                let resolved = heap.handle(index).and_then(|handle| {
                    let mirrored = host.mirror_buffer_into(index, handle, &mut retired)?;
                    if alive
                        && n % 64 == 63
                        && let Some(why) = crate::descriptor::host::device_removed()
                    {
                        alive = false;
                        GONE.store(true, Ordering::Relaxed);
                        let last: Vec<String> = todo[n - 63..=n]
                            .iter()
                            .filter_map(|&i| {
                                crate::descriptor::view(heap.handle(i)?)
                                    .map(|v| format!("{i}: {v:?}"))
                            })
                            .map(|s| s.chars().take(200).collect())
                            .collect();
                        tracing::error!(
                            why,
                            slot = index,
                            "the host device went while buffer views of the heap were made; one of: {last:#?}"
                        );
                        crate::host::log_removal_context();
                    }
                    let (shadow, address, len) = mirrored.copy?;
                    #[cfg(feature = "diagnostics")]
                    if mirrored.shows == Some(false) {
                        HEAP_VIEWS_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
                    }
                    Some((shadow, address, address + len))
                });
                let resolved_ok = resolved.is_some();
                cache.set(index, resolved, key_of);
                cache.set_slot_key(index, if resolved_ok { host.made_key(index) } else { 0 });
            }
            crate::host::retire_batch(retired);
        }
        let mirror_us = mirror_started.elapsed().as_micros() as u64;
        let views = cache.slot_count() as u32;
        let spans_started = std::time::Instant::now();
        let (shadows, bytes, rebuilt_shadows) = cache.ensure_spans();
        let spans_us = spans_started.elapsed().as_micros() as u64;
        let copies = shadows.len();
        let refresh_started = std::time::Instant::now();
        static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let every = *EVERY.get_or_init(|| {
            std::env::var("OATHKEEP_UNWATCHED_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1)
        });
        if every <= 1 {
            for entry in shadows.values() {
                if entry.item.is_in_place() {
                    list.keep(&entry.item.buffer);
                    continue;
                }
                for &(from, to) in &entry.spans {
                    entry
                        .item
                        .refresh(list, from as u64, (to - from) as u64);
                }
            }
        } else {
            static WHOLE: std::sync::Mutex<Option<std::collections::HashMap<usize, u64>>> =
                std::sync::Mutex::new(None);
            let mut whole_guard = None;
            for entry in shadows.values() {
                if entry.item.is_in_place() {
                    list.keep(&entry.item.buffer);
                    continue;
                }
                if entry.item.unwatched() {
                    let guard = whole_guard
                        .get_or_insert_with(|| WHOLE.lock().unwrap_or_else(|e| e.into_inner()));
                    let whole = guard.get_or_insert_with(Default::default);
                    let key = entry.item.id() as usize;
                    match whole.get(&key) {
                        Some(&last) if frame < last + every => continue,
                        _ => {
                            whole.insert(key, frame);
                        }
                    }
                }
                for &(from, to) in &entry.spans {
                    entry
                        .item
                        .refresh(list, from as u64, (to - from) as u64);
                }
            }
        }
        let refresh_us = refresh_started.elapsed().as_micros() as u64;
        drop(kept);
        let buffer_us = started.elapsed().as_micros() as u64;
        let tex_started = std::time::Instant::now();
        self.heap_textures(list, heap, host);
        let tex_us = tex_started.elapsed().as_micros() as u64;
        HEAP_BUFFER_PASSES.fetch_add(1, Ordering::Relaxed);
        HEAP_BUFFER_MICROS.fetch_add(buffer_us, Ordering::Relaxed);
        HEAP_TEXTURE_MICROS.fetch_add(tex_us, Ordering::Relaxed);
        LOG_BUF_PASSES.fetch_add(1, Ordering::Relaxed);
        LOG_BUF_US.fetch_add(buffer_us, Ordering::Relaxed);
        LOG_BUF_MIRROR_US.fetch_add(mirror_us, Ordering::Relaxed);
        LOG_BUF_SPANS_US.fetch_add(spans_us, Ordering::Relaxed);
        LOG_BUF_REFRESH_US.fetch_add(refresh_us, Ordering::Relaxed);
        if rebuilt_shadows > 0 {
            LOG_BUF_REBUILDS.fetch_add(1, Ordering::Relaxed);
            LOG_BUF_REBUILT_SHADOWS.fetch_add(rebuilt_shadows as u64, Ordering::Relaxed);
        }
        LOG_BUF_SLOTS.fetch_add(slots.len() as u64, Ordering::Relaxed);
        maybe_log_heap_breakdown();
        static LOGGED: AtomicU64 = AtomicU64::new(0);
        let seen = LOGGED.load(Ordering::Relaxed);
        if u64::from(views) > seen + seen / 2 {
            LOGGED.store(u64::from(views), Ordering::Relaxed);
            tracing::info!(
                slots = slots.len(),
                views,
                copies,
                mb = bytes >> 20,
                ms = buffer_us / 1000,
                tex_ms = tex_us / 1000,
                "buffer views of the heap made ready for shaders that pick them by index"
            );
        }
        Some(gpu)
    }

    fn early_textures(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        let (early_ms, bake_ms) = early_budgets();
        if compute || (early_ms == 0 && bake_ms == 0) {
            return;
        }
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        static BAKED_US: AtomicU64 = AtomicU64::new(0);
        static BAKED_PASSES: AtomicU64 = AtomicU64::new(0);
        static IN_FLIGHT: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        const MAX_BAKE_PASSES_PER_FRAME: u64 = 16;
        let frame = crate::frame::current();
        let first = FRAME.load(Ordering::Relaxed) != frame
            && FRAME.swap(frame, Ordering::Relaxed) != frame;
        if first {
            BAKED_US.store(0, Ordering::Relaxed);
            BAKED_PASSES.store(0, Ordering::Relaxed);
        }
        let left_us = bake_ms
            .saturating_mul(1000)
            .saturating_sub(BAKED_US.load(Ordering::Relaxed));
        let bake = left_us > 0
            && BAKED_PASSES.load(Ordering::Relaxed) < MAX_BAKE_PASSES_PER_FRAME
            && p.stages
                .iter()
                .any(|s| BAKE_PIXEL_SHADERS.contains(&s.shader.hash.as_str()));
        let loading_2d = HEAP_BUFFER_PASSES.load(Ordering::Relaxed) == 0
            || frame.saturating_sub(HEAP_BUFFER_LAST_FRAME.load(Ordering::Relaxed)) > 2;
        let eff_early_ms = if loading_2d && early_ms > 0 {
            early_ms.max(20)
        } else {
            early_ms
        };
        let budget = if bake {
            PassBudget {
                ms: left_us.div_ceil(1000),
                bytes: 64 << 20,
                bake: true,
            }
        } else if first
            && eff_early_ms > 0
            && FRAME_TEX_BUDGET.remaining_us(frame, eff_early_ms.saturating_mul(1000)) > 0
        {
            PassBudget {
                ms: eff_early_ms,
                bytes: if loading_2d { 128 << 20 } else { 64 << 20 },
                bake: false,
            }
        } else {
            return;
        };
        if IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        struct InFlightGuard;
        impl Drop for InFlightGuard {
            fn drop(&mut self) {
                IN_FLIGHT.store(false, Ordering::Release);
            }
        }
        let _in_flight = InFlightGuard;
        if bake && BAKED_PASSES.fetch_add(1, Ordering::Relaxed) >= MAX_BAKE_PASSES_PER_FRAME {
            return;
        }
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return;
        };
        let Some(host) = heap.host() else {
            return;
        };
        let n = EARLY_TEXTURE_PASSES[usize::from(bake)].fetch_add(1, Ordering::Relaxed) + 1;
        if bake && n.is_power_of_two() {
            let [slots, due, ..] = HEAP_TEXTURES_QUEUE
                .each_ref()
                .map(|c| c.load(Ordering::Relaxed));
            tracing::info!(
                frame,
                bakes = n,
                queued_at_the_last_pass = slots,
                due_at_the_last_pass = due,
                uploaded_so_far = HEAP_TEXTURES.load(Ordering::Relaxed),
                "a draw of the title's decoration bake: textures picked by index are uploaded first (OATHKEEP_BAKE_TEXTURES_MS=0: not)"
            );
        }
        let began = std::time::Instant::now();
        PASS_BUDGET.set(Some(budget));
        self.heap_textures(list, heap, host);
        PASS_BUDGET.set(None);
        let elapsed_us = began.elapsed().as_micros() as u64;
        LOG_EARLY_PASSES.fetch_add(1, Ordering::Relaxed);
        LOG_EARLY_US.fetch_add(elapsed_us, Ordering::Relaxed);
        if bake {
            BAKED_US.fetch_add(elapsed_us, Ordering::Relaxed);
        }
    }

    fn card_accounting(
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        frame: u64,
        room: bool,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON
            .get_or_init(|| std::env::var_os("OATHKEEP_CARD_ACCOUNTING").is_some_and(|v| v == "1"))
        {
            return;
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms().max(1);
        let next = NEXT.load(Ordering::Relaxed);
        let by_level = crate::tiles::STATS[4].load(Ordering::Relaxed) > 0;
        let due_now = if next == 0 {
            !room || by_level
        } else {
            now >= next
        };
        if !due_now
            || NEXT
                .compare_exchange(next, now + 5000, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let mut due = [0u64; 5];
        let (mut never, mut gone) = (Vec::new(), Vec::new());
        let mut seen = std::collections::BTreeSet::new();
        for (index, uploaded) in host.texture_slots_all_due(frame) {
            due[0] += 1;
            due[1] += u64::from(uploaded);
            let state = match heap.handle(index).and_then(crate::descriptor::view) {
                Some(View::Placed(false, resource, desc)) if resource[0] != 1 => {
                    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                    match crate::resource::placed_known(address, &resource) {
                        Some(texture) => crate::resource::copy_state(texture),
                        None => Some((
                            0,
                            crate::resource::placed_bytes(&resource).unwrap_or(0),
                            address,
                        )),
                    }
                }
                Some(View::Shader(object, _)) if crate::resource::title_fills(object as This) => {
                    crate::resource::copy_state(object as This)
                }
                _ => None,
            };
            let Some((state, bytes, memory)) = state else {
                due[2] += 1;
                continue;
            };
            if !seen.insert(memory) {
                continue;
            }
            match state {
                2 => {
                    due[3] += 1;
                    due[4] += bytes;
                }
                1 => gone.push((memory, bytes)),
                _ => never.push((memory, bytes)),
            }
        }
        crate::resource::card_accounting(room, due, never, gone);
    }

    fn heap_textures(
        &self,
        list: &HostList,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
    ) {
        use crate::descriptor::host::TextureSlotUpdate;
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_HEAP_TEXTURES").is_none_or(|v| v != "0"))
        {
            return;
        }
        let frame = crate::frame::current();
        let base_ms = texture_stage_ms();
        static CATCHUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let catchup = *CATCHUP
            .get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_CATCHUP").is_some_and(|v| v == "1"));
        let pass_budget = PASS_BUDGET.get();
        let is_bake = pass_budget.is_some_and(|b| b.bake);
        if !is_bake {
            let max_budget_us = match pass_budget {
                Some(b) => b.ms.saturating_mul(1000),
                None if catchup => base_ms.saturating_mul(3000),
                None => base_ms.saturating_mul(1000),
            };
            if FRAME_TEX_BUDGET.remaining_us(frame, max_budget_us) == 0 {
                return;
            }
        }
        let total_started = std::time::Instant::now();
        static PER_FRAME_MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let per_frame: u64 = *PER_FRAME_MB.get_or_init(|| {
            std::env::var("OATHKEEP_TEXTURE_STAGE_MB")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(32)
        }) << 20;
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        static SPENT: AtomicU64 = AtomicU64::new(0);
        if FRAME.load(Ordering::Relaxed) != frame && FRAME.swap(frame, Ordering::Relaxed) != frame {
            SPENT.store(0, Ordering::Relaxed);
        }
        let mut room = speculative_room_cached(false);
        if !room {
            HEAP_TEXTURES_FULL.fetch_add(1, Ordering::Relaxed);
            once!("textures picked by index: the card has no room for more; they wait");
        }
        let mut refused = 0;
        if room {
            host.texture_slots_room(frame);
        }
        host.texture_slots_remapped(frame);
        host.texture_views_remade();
        if room {
            host.texture_slots_waiting(frame);
        }
        static PASSED: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms();
        let took = now
            .saturating_sub(PASSED.swap(now, Ordering::Relaxed))
            .max(1);
        let most = (4000 / took).clamp(16, 256);
        let spent_initial = SPENT.load(Ordering::Relaxed);
        let max_possible_bytes = match pass_budget {
            Some(b) => spent_initial.saturating_add(b.bytes),
            None if catchup => per_frame * 3,
            None => per_frame,
        };
        const MAX_PER_PASS: usize = 4096;
        const BATCH_SIZE: usize = 64;
        let unheld_mode = !room && !full_card_pass();
        let first_batch_cap = if spent_initial >= max_possible_bytes {
            0
        } else if unheld_mode {
            MAX_PER_PASS
        } else {
            BATCH_SIZE
        };
        let prep_us = total_started.elapsed().as_micros() as u64;
        let due_started = std::time::Instant::now();
        let mut batch = Vec::with_capacity(first_batch_cap.min(BATCH_SIZE));
        let census = host
            .texture_slots_due_batch(frame, first_batch_cap, true, &mut batch)
            .unwrap_or([0; 5]);
        let first_due_us = due_started.elapsed().as_micros() as u64;
        let mut extra_due_us = 0u64;
        let mut batches_taken = 1u64;
        HEAP_TEXTURES_WAITING.store(census[0], Ordering::Relaxed);
        for (count, now) in HEAP_TEXTURES_QUEUE.iter().zip(census) {
            count.store(now, Ordering::Relaxed);
        }
        Self::card_accounting(heap, host, frame, room);
        let (per_frame, exact_ms) = if catchup && census[1] >= 1024 {
            (per_frame * 3, base_ms.saturating_mul(3))
        } else {
            (per_frame, base_ms)
        };
        let (pass_span, bytes_at_most) = match pass_budget {
            Some(b) if b.bake => (
                std::time::Duration::from_millis(b.ms),
                spent_initial.saturating_add(b.bytes),
            ),
            Some(b) => (
                std::time::Duration::from_micros(
                    FRAME_TEX_BUDGET.remaining_us(frame, b.ms.saturating_mul(1000)),
                ),
                spent_initial.saturating_add(b.bytes),
            ),
            None => (
                std::time::Duration::from_micros(
                    FRAME_TEX_BUDGET.remaining_us(frame, exact_ms.saturating_mul(1000)),
                ),
                per_frame,
            ),
        };
        let sampled = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };
        let began = std::time::Instant::now();
        let mut checked_spent = spent_initial;
        let mut seen = 0usize;
        let mut mirrored = 0usize;
        let mut last_mirrored: Option<(u32, Option<View>)> = None;
        let mut stop = batch.is_empty() || total_started.elapsed() >= pass_span;
        let mut updates: Vec<TextureSlotUpdate> = Vec::with_capacity(BATCH_SIZE);
        static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        while !stop {
            let batch_len = batch.len();
            for slot in &batch {
                let (index, write) = (slot.index, slot.write);
                let spent_now = SPENT.load(Ordering::Relaxed);
                if spent_now >= bytes_at_most || total_started.elapsed() >= pass_span {
                    stop = true;
                    break;
                }
                seen += 1;
                if room && spent_now.saturating_sub(checked_spent) >= (16 << 20) {
                    checked_spent = spent_now;
                    room = speculative_room_cached(true);
                    if !room {
                        HEAP_TEXTURES_FULL.fetch_add(1, Ordering::Relaxed);
                        once!("textures picked by index: the card has no room for more; they wait");
                    }
                }
                if let Some((at, len, print, looks, uploaded)) = slot.checked {
                    if crate::texture::fingerprint(at, len) == print {
                        HEAP_TEXTURES_RECHECKED[0].fetch_add(1, Ordering::Relaxed);
                        match RECHECK_FRAMES.get(usize::from(looks) + 1) {
                            Some(wait) => updates.push(TextureSlotUpdate::Recheck {
                                index,
                                write,
                                due: frame + wait,
                                at: (at, len),
                                print,
                                looks: looks + 1,
                                uploaded,
                            }),
                            None => updates.push(TextureSlotUpdate::Done {
                                index,
                                write,
                                again: None,
                            }),
                        }
                        continue;
                    }
                    let n = HEAP_TEXTURES_RECHECKED[1].fetch_add(1, Ordering::Relaxed);
                    if n < 200 || n.is_multiple_of(1000) {
                        tracing::warn!(
                            slot = index,
                            address = format_args!("{at:#x}"),
                            bytes = len,
                            frames_after_upload = frame.saturating_sub(uploaded),
                            looks,
                            changed = n + 1,
                            "a texture picked by index changed after its upload with nothing to tell of it: uploaded again"
                        );
                    }
                }
                let handle = heap.handle(index);
                let current_view = handle.and_then(crate::descriptor::view);
                let mut partly = None;
                match current_view {
                    Some(View::Placed(false, resource, desc)) => {
                        let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                        let len = placed_bytes_cached(&resource).unwrap_or(1);
                        let mapped = crate::texture::mapped_bytes(address as *const u8, len as usize);
                        if mapped == 0 {
                            HEAP_TEXTURES_AWAY.fetch_add(1, Ordering::Relaxed);
                            host.texture_slot_gone(index);
                            updates.push(TextureSlotUpdate::Later {
                                index,
                                write,
                                frame,
                                most,
                            });
                            continue;
                        }
                        if mapped < len {
                            if mapped <= slot.mapped {
                                HEAP_TEXTURES_PARTLY[0].fetch_add(1, Ordering::Relaxed);
                                updates.push(TextureSlotUpdate::Later {
                                    index,
                                    write,
                                    frame,
                                    most,
                                });
                                continue;
                            }
                            if !crate::texture::committed(address as *const u8, 1) {
                                HEAP_TEXTURES_PARTLY[2].fetch_add(1, Ordering::Relaxed);
                            }
                            partly = Some(mapped);
                        }
                    }
                    Some(View::Shader(object, _))
                        if crate::resource::buffer_bytes(object as This).is_none()
                            && !crate::resource::title_fills(object as This) =>
                    {
                        updates.push(TextureSlotUpdate::Done {
                            index,
                            write,
                            again: None,
                        });
                        continue;
                    }
                    _ => {}
                }
                if !room {
                    let has_its_texture = reupload_when_full()
                        && handle
                            .is_some_and(|handle| host.texture_slot_made_from(index, handle));
                    if !has_its_texture {
                        refused += 1;
                        if full_card_pass() {
                            updates.push(TextureSlotUpdate::Held {
                                index,
                                write,
                                due: frame + most,
                            });
                            if refused > 1024 {
                                stop = true;
                                break;
                            }
                        } else if refused > 256 {
                            stop = true;
                            break;
                        }
                        continue;
                    }
                    let n = HEAP_TEXTURES_REUPLOADED_FULL.fetch_add(1, Ordering::Relaxed) + 1;
                    if n == 1 || n.is_multiple_of(500) {
                        tracing::info!(
                            so_far = n,
                            slot = index,
                            "a texture already on the card is brought up to date although the card has no room for new ones"
                        );
                    }
                }
                if total_started.elapsed() >= pass_span {
                    stop = true;
                    break;
                }
                let mirrored_tex = handle.and_then(|handle| host.mirror_texture(index, handle));
                let (view, placed) = match mirrored_tex {
                    Some((v, p)) => (Some(v), p),
                    None => (None, None),
                };
                let resolved = placed.as_ref().or(view.as_ref());
                let resource = match resolved {
                    Some(&View::Shader(resource, _)) => resource as This,
                    _ => {
                        updates.push(TextureSlotUpdate::Done {
                            index,
                            write,
                            again: None,
                        });
                        continue;
                    }
                };
                let title_fills =
                    crate::resource::host_resource(resource).is_some_and(crate::texture::title_written);
                if !title_fills {
                    updates.push(TextureSlotUpdate::Done {
                        index,
                        write,
                        again: None,
                    });
                    continue;
                }
                let tabled = crate::texture::is_staged(resource);
                if let Some(resolved_view) = resolved {
                    readable(list, sampled.kind, resolved_view);
                }
                mirrored += 1;
                if !GONE.load(Ordering::Relaxed) {
                    if mirrored % 64 == 0 {
                        if let Some(why) = crate::descriptor::host::device_removed() {
                            GONE.store(true, Ordering::Relaxed);
                            let text: String = format!("{view:?}").chars().take(300).collect();
                            tracing::error!(
                                why,
                                slot = index,
                                "the host device went while textures picked by index were made ready; last: {text}"
                            );
                            crate::host::log_removal_context();
                            host.texture_apply_updates(&mut updates);
                            FRAME_TEX_BUDGET
                                .record(frame, total_started.elapsed().as_micros() as u64);
                            return;
                        }
                        last_mirrored = None;
                    } else {
                        last_mirrored = Some((index, view));
                    }
                }
                if tabled && let Some(mapped) = partly {
                    HEAP_TEXTURES_PARTLY[1].fetch_add(1, Ordering::Relaxed);
                    HEAP_TEXTURES_TABLED.fetch_add(1, Ordering::Relaxed);
                    updates.push(TextureSlotUpdate::Done {
                        index,
                        write,
                        again: Some((frame + 1, mapped)),
                    });
                    continue;
                }
                let (bytes, whole) = crate::texture::staged(resource).unwrap_or((0, true));
                let again = match partly {
                    Some(mapped) => Some((frame + 1, mapped)),
                    None => (!whole).then_some((frame + 120, 0)),
                };
                if again.is_some() {
                    HEAP_TEXTURES_PARTLY[1].fetch_add(1, Ordering::Relaxed);
                }
                if tabled {
                    HEAP_TEXTURES_TABLED.fetch_add(1, Ordering::Relaxed);
                    updates.push(TextureSlotUpdate::Done {
                        index,
                        write,
                        again,
                    });
                    continue;
                }
                let memory = if again.is_none() && heap_recheck() {
                    crate::texture::staged_memory(resource)
                } else {
                    None
                };
                crate::texture::unstage(resource);
                SPENT.fetch_add(bytes, Ordering::Relaxed);
                HEAP_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
                HEAP_TEXTURES.fetch_add(1, Ordering::Relaxed);
                match memory {
                    Some((at, len)) => {
                        let print = crate::texture::fingerprint(at, len);
                        updates.push(TextureSlotUpdate::Recheck {
                            index,
                            write,
                            due: frame + RECHECK_FRAMES[0],
                            at: (at, len),
                            print,
                            looks: 0,
                            uploaded: frame,
                        });
                    }
                    None => updates.push(TextureSlotUpdate::Done {
                        index,
                        write,
                        again,
                    }),
                }
            }
            if stop
                || unheld_mode
                || (!room && !full_card_pass())
                || seen >= MAX_PER_PASS
                || batch_len < BATCH_SIZE
                || SPENT.load(Ordering::Relaxed) >= bytes_at_most
                || total_started.elapsed() >= pass_span
            {
                break;
            }
            let next_cap = BATCH_SIZE.min(MAX_PER_PASS - seen);
            let next_due = std::time::Instant::now();
            host.texture_batch_step(frame, next_cap, &mut updates, &mut batch);
            extra_due_us += next_due.elapsed().as_micros() as u64;
            batches_taken += 1;
            if batch.is_empty() {
                break;
            }
        }
        host.texture_apply_updates(&mut updates);
        if let Some((index, view)) = last_mirrored
            && !GONE.load(Ordering::Relaxed)
            && let Some(why) = crate::descriptor::host::device_removed()
        {
            GONE.store(true, Ordering::Relaxed);
            let text: String = format!("{view:?}").chars().take(300).collect();
            tracing::error!(
                why,
                slot = index,
                "the host device went while textures picked by index were made ready; last: {text}"
            );
            crate::host::log_removal_context();
        }
        let loop_us = began.elapsed().as_micros() as u64;
        let total_us = total_started.elapsed().as_micros() as u64;
        FRAME_TEX_BUDGET.record(frame, total_us);
        LOG_TEX_PASSES.fetch_add(1, Ordering::Relaxed);
        LOG_TEX_US.fetch_add(total_us, Ordering::Relaxed);
        LOG_TEX_PREP_US.fetch_add(prep_us, Ordering::Relaxed);
        LOG_TEX_DUE_US.fetch_add(first_due_us + extra_due_us, Ordering::Relaxed);
        LOG_TEX_LOOP_US.fetch_add(loop_us.saturating_sub(extra_due_us), Ordering::Relaxed);
        LOG_TEX_BATCHES.fetch_add(batches_taken, Ordering::Relaxed);
        LOG_TEX_SEEN.fetch_add(seen as u64, Ordering::Relaxed);
    }

    #[cfg(feature = "diagnostics")]
    fn list_dropped_roots(&self, groups: [u32; 3]) {
        crate::capture::op(0, || {
            use std::fmt::Write;
            let [x, y, z] = groups;
            let mut text = format!("  its roots ({x}x{y}x{z}):");
            for (slot, root) in self.compute.iter().enumerate() {
                match root {
                    Root::None => {}
                    Root::Table(a) => {
                        let _ = write!(text, " {slot}=table {a:#x}");
                    }
                    Root::Constants(v) => {
                        let _ = write!(text, " {slot}=constants {:x?}", &v[..v.len().min(48)]);
                    }
                    Root::View(a) => {
                        let _ = write!(text, " {slot}=view {a:#x}");
                        match crate::shadow::extent(*a) {
                            Some((start, len)) => {
                                let _ = write!(text, " in buffer {start:#x}+{len:#x}");
                            }
                            None => text.push_str(" in no buffer"),
                        }
                    }
                }
            }
            text
        });
    }

    pub(crate) fn dispatch(&mut self, list: &HostList, x: u32, y: u32, z: u32) {
        self.windows.clear();
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop(self.pipeline as This);
            return;
        }
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_why(
                    "a stage of the ray-tracing structure builder, which is off".to_owned(),
                );
                self.list_dropped_roots([x, y, z]);
            }
            return;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            once!(
                pipeline = format_args!("{:#x}", self.pipeline),
                "a dispatch without a host pipeline dropped"
            );
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_drop(self.pipeline as This);
                self.list_dropped_roots([x, y, z]);
            }
            return;
        };
        if !p.compute {
            once!("a dispatch with a graphics pipeline");
            return;
        }
        if !self.bind(list, p, true) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            if crate::pipeline::is_builder(self.pipeline as This) {
                crate::pipeline::builder_off("one of its dispatches could not be bound");
            }
            return;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| self.op_text(format!("dispatch {x}x{y}x{z}"), true));
        // SAFETY: an open list with everything set.
        unsafe { list.list.Dispatch(x, y, z) };
        DISPATCHED.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(feature = "diagnostics")]
    fn op_text(&self, kind: String, compute: bool) -> String {
        use std::fmt::Write;
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return kind;
        };
        let mut text = format!("{kind} {}", p.name);
        if !compute {
            let described = |t: usize| {
                crate::resource::host_made(t as This).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{t:#x} {}x{} f{}", d.Width, d.Height, d.Format.0)
                })
            };
            let targets: Vec<String> = self.targets.iter().filter_map(|&t| described(t)).collect();
            let _ = write!(
                text,
                " targets {targets:?} depth {:?} state [{}]",
                described(self.depth),
                p.state
            );
            if let Some(r) = self.scissors.first() {
                let _ = write!(
                    text,
                    " scissor [{} {} {} {}]",
                    r.left, r.top, r.right, r.bottom
                );
            }
            if let Some(v) = self.viewports.first() {
                let _ = write!(
                    text,
                    " viewport [{} {} {} {} depth {}..{}]",
                    v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth
                );
            }
        }
        let views = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten();
        for stage in &p.stages {
            let t = stage.translated();
            let _ = write!(text, " | {}:", stage.shader.hash);
            for u in &t.host.user_data {
                let v = match u.root {
                    RootType::Other(kind) => self.host_word(compute, kind, u.slot, u.dword, None),
                    _ => self.user_value(compute, u.slot, u.root, u.dword),
                };
                let _ = write!(text, " u{}={v:#x}", u.index);
                let index = if u.root == RootType::Table { v / 32 } else { v };
                if let Some(slot) = views.and_then(|h| h.describe_slot(index)) {
                    let _ = write!(text, " [heap {index}: {slot}]");
                }
            }
            for &(slot, _) in stage
                .root_cbvs
                .iter()
                .chain(&stage.root_srvs)
                .chain(&stage.root_uavs)
            {
                if let Some(Root::View(address)) = self.roots_ref(compute).get(usize::from(slot)) {
                    let host = crate::shadow::find(*address, 1)
                        .map(|s| (s.gpu(*address), s.left(*address)));
                    let _ = write!(text, " root{slot}={address:#x} host {host:#x?}");
                }
            }
        }
        let _ = write!(text, " tables {:?}", self.reads(compute));
        if !compute && !list_slots().is_empty() {
            text.push_str(&self.slots_text());
        }
        text
    }

    #[cfg(feature = "diagnostics")]
    fn slots_text(&self) -> String {
        use std::fmt::Write;
        const MATERIALS: u32 = 160;
        const GLOBAL: u32 = 688;
        const DRAW_CONSTANTS: u32 = 56;
        let wanted = list_slots();
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return String::new();
        };
        let Some(stage) = p
            .stages
            .iter()
            .find(|s| wanted.iter().any(|w| s.shader.hash.starts_with(w.as_str())))
        else {
            return String::new();
        };
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return String::new();
        };
        let t = stage.translated();
        let mut text = String::new();
        let entry = |slot: u8, dwords: u32| -> Option<View> {
            let Some(Root::Table(table)) = self.roots_ref(false).get(usize::from(slot)) else {
                return None;
            };
            crate::descriptor::view(table + u64::from(dwords) * 4)
        };
        let mut material = None;
        for u in t
            .used
            .iter()
            .filter(|u| u.root == RootType::Table && u.kind != ResourceType::Sampler)
        {
            let label = format!("s{}+{}", u.slot, u.table_offset_dwords);
            let Some(view) = entry(u.slot, u.table_offset_dwords) else {
                let _ = write!(text, "\n    ~table {label} no view");
                continue;
            };
            let range = match &view {
                View::Constant(address, size) => Some((*address, u64::from(*size))),
                other => crate::descriptor::host::buffer_range(other),
            };
            match range {
                Some((address, len)) if len <= 2048 => {
                    Self::slot_words(&mut text, heap, &label, address, len as usize);
                    if u.slot == 1 && u.table_offset_dwords == DRAW_CONSTANTS && len >= 0x20 {
                        let mut word = [0u8; 4];
                        if crate::host::read_into((address + 0x1c) as *const u8, &mut word) {
                            material = Some(u32::from_le_bytes(word));
                        }
                    }
                }
                Some(_) => {}
                None => {
                    let Some(Root::Table(table)) = self.roots_ref(false).get(usize::from(u.slot))
                    else {
                        continue;
                    };
                    match heap.index(table + u64::from(u.table_offset_dwords) * 4) {
                        Some(index) => text.push_str(&Self::slot_line(
                            heap,
                            &format!("table:{label}"),
                            0,
                            index,
                        )),
                        None => {
                            let _ = write!(
                                text,
                                "\n    ~table {label} title=[{}]",
                                view_state(Some(&view))
                            );
                        }
                    }
                }
            }
        }
        let uses = |slot: u8, dwords: u32| {
            t.used.iter().any(|u| {
                u.root == RootType::Table && u.slot == slot && u.table_offset_dwords == dwords
            })
        };
        if let Some(at) = material
            && uses(0, MATERIALS)
            && let Some((base, len)) = entry(0, MATERIALS)
                .as_ref()
                .and_then(crate::descriptor::host::buffer_range)
        {
            let at = u64::from(at);
            let _ = write!(
                text,
                "\n    ~material record at {at:#x} of the buffer at {base:#x} bytes {len}"
            );
            if at + 64 <= len {
                Self::slot_words(&mut text, heap, "M", base + at, 64);
            }
            if at + 1936 + 512 <= len {
                Self::slot_words(&mut text, heap, "M+1936", base + at + 1936, 512);
            }
        }
        if uses(0, GLOBAL)
            && let Some(slot) = t
                .used
                .iter()
                .find(|u| u.root == RootType::Constants)
                .map(|u| u.slot)
            && let Some(Root::Constants(words)) = self.roots_ref(false).get(usize::from(slot))
            && let Some(&at) = words.get(1)
            && let Some((base, len)) = entry(0, GLOBAL)
                .as_ref()
                .and_then(crate::descriptor::host::buffer_range)
        {
            let at = u64::from(at);
            let _ = write!(
                text,
                "\n    ~root constants {words:08x?}: record at {at:#x} of the buffer at {base:#x} bytes {len}"
            );
            if at + 64 <= len {
                Self::slot_words(&mut text, heap, "G", base + at, 64);
            }
        }
        text
    }

    #[cfg(feature = "diagnostics")]
    fn slot_words(
        text: &mut String,
        heap: &crate::descriptor::Heap,
        label: &str,
        address: u64,
        len: usize,
    ) {
        use std::fmt::Write;
        let mut bytes = vec![0u8; len.min(512) & !3];
        if bytes.is_empty() || !crate::host::read_into(address as *const u8, &mut bytes) {
            let _ = write!(
                text,
                "\n    ~raw {label} at {address:#x} bytes {len}: unreadable"
            );
            return;
        }
        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let _ = write!(text, "\n    ~raw {label} at {address:#x} bytes {len}:");
        for w in &words {
            let _ = write!(text, " {w:08x}");
        }
        for (i, &index) in words.iter().enumerate() {
            let names = index != 0
                && heap
                    .handle(index)
                    .and_then(crate::descriptor::view)
                    .as_ref()
                    .is_some_and(crate::descriptor::host::is_texture_view);
            if names {
                text.push_str(&Self::slot_line(heap, label, i * 4, index));
            }
        }
    }

    #[cfg(feature = "diagnostics")]
    fn slot_line(heap: &crate::descriptor::Heap, from: &str, at: usize, index: u32) -> String {
        let title = heap.handle(index).and_then(crate::descriptor::view);
        let host: String = heap
            .host()
            .and_then(|h| h.describe_slot(index))
            .unwrap_or_else(|| "none".into())
            .chars()
            .take(90)
            .collect();
        let queue = match heap.host() {
            Some(h) => h.texture_slot_state(index, crate::frame::current()),
            None => "no host heap".into(),
        };
        format!(
            "\n    ~slot from={from}+{at:#x} slot={index} title=[{}] host=[{host}] queue=[{queue}]",
            view_state(title.as_ref())
        )
    }

    fn mesh_scissor(&self, list: &HostList, on: bool) {
        if mesh_test() != 2 {
            return;
        }
        if on {
            let one = RECT {
                left: 0,
                top: 0,
                right: 1,
                bottom: 1,
            };
            crate::draw::scissors(list, &vec![one; self.scissors.len().max(1)]);
        } else if !self.scissors.is_empty() {
            crate::draw::scissors(list, &self.scissors);
        }
    }

    pub(crate) fn dispatch_mesh(&mut self, list: &HostList, x: u32) {
        #[cfg(feature = "diagnostics")]
        self.trace("mesh", x, 0);
        let ran = mesh_test() != 1
            && mesh_kept(self.pipeline)
            && self.prepare_for(list, false, true)
            && match list.list.cast::<ID3D12GraphicsCommandList6>() {
                Ok(l6) => {
                    #[cfg(feature = "diagnostics")]
                    list.note_op(|| self.op_text(format!("mesh {x}"), false));
                    self.mesh_scissor(list, true);
                    // SAFETY: an open list with everything set.
                    unsafe { l6.DispatchMesh(x, 1, 1) };
                    self.mesh_scissor(list, false);
                    DRAWN.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            };
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }

    fn walls_census(&self, p: &HostPipeline, compute: bool) {
        const CULL: [(&str, &str); 9] = [
            ("55679a0547649d0b", "tree"),
            ("89d08e561897d4e5", "tree_imposter"),
            ("9209777e8face9dd", "grass"),
            ("17af15c4015cb4b6", "prop"),
            ("fdd56d97a806bcca", "prop_nocull"),
            ("42f717f1b4d18e94", "simple"),
            ("1c4b3c0547e3e6fa", "slod"),
            ("9146291b38ea9ce1", "meshlet_args"),
            ("42683eebec081520", "depth_pyramid_cpu_occlusion"),
        ];
        type Census = (u64, std::collections::BTreeMap<[i32; 4], u32>, [u32; 9]);
        static CENSUS: std::sync::Mutex<Census> =
            std::sync::Mutex::new((u64::MAX, std::collections::BTreeMap::new(), [0; 9]));
        let frame = crate::frame::current();
        let mut census = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
        if census.0 != frame {
            let draws: u32 = census.1.values().sum();
            if draws > 0 {
                let mut rects: Vec<([i32; 4], u32)> =
                    census.1.iter().map(|(r, n)| (*r, *n)).collect();
                rects.sort_unstable_by(|a, b| b.1.cmp(&a.1));
                let scissors: Vec<String> = rects
                    .iter()
                    .take(5)
                    .map(|(r, n)| format!("{} {} {} {} x{n}", r[0], r[1], r[2], r[3]))
                    .collect();
                let counts = census.2;
                let culling: Vec<String> = CULL
                    .iter()
                    .zip(counts)
                    .filter(|(_, n)| *n > 0)
                    .map(|(c, n)| format!("{}={n}", c.1))
                    .collect();
                tracing::info!(
                    frame = census.0,
                    main_pass_draws = draws,
                    rectangles = rects.len(),
                    scissors = ?scissors,
                    culling = ?culling,
                    "walls trace: the frame's main pass by scissor rectangle, and its culling dispatches"
                );
            }
            census.0 = frame;
            census.1.clear();
            census.2 = [0; 9];
        }
        if compute {
            for (i, c) in CULL.iter().enumerate() {
                if p.stages.iter().any(|s| s.shader.hash.as_str() == c.0) {
                    census.2[i] += 1;
                }
            }
        } else if self.targets.iter().filter(|&&t| t != 0).count() >= 4 {
            let r = self
                .scissors
                .first()
                .map_or([0; 4], |r| [r.left, r.top, r.right, r.bottom]);
            *census.1.entry(r).or_insert(0) += 1;
        }
    }

    fn roots_ref(&self, compute: bool) -> &Vec<Root> {
        if compute {
            &self.compute
        } else {
            &self.graphics
        }
    }

    fn table_entry(&self, list: &HostList, u: &UsedResource, compute: bool) {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
            return;
        };
        let handle = table + u64::from(u.table_offset_dwords) * 4;
        let Some(heap) = self.heap_of(handle) else {
            return;
        };
        let (Some(host), Some(index)) = (heap.host(), heap.index(handle)) else {
            return;
        };
        if let Some(view) = host.mirror(index, handle, true) {
            readable(list, u.kind, &view);
            if crate::descriptor::host::is_texture_view(&view) {
                host.texture_slot_tabled(index, true);
            }
        }
    }

    fn tables_into(
        &mut self,
        list: &HostList,
        used: &[UsedResource],
        indexed: &[RootIndexedView],
        compute: bool,
    ) {
        self.scratch_fresh.clear();
        self.scratch_made.clear();
        for u in used {
            if u.root != RootType::Table {
                continue;
            }
            if u.kind == ResourceType::Cbv {
                if !self.scratch_fresh.contains(&u.slot) {
                    self.scratch_fresh.push(u.slot);
                }
                continue;
            }
            if self.scratch_fresh.contains(&u.slot) {
                continue;
            }
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
                continue;
            };
            let end = table + u64::from(u.table_offset_dwords) * 4;
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            let (Some(host), Some(index)) = (heap.host(), heap.index(end)) else {
                continue;
            };
            if index >= host.count() {
                self.scratch_fresh.push(u.slot);
            }
        }
        self.scratch_fresh.sort_unstable();
        self.scratch_fresh.dedup();
        for u in used {
            if u.root == RootType::Table && !self.scratch_fresh.contains(&u.slot) {
                self.table_entry(list, u, compute);
            }
        }
        for fresh_i in 0..self.scratch_fresh.len() {
            let slot = self.scratch_fresh[fresh_i];
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
                continue;
            };
            self.scratch_entries.clear();
            for (idx, u) in used.iter().enumerate() {
                if u.root == RootType::Table && u.slot == slot {
                    self.scratch_entries.push(idx);
                }
            }
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            if heap.samplers() || (table - heap.start) % 32 != 0 {
                continue;
            }
            let Some(host) = heap.host() else {
                continue;
            };
            self.scratch_selected.clear();
            for index in indexed {
                if index.table_slot != slot {
                    continue;
                }
                let constant = self.user_value(
                    compute,
                    index.constant_slot,
                    RootType::Constants,
                    index.constant_dword,
                );
                self.scratch_selected
                    .push(index.byte_offset.wrapping_add(constant << index.shift) / 32);
            }
            self.scratch_selected.sort_unstable();
            self.scratch_selected.dedup();
            let n = self
                .scratch_entries
                .iter()
                .map(|&idx| used[idx].table_offset_dwords / 8)
                .chain(self.scratch_selected.last().copied())
                .max()
                .unwrap_or(0)
                + 1;
            let last = table.checked_add(u64::from(n - 1) * 32);
            if n > crate::descriptor::host::DYNAMIC
                || last.and_then(|handle| heap.index(handle)).is_none()
            {
                once!(slot, n, "fresh descriptor table exceeds the available span");
                continue;
            }
            let key = {
                let mut h1 = 0xcbf2_9ce4_8422_2325_u64 ^ u64::from(n);
                let mut h2 = 0x517c_c1b7_2722_0a95_u64 ^ table;
                for k in 0..n {
                    let at = table + u64::from(k) * 32;
                    // SAFETY: a descriptor slot in title memory.
                    let words = unsafe { (at as *const [u32; 3]).read_unaligned() };
                    hash_table_words(&words, &mut h1, &mut h2);
                }
                (table, n, finalize_table_hash(h1, h2))
            };
            if let Some(&first) = self.fresh_tables.get(&key) {
                for &idx in &self.scratch_entries {
                    let u = &used[idx];
                    let handle = table + u64::from(u.table_offset_dwords) * 4;
                    if u.kind == ResourceType::Cbv {
                        if direct_cbv()
                            && let Some(View::Constant(address, size)) =
                                crate::descriptor::view(handle)
                            && let Some(shadow) = crate::shadow::find(address, u64::from(size))
                        {
                            let gpu = shadow.gpu(address);
                            let aligned = size.next_multiple_of(256);
                            if gpu % 256 == 0 && shadow.left(address) >= u64::from(aligned) {
                                shadow.refresh(list, address, u64::from(size));
                            }
                        }
                        continue;
                    }
                    if let Some(view) = crate::descriptor::view(handle) {
                        readable(list, u.kind, &view);
                    }
                }
                for &k in &self.scratch_selected {
                    if self
                        .scratch_entries
                        .iter()
                        .any(|&idx| used[idx].table_offset_dwords / 8 == k)
                    {
                        continue;
                    }
                    let handle = table + u64::from(k) * 32;
                    if let Some(view) = crate::descriptor::view(handle)
                        && let Some(kind) = view_kind(&view)
                    {
                        readable(list, kind, &view);
                    }
                }
                self.scratch_made.push((slot, first * 32));
                continue;
            }
            let Some(first) = host.dynamic(n) else {
                self.scratch_made.push((slot, u32::MAX));
                continue;
            };
            self.fresh_tables.insert(key, first);
            self.dynamic
                .push((std::ptr::from_ref(heap) as usize, first, n));
            let title_first = heap.index(table).filter(|first| first + n <= host.count());
            for k in 0..n {
                let index = first + k;
                let used_entry = self
                    .scratch_entries
                    .iter()
                    .map(|&idx| &used[idx])
                    .find(|u| u.table_offset_dwords / 8 == k);
                match crate::descriptor::view(table + u64::from(k) * 32) {
                    Some(View::Constant(address, size)) => {
                        if direct_cbv()
                            && let Some(shadow) = crate::shadow::find(address, u64::from(size))
                        {
                            let gpu = shadow.gpu(address);
                            let aligned = size.next_multiple_of(256);
                            if gpu % 256 == 0 && shadow.left(address) >= u64::from(aligned) {
                                shadow.refresh(list, address, u64::from(size));
                                host.constant_at_kept(index, gpu, size, shadow.buffer.clone());
                                continue;
                            }
                        }
                        let Some((gpu, to)) = list.scratch(u64::from(size)) else {
                            host.make_null(index);
                            continue;
                        };
                        list.defer_fill(to, address as *const u8, size as usize);
                        host.constant_at(index, gpu, size);
                    }
                    Some(view) => {
                        host.make_at(index, &view);
                        let kind = used_entry.map(|u| u.kind).or_else(|| {
                            self.scratch_selected
                                .binary_search(&k)
                                .ok()
                                .and_then(|_| view_kind(&view))
                        });
                        if let Some(kind) = kind {
                            readable(list, kind, &view);
                        }
                        if let Some(title) = title_first
                            && crate::descriptor::host::is_texture_view(&view)
                        {
                            host.texture_slot_tabled(title + k, kind.is_some());
                            if kind.is_some() {
                                host.mirror(title + k, table + u64::from(k) * 32, true);
                            }
                        }
                    }
                    None => {
                        host.make_null(index);
                        if used_entry.is_some() {
                            once!(slot, "a table entry without a view");
                        }
                    }
                }
            }
            self.scratch_made.push((slot, first * 32));
        }
    }

    #[cfg(feature = "diagnostics")]
    fn trace(&self, kind: &str, count: u32, first: u32) {
        static TRACED: AtomicU64 = AtomicU64::new(0);
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let listed = crate::capture::listed(crate::queue::presented() + 1);
        let logged = listed || TRACED.fetch_add(1, Ordering::Relaxed) < 64;
        let targets: Vec<String> = self
            .targets
            .iter()
            .filter_map(|&t| crate::resource::host_made(t as This))
            .map(|t| {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                format!("{}x{} f{}", d.Width, d.Height, d.Format.0)
            })
            .collect();
        let v = self.vertex[0];
        let line = format!(
            "{kind} {count} from {first} topology {} pipeline {:#x} targets {targets:?} vb {:#x}+{} stride {} reads {:?}",
            self.topology,
            self.pipeline,
            v.address,
            v.size,
            v.stride,
            self.reads(false)
        );
        if logged {
            tracing::debug!("draw {line}");
        }
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        if frame.len() < 256 {
            frame.push(line);
        }
    }

    #[cfg(feature = "diagnostics")]
    fn reads(&self, compute: bool) -> Vec<String> {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return vec!["no host pipeline".into()];
        };
        let mut out = Vec::new();
        for stage in &p.stages {
            let t = stage.translated();
            for u in &t.used {
                let what = match (u.root, self.roots_ref(compute).get(usize::from(u.slot))) {
                    (RootType::Table, Some(Root::Table(table))) => {
                        let handle = table + u64::from(u.table_offset_dwords) * 4;
                        describe(crate::descriptor::view(handle).as_ref())
                    }
                    (root, _) => format!("{root:?}"),
                };
                out.push(format!(
                    "{} {:?} s{}+{}: {what}",
                    stage.shader.hash, u.kind, u.slot, u.table_offset_dwords
                ));
            }
        }
        out
    }

    pub(crate) fn draw(
        &mut self,
        list: &HostList,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        #[cfg(feature = "diagnostics")]
        self.trace("draw", vertices, first);
        let ready = self.prepare(list, false);
        let rects = self.topology == RECTANGLES && !self.completes_rects();
        let ran = ready && (!rects || self.rectangles(list, vertices, first));
        if ran {
            let (count, from) = if rects {
                (vertices / 3 * 6, 0)
            } else {
                (vertices, first)
            };
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("draw {count}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawInstanced(count, instances, from, first_instance)
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }

    fn completes_rects(&self) -> bool {
        crate::pipeline::host_pipeline(self.pipeline as This).is_some_and(|p| p.rects)
    }

    fn rectangles(&self, list: &HostList, count: u32, first: u32) -> bool {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        let rects = u64::from(count / 3);
        for slot in 0..32 {
            let elements: Vec<(u32, u32)> = p
                .inputs
                .iter()
                .filter(|e| e.slot == slot && !e.per_instance)
                .map(|e| (e.offset, e.format))
                .collect();
            if elements.is_empty() {
                continue;
            }
            let v = self.vertex[slot as usize];
            let stride = u64::from(v.stride);
            let from = v.address + u64::from(first) * stride;
            if v.address == 0
                || stride == 0
                || u64::from(first + count) * stride > u64::from(v.size)
                || crate::shadow::find(from, rects * 3 * stride).is_none()
            {
                once!(slot, "rectangle vertices outside their buffer");
                return false;
            }
            let Some((gpu, to)) = list.scratch(rects * 6 * stride) else {
                return false;
            };
            let (from, to, stride) = (from as usize, to as usize, stride as usize);
            let n = rects as usize;
            list.defer_host(move || {
                let _mapped = crate::watch::title_memory_reading();
                // SAFETY: the vertices lie in a title buffer (checked when
                // recorded) and the scratch memory is the list's.
                let (src, dst) = unsafe {
                    (
                        std::slice::from_raw_parts(from as *const u8, n * 3 * stride),
                        std::slice::from_raw_parts_mut(to as *mut u8, n * 6 * stride),
                    )
                };
                crate::primitive::expand_rects(src, dst, stride, &elements);
            });
            let view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: gpu,
                SizeInBytes: (rects * 6 * stride as u64) as u32,
                StrideInBytes: v.stride,
            };
            // SAFETY: an open list; the view names live scratch memory.
            unsafe { list.list.IASetVertexBuffers(slot, Some(&[view])) };
        }
        true
    }

    pub(crate) fn draw_indexed(
        &mut self,
        list: &HostList,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        #[cfg(feature = "diagnostics")]
        self.trace("indexed", indices, first);
        let lost = self.topology == RECTANGLES && !self.completes_rects();
        if lost {
            once!("indexed rectangles not drawn");
        }
        let ran = !lost && self.prepare(list, true);
        if ran {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("indexed {indices}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawIndexedInstanced(indices, instances, first, base, first_instance)
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }
}

impl State {
    pub(crate) fn execute_indirect(
        &mut self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        }
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            return false;
        }
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        const DISPATCH: u32 = 2;
        const MESH: u32 = 10;
        #[cfg(feature = "diagnostics")]
        let why = |what: &str| crate::pipeline::note_why(format!("indirect kind {kind}: {what}"));
        #[cfg(not(feature = "diagnostics"))]
        let why = |_: &str| {};
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        };
        let made_rects = !p.rects
            && kind == DRAW
            && self.topology == RECTANGLES
            && !p.inputs.iter().any(|e| !e.per_instance);
        if made_rects {
            once!("indirect rectangles without vertex inputs drawn as their first triangle");
        }
        let matches_kind = match kind {
            DRAW | INDEXED => {
                (self.topology != RECTANGLES || made_rects || p.rects) && !p.compute && !p.mesh
            }
            MESH => !p.compute && p.mesh,
            DISPATCH => p.compute,
            _ => false,
        };
        if !matches_kind {
            why(&format!(
                "pipeline mismatch (compute {}, mesh {}, topology {})",
                p.compute, p.mesh, self.topology
            ));
            return false;
        }
        let len = u64::from(stride) * u64::from(max.max(1));
        let Some(args) = crate::shadow::find_any(arguments, len) else {
            why("arguments outside title buffers");
            return false;
        };
        let counter = match count {
            Some(at) => match crate::shadow::find_any(at, 4) {
                Some(s) => Some((s, at)),
                None => {
                    why("count outside title buffers");
                    return false;
                }
            },
            None => None,
        };
        let Some(signature) = matches!(kind, DRAW | INDEXED | DISPATCH | MESH)
            .then(|| host_signature(kind, stride))
            .flatten()
        else {
            why("no host signature");
            return false;
        };
        if kind == MESH && (mesh_test() == 1 || !mesh_kept(self.pipeline)) {
            why("mesh draws are switched off (OATHKEEP_MESH_TEST=skip)");
            return false;
        }
        const WINDOW_USE: usize = usize::MAX - 0x77;
        let offset = args.offset(arguments);
        let epoch = list.indirect_epoch();
        let windowed = windows() && max == 1 && count.is_none();
        if list.first_use(WINDOW_USE) {
            self.windows.clear();
        } else {
            self.windows.retain(|w| w.epoch == epoch);
        }
        let same = |w: &Window| w.copy == args.id() && w.kind == kind && w.stride == stride;
        let hit = windowed
            .then(|| {
                self.windows
                    .iter()
                    .rev()
                    .find(|w| {
                        same(w)
                            && offset >= w.base
                            && (offset - w.base) % u64::from(w.step) == 0
                            && (offset - w.base) / u64::from(w.step) < u64::from(w.records)
                    })
                    .map(|w| {
                        let idx = (offset - w.base) / u64::from(w.step);
                        (w.scratch.clone(), w.dst_offset + idx * u64::from(w.stride))
                    })
            })
            .flatten();
        let (bounded, at) = match hit {
            Some(hit) => {
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::IndHits, 1);
                hit
            }
            None => {
                let prev_window = windowed
                    .then(|| self.windows.iter().rev().find(|w| same(w)))
                    .flatten();
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    if !windowed {
                        if kind == DISPATCH {
                            add(C::IndNonWinDispatch, 1);
                        } else if max > 1 {
                            add(C::IndNonWinMax, 1);
                        } else if count.is_some() {
                            add(C::IndNonWinCount, 1);
                        }
                    } else if prev_window.is_none() {
                        add(C::IndMissFirst, 1);
                    } else {
                        add(C::IndMissStep, 1);
                    }
                }
                let (base_off, step, records) = match prev_window {
                    Some(w) => {
                        let last = w.base + u64::from(w.records - 1) * u64::from(w.step);
                        let step = if offset > last {
                            offset - last
                        } else if w.records == 1 && offset < w.base {
                            w.base - offset
                        } else if w.records > 1
                            && offset < w.base
                            && (w.base - offset).is_multiple_of(u64::from(w.step))
                        {
                            u64::from(w.step)
                        } else {
                            0
                        };
                        let walks = step >= u64::from(stride)
                            && step <= WINDOW_STEP_MAX
                            && step % 4 == 0
                            && (w.records == 1 || step == u64::from(w.step));
                        let left = args.left(arguments);
                        if walks {
                            let target = (if w.records == 1 { 64 } else { w.records * 2 })
                                .min(WINDOW_MOST as u32);
                            if offset < w.base {
                                // Walking backwards: anchor the window ending at `offset` so
                                // subsequent decreasing offsets hit the cache.
                                let back_avail = offset / step;
                                let from_here_avail = if left >= u64::from(stride) {
                                    (((left - u64::from(stride)) / step) + 1).min(WINDOW_MOST)
                                } else {
                                    1
                                };
                                let count = back_avail
                                    .saturating_add(from_here_avail)
                                    .min(u64::from(target))
                                    .max(1);
                                let back = back_avail.min(count - 1);
                                (offset - back * step, step as u32, count as u32)
                            } else {
                                let fit = if left >= u64::from(stride) {
                                    (((left - u64::from(stride)) / step) + 1).min(WINDOW_MOST)
                                        as u32
                                } else {
                                    1
                                };
                                (offset, step as u32, target.min(fit).max(1))
                            }
                        } else {
                            // Random or non-sequential jump within the same indirect buffer:
                            // clamp a stride-packed block of up to 64 records around `offset`
                            // aligned to `phase = offset % block_step` so scattered/backwards
                            // accesses hit the window cache.
                            const JUMP_BLOCK: u64 = 64;
                            let block_step = if w.records > 1
                                && u64::from(w.step) > u64::from(stride)
                                && u64::from(w.step) <= WINDOW_STEP_MAX
                                && offset.abs_diff(w.base).is_multiple_of(u64::from(w.step))
                            {
                                u64::from(w.step)
                            } else {
                                u64::from(stride)
                            };
                            let before_avail = offset / block_step;
                            let from_here_avail = if left >= u64::from(stride) {
                                (((left - u64::from(stride)) / block_step) + 1).min(WINDOW_MOST)
                            } else {
                                1
                            };
                            let count = before_avail
                                .saturating_add(from_here_avail)
                                .min(JUMP_BLOCK)
                                .max(1);
                            let want_before = (count / 2).min(before_avail);
                            let after_needed = count.saturating_sub(want_before);
                            let before = if after_needed > from_here_avail {
                                (count - from_here_avail).min(before_avail)
                            } else {
                                want_before
                            };
                            (offset - before * block_step, block_step as u32, count as u32)
                        }
                    }
                    None => (offset, stride, max),
                };
                let refresh_len = if records > 1 {
                    u64::from(step) * u64::from(records - 1) + u64::from(stride)
                } else {
                    len
                };
                let base_addr = arguments - (offset - base_off);
                args.refresh(list, base_addr, len.max(refresh_len));
                if let Some((s, at)) = &counter {
                    s.refresh(list, *at, 4);
                }
                let count_arg = counter.as_ref().map(|(s, at)| (&s.buffer, s.offset(*at)));
                let bounded = if windowed && records > 1 {
                    crate::indirect::bound_ahead(
                        list,
                        (&args.buffer, base_off),
                        (kind, stride, step, records),
                    )
                } else {
                    crate::indirect::bound(
                        list,
                        (&args.buffer, base_off),
                        count_arg,
                        (kind, stride, records),
                    )
                };
                let Some((bounded, dst_offset, evicted)) = bounded else {
                    why("not bounded (no clamp, or arguments not 4-byte aligned)");
                    return false;
                };
                if evicted {
                    self.windows.clear();
                }
                self.host_pipeline = 0;
                self.host_roots[1] = 0;
                if windowed {
                    if self.windows.len() >= MAX_WINDOWS {
                        self.windows.remove(0);
                    }
                    self.windows.push(Window {
                        copy: args.id(),
                        kind,
                        stride,
                        step,
                        base: base_off,
                        records,
                        epoch,
                        dst_offset,
                        scratch: bounded.clone(),
                    });
                }
                let idx = (offset - base_off) / u64::from(step);
                (bounded, dst_offset + idx * u64::from(stride))
            }
        };
        let ran = match kind {
            DRAW | INDEXED => self.prepare(list, kind == INDEXED),
            MESH => self.prepare_for(list, false, true),
            DISPATCH => self.bind(list, p, true),
            _ => false,
        };
        if !ran {
            why("prepare failed");
            return false;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| {
            self.op_text(
                format!("indirect kind {kind} max {max} at {arguments:#x}"),
                kind == DISPATCH,
            )
        });
        if kind == MESH {
            self.mesh_scissor(list, true);
        }
        // SAFETY: an open list with everything set; the scratch buffer and
        // host copies are kept alive by the list, and the signature is cached.
        unsafe {
            list.list.ExecuteIndirect(
                &signature,
                max,
                &bounded,
                at,
                counter.as_ref().map(|(s, _)| &s.buffer),
                counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
            )
        };
        if kind == MESH {
            self.mesh_scissor(list, false);
        }
        if kind == DISPATCH {
            DISPATCHED.fetch_add(1, Ordering::Relaxed);
        } else {
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}

fn host_signature(kind: u32, stride: u32) -> Option<ID3D12CommandSignature> {
    static MADE: std::sync::Mutex<Vec<((u32, u32), ID3D12CommandSignature)>> =
        std::sync::Mutex::new(Vec::new());
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = made.iter().find(|(k, _)| *k == (kind, stride)) {
        return Some(s.clone());
    }
    let argument = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: 1,
        pArgumentDescs: &argument,
        NodeMask: 0,
    };
    let mut signature: Option<ID3D12CommandSignature> = None;
    // SAFETY: a description of one argument; no root arguments.
    let result = unsafe {
        crate::host::host()?.device.CreateCommandSignature(
            &desc,
            None::<&ID3D12RootSignature>,
            &mut signature,
        )
    };
    if let Err(e) = result {
        once!(error = %e, kind, stride, "host command signature refused");
        return None;
    }
    let signature = signature?;
    made.push(((kind, stride), signature.clone()));
    Some(signature)
}

fn view_kind(view: &View) -> Option<ResourceType> {
    match view {
        View::Shader(..) | View::Placed(false, ..) => Some(ResourceType::Srv),
        View::Unordered(..) | View::Placed(true, ..) => Some(ResourceType::Uav),
        _ => None,
    }
}

fn depth_state(texture: &ID3D12Resource, desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>) -> u32 {
    const ONE_PLANE: [i32; 2] = [55, 40];
    let Some(desc) = desc else {
        return DEPTH_WRITE;
    };
    let format = if desc.Format.0 == 0 {
        // SAFETY: a live texture; a view without a format has its own.
        unsafe { texture.GetDesc() }.Format.0
    } else {
        desc.Format.0
    };
    let mut needed = D3D12_DSV_FLAG_READ_ONLY_DEPTH.0;
    if !ONE_PLANE.contains(&format) {
        needed |= D3D12_DSV_FLAG_READ_ONLY_STENCIL.0;
    }
    if desc.Flags.0 & needed == needed {
        DEPTH_READ
    } else {
        DEPTH_WRITE
    }
}

fn readable(list: &HostList, kind: ResourceType, view: &View) {
    const UNORDERED_ACCESS: u32 = 0x8;
    let placed = crate::descriptor::host::placed_texture(view);
    let view = placed.as_ref().unwrap_or(view);
    if let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(view) {
        shadow.refresh(list, address, len);
        if kind == ResourceType::Uav {
            if STORES.get() || !write_skip() {
                shadow.gpu_writes(list, address, len);
            } else {
                #[cfg(feature = "diagnostics")]
                {
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOut, 1);
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOutBytes, len);
                }
            }
        }
        return;
    }
    match (view, kind) {
        (View::Shader(resource, _), ResourceType::Srv) => {
            let aliased = crate::resource::alias_of(*resource as This)
                .is_some_and(|source| crate::texture::alias(list, *resource as This, source));
            if !aliased
                && let Some(t) = crate::resource::host_resource(*resource as This)
                && crate::texture::title_written(t)
            {
                crate::texture::upload(list, *resource as This, t);
            }
            list.twin_use(*resource as This, false, false);
            let (tested, _) = TESTED_DEPTH.get();
            let state = if tested != 0 && tested == *resource {
                TESTED_DEPTH.set((tested, true));
                DEPTH_READ | SHADER_RESOURCE
            } else {
                SHADER_RESOURCE
            };
            crate::list::ensure_state(list, *resource as This, state);
            #[cfg(feature = "diagnostics")]
            if tracing::enabled!(tracing::Level::DEBUG)
                && let Some(t) = crate::resource::host_resource(*resource as This)
            {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                if d.Flags.0 & 7 == 0 {
                    tracing::debug!(
                        resource = format_args!("{resource:#x}"),
                        ?d,
                        "a shader reads a texture the title fills"
                    );
                }
            }
        }
        (View::Unordered(resource, _, _), ResourceType::Uav) => {
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(*resource as This);
            if !list.twin_use(*resource as This, true, true) {
                crate::list::ensure_state(list, *resource as This, UNORDERED_ACCESS);
            }
        }
        _ => {}
    }
}

#[cfg(feature = "diagnostics")]
fn list_slots() -> &'static [String] {
    static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        std::env::var("OATHKEEP_LIST_SLOTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

#[cfg(feature = "diagnostics")]
fn print_4k(at: u64) -> String {
    let mut bytes = [0u8; 4096];
    if at == 0 || !crate::host::read_into(at as *const u8, &mut bytes) {
        return "unreadable".into();
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(feature = "diagnostics")]
fn memory_state(at: u64, bytes: u64) -> String {
    let mapped = crate::texture::mapped_bytes(at as *const u8, bytes as usize);
    let last = at + bytes.saturating_sub(4096);
    format!(
        "memory={at:#x} mapped={mapped}/{bytes} first4k={} last4k={}",
        print_4k(at),
        print_4k(last)
    )
}

#[cfg(feature = "diagnostics")]
fn texture_state(this: This) -> String {
    let Some((at, desc)) = crate::resource::texture_memory(this) else {
        return "not a texture".into();
    };
    // SAFETY: 56 bytes in the description's layout.
    let words = unsafe {
        std::ptr::from_ref(&desc)
            .cast::<[u32; 14]>()
            .read_unaligned()
    };
    let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
    let staged = match crate::texture::staged_state(this) {
        Some((uploaded, dirty)) => format!("staged=yes uploaded={uploaded} dirty={dirty}"),
        None => "staged=no".into(),
    };
    format!(
        "{}x{} f{} levels={} host_copy={} {staged} host_written={} title_fills={} {}",
        u64::from(words[4]) | u64::from(words[5]) << 32,
        words[6],
        words[8],
        words[7] >> 16,
        crate::resource::host_made(this).is_some(),
        crate::texture::host_wrote(this as usize),
        crate::resource::title_fills(this),
        memory_state(at as u64, bytes)
    )
}

#[cfg(feature = "diagnostics")]
fn view_state(view: Option<&View>) -> String {
    match view {
        None => "no view".into(),
        Some(View::Placed(false, r, d)) if r[0] != 1 => {
            let address = u64::from(d[10]) | u64::from(d[11]) << 32;
            let state = match crate::resource::texture_at_made(address, r) {
                Some(this) => texture_state(this),
                None => format!(
                    "{}x{} f{} levels={} resource=none {}",
                    u64::from(r[4]) | u64::from(r[5]) << 32,
                    r[6],
                    r[8],
                    r[7] >> 16,
                    memory_state(address, crate::resource::placed_bytes(r).unwrap_or(0))
                ),
            };
            format!(
                "placed texture view_format={} {state} view_levels={}+{}",
                d[0], d[4], d[5] as i32
            )
        }
        Some(View::Shader(r, d)) if crate::resource::texture_memory(*r as This).is_some() => {
            format!(
                "texture object {r:#x} view_format={} {} view_levels={}",
                d.as_ref().map_or(0, |d| d[0]),
                texture_state(*r as This),
                d.as_ref()
                    .map_or("all".to_owned(), |d| format!("{}+{}", d[4], d[5] as i32))
            )
        }
        Some(v) => format!("{v:?}").chars().take(80).collect(),
    }
}

#[cfg(feature = "diagnostics")]
fn describe(view: Option<&View>) -> String {
    let texture = |r: This| {
        crate::resource::host_made(r).map(|t| {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            format!(
                "{}x{} f{} flags {:#x} of {r:?}",
                d.Width, d.Height, d.Format.0, d.Flags.0
            )
        })
    };
    match view {
        None => "no view".into(),
        Some(v @ View::Shader(r, _)) => texture(*r as This)
            .unwrap_or_else(|| format!("buffer {:?}", crate::descriptor::host::buffer_range(v))),
        Some(v @ View::Unordered(r, ..)) => match texture(*r as This) {
            Some(t) => format!("uav texture {t}"),
            None => format!(
                "uav resource {r:#x} buffer {:?}",
                crate::descriptor::host::buffer_range(v)
            ),
        },
        Some(View::Placed(u, r, d)) => format!(
            "placed uav={u} dim {} {}x{} f{} layout {:#x} at {:#x}",
            r[0],
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[8],
            r[11],
            u64::from(d[10]) | u64::from(d[11]) << 32
        ),
        Some(View::Constant(address, size))
            if *size >= 16 && crate::texture::committed(*address as *const u8, 16) =>
        {
            // SAFETY: 16 committed bytes of title memory.
            let v = unsafe { (*address as *const [f32; 4]).read_unaligned() };
            format!("Constant({address:#x}, {size}) {v:?}")
        }
        Some(v) => format!("{v:?}").chars().take(60).collect(),
    }
}

const TRIANGLE_FAN: u32 = 6;
const RECTANGLES: u32 = 17;
const LINE_LOOP: u32 = 18;
const QUADS: u32 = 19;
const QUAD_STRIP: u32 = 20;

pub(crate) fn viewports(list: &HostList, viewports: &[D3D12_VIEWPORT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetViewports(viewports) };
}

pub(crate) fn scissors(list: &HostList, rects: &[RECT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetScissorRects(rects) };
}

pub(crate) fn blend_factor(list: &HostList, factor: Option<&[f32; 4]>) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetBlendFactor(factor) };
}

pub(crate) fn stencil_ref(list: &HostList, reference: u32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("STENCIL REF {reference:#x}")
    });
    // SAFETY: an open list.
    unsafe { list.list.OMSetStencilRef(reference) };
}

pub(crate) fn depth_bounds(list: &HostList, min: f32, max: f32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("DEPTH BOUNDS {min}..{max}")
    });
    if crate::host::host().is_some_and(|h| h.depth_bounds())
        && let Ok(l1) = list.list.cast::<ID3D12GraphicsCommandList1>()
    {
        // SAFETY: an open list on a device that supports depth bounds.
        unsafe { l1.OMSetDepthBounds(min, max) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_span_cache_reuses_clean_spans_and_rebuilds_only_dirty_shadows() {
        let mut cache: BufferSpanCache<usize> = BufferSpanCache::default();
        let key_of = |&id: &usize| id;

        // Initial empty cache does not rebuild.
        let (shadows, bytes, rebuilt) = cache.ensure_spans();
        assert!(shadows.is_empty());
        assert_eq!(bytes, 0);
        assert_eq!(rebuilt, 0);
        assert_eq!(cache.rebuilds, 0);

        // Adding slots marks only the affected shadows dirty and merges nearby ranges per shadow.
        cache.set(1, Some((100, 0x1000, 0x2000)), key_of);
        cache.set(2, Some((100, 0x2000, 0x3000)), key_of);
        cache.set(3, Some((200, 0x100_0000, 0x100_4000)), key_of);
        cache.set(4, Some((300, 0x200_0000, 0x200_8000)), key_of);
        let (shadows, bytes, rebuilt) = cache.ensure_spans();
        assert_eq!(rebuilt, 3);
        assert_eq!(shadows.len(), 3);
        assert_eq!(bytes, 0x2000 + 0x4000 + 0x8000);
        assert_eq!(cache.rebuilds, 1);

        // Clean frame reuses cached spans with zero rebuilds.
        let (_, bytes2, rebuilt2) = cache.ensure_spans();
        assert_eq!(rebuilt2, 0);
        assert_eq!(bytes2, 0xe000);
        assert_eq!(cache.rebuilds, 1);

        // Re-setting a slot to the identical (shadow, from, to) stays clean.
        cache.set(1, Some((100, 0x1000, 0x2000)), key_of);
        let (_, _, rebuilt3) = cache.ensure_spans();
        assert_eq!(rebuilt3, 0);
        assert_eq!(cache.rebuilds, 1);

        // Updating one slot only rebuilds that single shadow, leaving the other two untouched.
        cache.set(2, Some((100, 0x2000, 0x5000)), key_of);
        let (_, bytes4, rebuilt4) = cache.ensure_spans();
        assert_eq!(rebuilt4, 1);
        assert_eq!(bytes4, 0x4000 + 0x4000 + 0x8000);
        assert_eq!(cache.rebuilds, 2);

        // Moving a slot from shadow 200 to shadow 300 dirties only those two shadows and removes empty shadow 200.
        cache.set(3, Some((300, 0x200_8000, 0x200_c000)), key_of);
        let (shadows5, bytes5, rebuilt5) = cache.ensure_spans();
        assert_eq!(rebuilt5, 2);
        assert_eq!(shadows5.len(), 2);
        assert!(!shadows5.contains_key(&200));
        assert_eq!(bytes5, 0x4000 + 0xc000);
        assert_eq!(cache.rebuilds, 3);

        // Removing a slot invalidates only its owner shadow and shrinks bytes incrementally.
        cache.set(3, None, key_of);
        let (shadows6, bytes6, rebuilt6) = cache.ensure_spans();
        assert_eq!(rebuilt6, 1);
        assert_eq!(shadows6.len(), 2);
        assert_eq!(bytes6, 0x4000 + 0x8000);
        assert_eq!(cache.rebuilds, 4);

        // Removing a non-existent slot does not dirty anything.
        cache.set(999, None, key_of);
        let (_, _, rebuilt7) = cache.ensure_spans();
        assert_eq!(rebuilt7, 0);
        assert_eq!(cache.rebuilds, 4);

        // Adding and removing a slot on a new shadow before ensure_spans cleans up cleanly.
        cache.set(50, Some((400, 0x1000, 0x2000)), key_of);
        cache.set(50, None, key_of);
        let (shadows8, bytes8, rebuilt8) = cache.ensure_spans();
        assert_eq!(rebuilt8, 1);
        assert!(!shadows8.contains_key(&400));
        assert_eq!(bytes8, 0xc000);

        // collect_stale_slots filters by shadow and range accurately.
        let stale = cache.collect_stale_slots(|&id| id == 100, |from, to| from < 0x3000 && to > 0x1500);
        assert!(stale.contains(&1));
        assert!(stale.contains(&2));
        assert_eq!(stale.len(), 2);

        // Clearing (rebuild/eviction) resets spans and bytes cleanly.
        cache.clear();
        let (shadows9, bytes9, rebuilt9) = cache.ensure_spans();
        assert_eq!(rebuilt9, 0);
        assert!(shadows9.is_empty());
        assert_eq!(bytes9, 0);
        assert_eq!(cache.slot_count(), 0);
    }

    #[test]
    fn frame_texture_budget_caps_combined_passes_and_resets_each_frame() {
        let budget = FrameTextureBudget::new();
        // Frame 10 starts with full 4000 us budget.
        assert_eq!(budget.remaining_us(10, 4000), 4000);
        // First pass (e.g. early_textures) spends 2500 us.
        budget.record(10, 2500);
        assert_eq!(budget.remaining_us(10, 4000), 1500);
        // Second pass (e.g. heap_buffers -> heap_textures) spends 2000 us, exhausting the 4000 us cap.
        budget.record(10, 2000);
        assert_eq!(budget.remaining_us(10, 4000), 0);
        // Larger budget (e.g. catchup 12000 us) still sees remaining room on the same frame.
        assert_eq!(budget.remaining_us(10, 12000), 7500);
        // Advancing to frame 11 resets the accumulator.
        assert_eq!(budget.remaining_us(11, 4000), 4000);
        budget.record(11, 4100);
        assert_eq!(budget.remaining_us(11, 4000), 0);
    }

    #[test]
    fn table_hash_distinguishes_slot_changes() {
        let hash = |slots: &[[u32; 3]]| {
            let mut h1 = 0xcbf2_9ce4_8422_2325_u64 ^ slots.len() as u64;
            let mut h2 = 0x517c_c1b7_2722_0a95_u64 ^ 0x1000;
            for w in slots {
                hash_table_words(w, &mut h1, &mut h2);
            }
            finalize_table_hash(h1, h2)
        };
        let a = hash(&[[1, 2, 3], [4, 5, 6]]);
        let b = hash(&[[1, 2, 3], [4, 5, 7]]);
        let c = hash(&[[4, 5, 6], [1, 2, 3]]);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, hash(&[[1, 2, 3], [4, 5, 6]]));
    }
}
