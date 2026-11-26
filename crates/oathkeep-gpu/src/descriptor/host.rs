use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;

const MADE_LOCK_STRIPES: usize = 64;

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    count: u32,
    dynamic: u32,
    sampler: bool,
    made_keys: Vec<std::sync::atomic::AtomicU64>,
    made_locks: Box<[Mutex<std::collections::HashMap<u32, u64>>; MADE_LOCK_STRIPES]>,
    kept: Box<[Mutex<Vec<Option<ID3D12Resource>>>; MADE_LOCK_STRIPES]>,
    has_kept: Vec<std::sync::atomic::AtomicU64>,
    is_buffer: Vec<std::sync::atomic::AtomicU64>,
    in_texture_queue: Vec<std::sync::atomic::AtomicU64>,
    in_placed: Vec<std::sync::atomic::AtomicU64>,
    next: Mutex<DynamicSlots>,
    free: std::sync::atomic::AtomicU32,
    too_long: std::sync::atomic::AtomicU32,
    strides: Option<(ID3D12Resource, u64, usize)>,
    buffers: Mutex<std::collections::BTreeSet<u32>>,
    changed: Mutex<std::collections::HashSet<u32>>,
    textures: Mutex<TextureQueue>,
    tabled: Vec<std::sync::atomic::AtomicU32>,
    placed: std::sync::Arc<Mutex<Spans>>,
}

#[inline]
fn bit_mask(lo: u32, hi: u32) -> u64 {
    let width = hi - lo;
    let ones = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    ones << lo
}

struct DynamicSlots {
    cursor: u32,
    count: u32,
    words: Vec<u64>,
}

impl DynamicSlots {
    fn new(count: u32) -> Self {
        let word_count = (count as usize).div_ceil(64);
        let mut words = vec![0u64; word_count];
        let rem = count & 63;
        if rem != 0
            && let Some(last) = words.last_mut()
        {
            *last = !((1u64 << rem) - 1);
        }
        Self {
            cursor: 0,
            count,
            words,
        }
    }

    fn find_run(&self, start: u32, end: u32, n: u32) -> Option<u32> {
        if start >= end || end - start < n {
            return None;
        }
        if n == 1 {
            let mut word_idx = (start as usize) >> 6;
            let bit = start & 63;
            let mut w = self.words[word_idx] | ((1u64 << bit) - 1);
            loop {
                if w != u64::MAX {
                    let pos = (word_idx as u32) * 64 + (!w).trailing_zeros();
                    return (pos < end).then_some(pos);
                }
                word_idx += 1;
                if word_idx * 64 >= end as usize || word_idx >= self.words.len() {
                    return None;
                }
                w = self.words[word_idx];
            }
        }
        let mut run_start = start;
        let mut run = 0u32;
        let mut pos = start;
        while pos < end {
            let word_idx = (pos as usize) >> 6;
            let bit = pos & 63;
            let word = self.words[word_idx];
            if bit == 0 {
                if word == 0 {
                    let avail = (end - pos).min(64);
                    if run == 0 {
                        run_start = pos;
                    }
                    run += avail;
                    if run >= n {
                        return Some(run_start);
                    }
                    pos += avail;
                    continue;
                }
                if word == u64::MAX {
                    run = 0;
                    pos = (pos + 64).min(end);
                    continue;
                }
            }
            let shifted = word >> bit;
            let bits_left = (64 - bit).min(end - pos);
            if shifted & 1 == 0 {
                let zeros = shifted.trailing_zeros().min(bits_left);
                if run == 0 {
                    run_start = pos;
                }
                run += zeros;
                if run >= n {
                    return Some(run_start);
                }
                pos += zeros;
            } else {
                let ones = (!shifted).trailing_zeros().min(bits_left);
                run = 0;
                pos += ones;
            }
        }
        None
    }

    fn alloc(&mut self, n: u32) -> Option<u32> {
        if n == 0 || n > self.count {
            return None;
        }
        let first = self.find_run(self.cursor, self.count, n).or_else(|| {
            if self.cursor == 0 {
                None
            } else {
                let wrap_end = self.cursor.saturating_add(n - 1).min(self.count);
                self.find_run(0, wrap_end, n)
            }
        })?;
        let mut pos = first;
        let end = first + n;
        while pos < end {
            let word_idx = (pos as usize) >> 6;
            let lo = pos & 63;
            let hi = (end - pos).min(64 - lo) + lo;
            self.words[word_idx] |= bit_mask(lo, hi);
            pos += hi - lo;
        }
        self.cursor = (first + n) % self.count;
        Some(first)
    }

    fn free(&mut self, first: u32, n: u32) -> u32 {
        let end = first.saturating_add(n).min(self.count);
        if first >= end {
            return 0;
        }
        let mut given = 0u32;
        let mut pos = first;
        while pos < end {
            let word_idx = (pos as usize) >> 6;
            let lo = pos & 63;
            let hi = (end - pos).min(64 - lo) + lo;
            let mask = bit_mask(lo, hi);
            let w = &mut self.words[word_idx];
            given += (*w & mask).count_ones();
            *w &= !mask;
            pos += hi - lo;
        }
        given
    }
}

fn bit_words(count: u32) -> Vec<std::sync::atomic::AtomicU64> {
    (0..(count as usize).div_ceil(64))
        .map(|_| std::sync::atomic::AtomicU64::new(0))
        .collect()
}

fn bit_get(bits: &[std::sync::atomic::AtomicU64], index: u32) -> bool {
    let word = (index as usize) >> 6;
    let mask = 1u64 << (index & 63);
    bits.get(word)
        .is_some_and(|w| w.load(std::sync::atomic::Ordering::Acquire) & mask != 0)
}

fn bit_set(bits: &[std::sync::atomic::AtomicU64], index: u32, on: bool) {
    let word = (index as usize) >> 6;
    let mask = 1u64 << (index & 63);
    if let Some(w) = bits.get(word) {
        if on {
            w.fetch_or(mask, std::sync::atomic::Ordering::Release);
        } else {
            w.fetch_and(!mask, std::sync::atomic::Ordering::Release);
        }
    }
}

fn bit_any_in_range(bits: &[std::sync::atomic::AtomicU64], start: u32, end: u32) -> bool {
    if start >= end {
        return false;
    }
    let mut pos = start;
    while pos < end {
        let word_idx = (pos as usize) >> 6;
        let lo = pos & 63;
        let hi = (end - pos).min(64 - lo) + lo;
        let mask = bit_mask(lo, hi);
        if bits
            .get(word_idx)
            .is_some_and(|w| w.load(std::sync::atomic::Ordering::Acquire) & mask != 0)
        {
            return true;
        }
        pos += hi - lo;
    }
    false
}

fn coalesce_u64_ranges(ranges: &mut Vec<(u64, u64)>) {
    if ranges.len() <= 1 {
        ranges.retain(|&(start, end)| start < end);
        return;
    }
    ranges.sort_unstable();
    let mut write = 0usize;
    for i in 0..ranges.len() {
        let (start, end) = ranges[i];
        if start >= end {
            continue;
        }
        if write > 0 && start <= ranges[write - 1].1 {
            ranges[write - 1].1 = ranges[write - 1].1.max(end);
        } else {
            ranges[write] = (start, end);
            write += 1;
        }
    }
    ranges.truncate(write);
}

static PLACED_SPANS: Mutex<Vec<std::sync::Weak<Mutex<Spans>>>> = Mutex::new(Vec::new());

pub(crate) fn next_start(start: u64, end: u64) -> Option<u64> {
    let heaps = PLACED_SPANS.lock().unwrap_or_else(|e| e.into_inner());
    heaps
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter_map(|spans| {
            spans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .next_start(start, end)
        })
        .min()
}

pub(crate) fn first_viewed(start: u64) -> Option<u32> {
    let heaps = PLACED_SPANS.lock().unwrap_or_else(|e| e.into_inner());
    heaps
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter_map(|spans| {
            spans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_viewed(start)
        })
        .min()
}

static RESHOWN: Mutex<Vec<u64>> = Mutex::new(Vec::new());

const STAGE_TEX_BUCKETS: usize = 64;
static STAGE_TEX_EPOCHS: [std::sync::atomic::AtomicU64; STAGE_TEX_BUCKETS] =
    [const { std::sync::atomic::AtomicU64::new(1) }; STAGE_TEX_BUCKETS];

#[inline]
fn tex_epoch_bucket(addr: u64) -> usize {
    (((addr >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15)) >> 58) as usize & (STAGE_TEX_BUCKETS - 1)
}

pub(crate) fn bump_stage_texture_epoch_at(addr: u64) {
    let b = tex_epoch_bucket(addr);
    STAGE_TEX_EPOCHS[b].fetch_add(1, std::sync::atomic::Ordering::Release);
}

pub(crate) fn bump_stage_texture_epoch() {
    for ep in &STAGE_TEX_EPOCHS {
        ep.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) enum StageAction {
    None,
    Buffer {
        shadow: std::sync::Arc<crate::shadow::Shadow>,
        address: u64,
        len: u64,
    },
    Texture {
        resource: usize,
    },
}

pub(crate) struct DynamicMade {
    pub(crate) found: bool,
    pub(crate) kept: Option<ID3D12Resource>,
    pub(crate) action: StageAction,
    pub(crate) view_kind: Option<oathkeep_shader::ResourceType>,
    pub(crate) is_texture: bool,
}

impl DynamicMade {
    fn missing() -> Self {
        Self {
            found: false,
            kept: None,
            action: StageAction::None,
            view_kind: None,
            is_texture: false,
        }
    }
}

pub(crate) enum BatchSlot {
    Unprobed,
    Hit(DynamicMade),
    MissNew,
    MissEpoch(u8),
}

const STAGE_STRIPES: usize = 256;
const SLOTS_PER_STRIPE: usize = 1024;
const RECENT_MISS_SLOTS: usize = 32;

struct StageEntry {
    key: u64,
    slot: u32,
    epoch: u64,
    is_buffer: bool,
    is_texture: bool,
    tex_bucket: u8,
    view_kind: Option<oathkeep_shader::ResourceType>,
    kept_raw: usize,
    kept: Option<ID3D12Resource>,
    action: StageAction,
}

impl StageEntry {
    #[inline]
    fn is_valid(&self) -> bool {
        if self.is_buffer {
            if let StageAction::Buffer {
                shadow,
                address,
                len,
            } = &self.action
            {
                !shadow.is_dead() && crate::shadow::epoch_for_range(*address, *len) == self.epoch
            } else {
                false
            }
        } else {
            STAGE_TEX_EPOCHS[self.tex_bucket as usize]
                .load(std::sync::atomic::Ordering::Acquire)
                == self.epoch
        }
    }
}

#[derive(Default)]
struct StageKeyHasher(u64);

impl std::hash::Hasher for StageKeyHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (i ^ (i >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    }
}

type StageKeyMap =
    std::collections::HashMap<u64, usize, std::hash::BuildHasherDefault<StageKeyHasher>>;

struct StageStripe {
    map: StageKeyMap,
    entries: Vec<Option<StageEntry>>,
    cursor: usize,
}

impl StageStripe {
    fn new() -> Self {
        let mut entries = Vec::with_capacity(SLOTS_PER_STRIPE);
        entries.resize_with(SLOTS_PER_STRIPE, || None);
        Self {
            map: StageKeyMap::default(),
            entries,
            cursor: 0,
        }
    }
}

struct StageHeap {
    _heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    stripes: Box<[std::sync::RwLock<StageStripe>; STAGE_STRIPES]>,
    recent_miss: Box<[[std::sync::atomic::AtomicU64; RECENT_MISS_SLOTS]; STAGE_STRIPES]>,
}

// SAFETY: CPU-only descriptor heap; per-stripe RwLock guards concurrent read/write
// of every slot, and CopyDescriptorsSimple on disjoint slots is free-threaded in D3D12.
unsafe impl Send for StageHeap {}
unsafe impl Sync for StageHeap {}

impl StageHeap {
    fn cpu(&self, slot: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + slot as usize * self.step as usize,
        }
    }
}

fn cpu_stage_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CPU_STAGE_CACHE").is_none_or(|v| v != "0"))
}

fn cpu_stage_heap() -> Option<&'static StageHeap> {
    if !cpu_stage_enabled() {
        return None;
    }
    static STAGE: std::sync::OnceLock<Option<StageHeap>> = std::sync::OnceLock::new();
    STAGE
        .get_or_init(|| {
            let host = host()?;
            let total = (STAGE_STRIPES * SLOTS_PER_STRIPE) as u32;
            // SAFETY: plain CPU-only descriptor heap creation.
            let heap: ID3D12DescriptorHeap = unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: total,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                        NodeMask: 0,
                    })
                    .ok()?
            };
            // SAFETY: live heap.
            let (cpu, step) = unsafe {
                (
                    heap.GetCPUDescriptorHandleForHeapStart().ptr,
                    host.device
                        .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
                )
            };
            let stripes: Vec<std::sync::RwLock<StageStripe>> = (0..STAGE_STRIPES)
                .map(|_| std::sync::RwLock::new(StageStripe::new()))
                .collect();
            let stripes = stripes.into_boxed_slice().try_into().ok()?;
            let recent_miss: Vec<[std::sync::atomic::AtomicU64; RECENT_MISS_SLOTS]> =
                (0..STAGE_STRIPES)
                    .map(|_| std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)))
                    .collect();
            let recent_miss = recent_miss.into_boxed_slice().try_into().ok()?;
            Some(StageHeap {
                _heap: heap,
                cpu,
                step,
                stripes,
                recent_miss,
            })
        })
        .as_ref()
}

fn stage_view_kind(view: &View) -> Option<oathkeep_shader::ResourceType> {
    match view {
        View::Shader(..) | View::Placed(false, ..) => Some(oathkeep_shader::ResourceType::Srv),
        View::Unordered(..) | View::Placed(true, ..) => Some(oathkeep_shader::ResourceType::Uav),
        _ => None,
    }
}

pub(crate) fn levels_changed(start: u64) {
    bump_stage_texture_epoch_at(start);
    RESHOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(start);
}

static WAITING: Mutex<std::collections::BTreeMap<u64, u64>> =
    Mutex::new(std::collections::BTreeMap::new());
static WAITING_OLDEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn levels_waiting(start: u64) {
    let now = crate::host::moment().max(1);
    let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
    let was_empty = waiting.is_empty();
    waiting.insert(start, now);
    if was_empty {
        WAITING_OLDEST.store(now, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Default)]
pub(crate) struct Spans {
    rule: Rule,
    by_start: std::collections::BTreeMap<u64, (u64, std::collections::BTreeSet<u32>)>,
    by_slot: std::collections::HashMap<u32, u64>,
    first: std::collections::HashMap<u32, u32>,
    sizes: std::collections::HashMap<[u32; 14], Option<u64>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Rule {
    #[default]
    Start,
    Overlap,
    Never,
}

const HEAD: u64 = 4096;

impl Spans {
    fn bytes(&mut self, desc: &[u32; 14]) -> Option<u64> {
        if let Some(&known) = self.sizes.get(desc) {
            return known;
        }
        let bytes = crate::resource::placed_bytes(desc);
        self.sizes.insert(*desc, bytes);
        bytes
    }

    fn overlapping(&self, start: u64, end: u64) -> Vec<u64> {
        let before = self
            .by_start
            .range(..start)
            .next_back()
            .filter(|(_, span)| span.0 > start)
            .map(|(at, _)| *at);
        before
            .into_iter()
            .chain(self.by_start.range(start..end).map(|(at, _)| *at))
            .collect()
    }

    fn note(&mut self, index: u32, start: u64, end: u64, newer: bool) -> Vec<u32> {
        let same = |span: Option<&(u64, std::collections::BTreeSet<u32>)>| {
            span.is_some_and(|s| s.0 == end)
        };
        if self.by_slot.get(&index) == Some(&start) && same(self.by_start.get(&start)) {
            return Vec::new();
        }
        self.forget(index);
        if end <= start {
            return Vec::new();
        }
        let rule = self.rule;
        let has_clash = match rule {
            Rule::Start => self
                .by_start
                .range(start.saturating_sub(HEAD - 1)..start.saturating_add(HEAD))
                .any(|(&at, span)| !(at == start && span.0 == end)),
            Rule::Overlap | Rule::Never => {
                let before_clash = self
                    .by_start
                    .range(..start)
                    .next_back()
                    .is_some_and(|(_, span)| span.0 > start);
                before_clash
                    || self
                        .by_start
                        .range(start..end)
                        .any(|(&at, span)| !(at == start && span.0 == end))
            }
        };
        let mut stale = Vec::new();
        if has_clash {
            if !newer || rule == Rule::Never {
                return stale;
            }
            let clashing: Vec<u64> = match rule {
                Rule::Start => self
                    .by_start
                    .range(start.saturating_sub(HEAD - 1)..start.saturating_add(HEAD))
                    .filter(|&(&at, span)| !(at == start && span.0 == end))
                    .map(|(&at, _)| at)
                    .collect(),
                Rule::Overlap | Rule::Never => self
                    .overlapping(start, end)
                    .into_iter()
                    .filter(|&at| !(at == start && same(self.by_start.get(&at))))
                    .collect(),
            };
            for at in clashing {
                if let Some((old_end, slots)) = self.by_start.remove(&at) {
                    static SAID: std::sync::atomic::AtomicU32 =
                        std::sync::atomic::AtomicU32::new(0);
                    if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 48 {
                        tracing::info!(
                            slot = index,
                            start = format_args!("{start:#x}"),
                            end = format_args!("{end:#x}"),
                            stale_start = format_args!("{at:#x}"),
                            stale_end = format_args!("{old_end:#x}"),
                            stale_slots = slots.len(),
                            rule = ?rule,
                            "a placed texture view makes older views of other memory stale"
                        );
                    }
                    for slot in slots {
                        self.by_slot.remove(&slot);
                        stale.push(slot);
                    }
                }
            }
        }
        self.by_start
            .entry(start)
            .or_insert_with(|| (end, Default::default()))
            .1
            .insert(index);
        self.by_slot.insert(index, start);
        stale
    }

    fn shows(&mut self, index: u32, first: u32) {
        self.first.insert(index, first);
    }

    fn first_viewed(&self, start: u64) -> Option<u32> {
        self.by_start
            .get(&start)?
            .1
            .iter()
            .filter_map(|slot| self.first.get(slot))
            .min()
            .copied()
    }

    fn next_start(&self, start: u64, end: u64) -> Option<u64> {
        self.by_start
            .range(start.saturating_add(HEAD)..end)
            .next()
            .map(|(at, _)| *at)
    }

    fn slots_at(&self, start: u64) -> Vec<u32> {
        self.by_start
            .get(&start)
            .map(|span| span.1.iter().copied().collect())
            .unwrap_or_default()
    }

    fn forget(&mut self, index: u32) {
        self.first.remove(&index);
        let Some(start) = self.by_slot.remove(&index) else {
            return;
        };
        if let Some(span) = self.by_start.get_mut(&start) {
            span.1.remove(&index);
            if span.1.is_empty() {
                self.by_start.remove(&start);
            }
        }
    }

    fn extend_slots_in(&self, start: u64, end: u64, out: &mut Vec<u32>) {
        if end <= start {
            return;
        }
        if let Some((_, span)) = self
            .by_start
            .range(..start)
            .next_back()
            .filter(|(_, span)| span.0 > start)
        {
            out.extend(span.1.iter().copied());
        }
        for (_, span) in self.by_start.range(start..end) {
            out.extend(span.1.iter().copied());
        }
    }

    #[cfg(test)]
    fn slots_in(&self, start: u64, end: u64) -> Vec<u32> {
        let mut out = Vec::new();
        self.extend_slots_in(start, end, &mut out);
        out
    }

    fn extend_slots_holding(&self, start: u64, end: u64, out: &mut Vec<u32>) {
        if end <= start {
            return;
        }
        if !self.by_start.contains_key(&start)
            && let Some((_, span)) = self
                .by_start
                .range(..start)
                .next_back()
                .filter(|(_, span)| span.0 > start)
        {
            out.extend(span.1.iter().copied());
        }
        for (_, span) in self.by_start.range(start..end) {
            out.extend(span.1.iter().copied());
        }
    }

    #[cfg(test)]
    fn slots_holding(&self, start: u64, end: u64) -> Vec<u32> {
        let mut out = Vec::new();
        self.extend_slots_holding(start, end, &mut out);
        out
    }
}

static PAGE_WRITTEN: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

pub(crate) static PAGE_WRITES: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

const PAGE_WRITE_AFTER: u64 = 2;

pub(crate) fn page_written(at: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    const MOST: usize = 1 << 15;
    let (start, end) = (at as u64, at.saturating_add(len) as u64);
    if end <= SPANNED[0].load(Relaxed) || start >= SPANNED[1].load(Relaxed) {
        return;
    }
    let mut ranges = PAGE_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    PAGE_WRITES[0].fetch_add(1, Relaxed);
    if let Some(last) = ranges.last_mut()
        && last.1 == start
    {
        last.1 = end;
        return;
    }
    if ranges.len() >= MOST {
        PAGE_WRITES[1].fetch_add(ranges.len() as u64, Relaxed);
        ranges.clear();
    }
    ranges.push((start, end));
}

static REMAPPED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

static SPANNED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(u64::MAX),
    std::sync::atomic::AtomicU64::new(0),
];

pub(crate) static STALE: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

fn stale_rule() -> Rule {
    static RULE: std::sync::OnceLock<Rule> = std::sync::OnceLock::new();
    *RULE.get_or_init(
        || match std::env::var("OATHKEEP_STALE_TEXTURES").as_deref() {
            Ok("0") => Rule::Never,
            Ok("overlap") => Rule::Overlap,
            _ => Rule::Start,
        },
    )
}

pub(crate) fn remapped(at: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    const MOST: usize = 1 << 15;
    let (start, end) = (at as u64, at.saturating_add(len) as u64);
    if end <= SPANNED[0].load(Relaxed) || start >= SPANNED[1].load(Relaxed) {
        return;
    }
    let mut ranges = REMAPPED.lock().unwrap_or_else(|e| e.into_inner());
    STALE[1].fetch_add(1, Relaxed);
    if let Some(last) = ranges.last_mut()
        && last.1 == start
    {
        last.1 = end;
        return;
    }
    if ranges.len() >= MOST {
        STALE[2].fetch_add(ranges.len() as u64, Relaxed);
        ranges.clear();
    }
    ranges.push((start, end));
}

pub(crate) fn remapped_batch(spans: &[(usize, usize)]) {
    use std::sync::atomic::Ordering::Relaxed;
    const MOST: usize = 1 << 15;
    if spans.is_empty() {
        return;
    }
    let lo = SPANNED[0].load(Relaxed);
    let hi = SPANNED[1].load(Relaxed);
    let mut ranges = None;
    for &(at, end) in spans {
        let (start, end) = (at as u64, end as u64);
        if end <= lo || start >= hi {
            continue;
        }
        let guard = ranges.get_or_insert_with(|| REMAPPED.lock().unwrap_or_else(|e| e.into_inner()));
        STALE[1].fetch_add(1, Relaxed);
        if let Some(last) = guard.last_mut()
            && last.1 == start
        {
            last.1 = end;
            continue;
        }
        if guard.len() >= MOST {
            STALE[2].fetch_add(guard.len() as u64, Relaxed);
            guard.clear();
        }
        guard.push((start, end));
    }
}

const TABLED_FRAMES: u64 = 240;

const TABLED_GRACE: u64 = 8;

const TABLED_READ: u32 = 1 << 31;

pub(crate) static TABLED_WRITES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

fn tabled_out() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TABLED_TEXTURES").is_none_or(|v| v != "1"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DueSlot {
    pub(crate) index: u32,
    pub(crate) write: u64,
    pub(crate) mapped: u64,
    pub(crate) checked: Option<(u64, u64, u64, u8, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextureSlotUpdate {
    Done {
        index: u32,
        write: u64,
        again: Option<(u64, u64)>,
    },
    Recheck {
        index: u32,
        write: u64,
        due: u64,
        at: (u64, u64),
        print: u64,
        looks: u8,
        uploaded: u64,
    },
    Later {
        index: u32,
        write: u64,
        frame: u64,
        most: u64,
    },
    Held {
        index: u32,
        write: u64,
        due: u64,
    },
}

pub(crate) struct MirroredBuffer {
    pub(crate) copy: Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)>,
    #[cfg(feature = "diagnostics")]
    pub(crate) shows: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Waiting {
    due: u64,
    written: u64,
    write: u64,
    looks: u8,
    mapped: u64,
    checked: (u64, u64, u64, u8, u64),
}

#[derive(Default)]
pub(crate) struct TextureQueue {
    slots: std::collections::HashMap<u32, Waiting>,
    writes: u64,
    held: std::collections::HashMap<u32, u64>,
    // High-water frame promoted into the ready maps. Every slot in `slots` lives
    // in exactly one of:
    // - `ready_again` (keyed by `write`) if `due <= ready_until && (looks > 0 || mapped > 0)`
    // - `ready_fresh` (keyed by `write`) if `due <= ready_until && looks == 0 && mapped == 0`
    // - `future` (ordered by `(due, index)`) if `due > ready_until`
    ready_until: u64,
    ready_again: std::collections::BTreeMap<u64, u32>,
    ready_fresh: std::collections::BTreeMap<u64, u32>,
    future: std::collections::BTreeSet<(u64, u32)>,
    // Reference-counted `written` frames of entries in `ready_*` for O(1) oldest-due lookup.
    ready_written: std::collections::BTreeMap<u64, u32>,
    mapped_count: u64,
    looks_only_count: u64,
}

impl TextureQueue {
    #[inline]
    fn is_again(w: &Waiting) -> bool {
        w.looks > 0 || w.mapped > 0
    }

    fn ready_written_inc(&mut self, written: u64) {
        *self.ready_written.entry(written).or_insert(0) += 1;
    }

    fn ready_written_dec(&mut self, written: u64) {
        if let std::collections::btree_map::Entry::Occupied(mut e) =
            self.ready_written.entry(written)
        {
            if *e.get() <= 1 {
                e.remove();
            } else {
                *e.get_mut() -= 1;
            }
        }
    }

    fn index_insert(&mut self, index: u32, w: Waiting) {
        if w.mapped > 0 {
            self.mapped_count += 1;
        } else if w.looks > 0 {
            self.looks_only_count += 1;
        }
        if w.due <= self.ready_until {
            if Self::is_again(&w) {
                self.ready_again.insert(w.write, index);
            } else {
                self.ready_fresh.insert(w.write, index);
            }
            self.ready_written_inc(w.written);
        } else {
            self.future.insert((w.due, index));
        }
    }

    fn index_remove(&mut self, index: u32, w: Waiting) {
        if w.mapped > 0 {
            self.mapped_count -= 1;
        } else if w.looks > 0 {
            self.looks_only_count -= 1;
        }
        if w.due <= self.ready_until {
            if Self::is_again(&w) {
                self.ready_again.remove(&w.write);
            } else {
                self.ready_fresh.remove(&w.write);
            }
            self.ready_written_dec(w.written);
        } else {
            self.future.remove(&(w.due, index));
        }
    }

    fn promote(&mut self, frame: u64) {
        if frame <= self.ready_until || frame == u64::MAX {
            return;
        }
        while let Some(&(due, _)) = self.future.first() {
            if due > frame {
                break;
            }
            let Some((_, index)) = self.future.pop_first() else {
                break;
            };
            if let Some(&w) = self.slots.get(&index) {
                if Self::is_again(&w) {
                    self.ready_again.insert(w.write, index);
                } else {
                    self.ready_fresh.insert(w.write, index);
                }
                self.ready_written_inc(w.written);
            }
        }
        self.ready_until = frame;
    }

    fn written(&mut self, index: u32, frame: u64, due: u64, seen: bool) {
        if seen && self.slots.contains_key(&index) {
            return;
        }
        self.writes += 1;
        let write = self.writes;
        let w = Waiting {
            due,
            written: frame,
            write,
            looks: 0,
            mapped: 0,
            checked: (0, 0, 0, 0, 0),
        };
        if let Some(old) = self.slots.insert(index, w) {
            self.index_remove(index, old);
        }
        self.held.remove(&index);
        self.index_insert(index, w);
    }

    fn remove(&mut self, index: u32) -> bool {
        self.held.remove(&index);
        if let Some(old) = self.slots.remove(&index) {
            self.index_remove(index, old);
            true
        } else {
            false
        }
    }

    fn due(&self, frame: u64, max: usize) -> Vec<(u32, u64)> {
        if max == 0 {
            return Vec::new();
        }
        let no_unpromoted = frame >= self.ready_until
            && self.future.first().is_none_or(|&(due, _)| due > frame);
        if no_unpromoted {
            let cap = max.min(self.ready_again.len() + self.ready_fresh.len());
            let mut order = Vec::with_capacity(cap);
            for (&write, &index) in self.ready_again.iter().rev().take(max) {
                order.push((index, write));
            }
            if order.len() < max {
                let mut fresh = self.ready_fresh.iter();
                while order.len() < max {
                    let Some((&write, &index)) = fresh.next_back() else {
                        break;
                    };
                    order.push((index, write));
                    if order.len() < max {
                        let Some((&write, &index)) = fresh.next() else {
                            break;
                        };
                        order.push((index, write));
                    }
                }
            }
            return order;
        }
        if frame < self.ready_until {
            let mut order = Vec::with_capacity(max.min(self.slots.len()));
            for (&write, &index) in self.ready_again.iter().rev() {
                if self.slots.get(&index).is_some_and(|w| w.due <= frame) {
                    order.push((index, write));
                    if order.len() == max {
                        return order;
                    }
                }
            }
            let fresh: Vec<(u32, u64)> = self
                .ready_fresh
                .iter()
                .filter(|&(_, index)| self.slots.get(index).is_some_and(|w| w.due <= frame))
                .map(|(&write, &index)| (index, write))
                .collect();
            let (mut lo, mut hi) = (0, fresh.len());
            while lo < hi && order.len() < max {
                hi -= 1;
                order.push(fresh[hi]);
                if lo < hi && order.len() < max {
                    order.push(fresh[lo]);
                    lo += 1;
                }
            }
            return order;
        }
        // `frame > self.ready_until` with unpromoted entries in `future.range(..=(frame, u32::MAX))`.
        // Only visit entries actually due at `<= frame`, never scanning future-due slots.
        let mut extra_again: Vec<(u64, u32)> = Vec::new();
        let mut extra_fresh: Vec<(u64, u32)> = Vec::new();
        for &(_, index) in self.future.range(..=(frame, u32::MAX)) {
            if let Some(w) = self.slots.get(&index) {
                if Self::is_again(w) {
                    extra_again.push((w.write, index));
                } else {
                    extra_fresh.push((w.write, index));
                }
            }
        }
        extra_again.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        extra_fresh.sort_unstable_by_key(|&(write, _)| write);
        let mut order = Vec::with_capacity(
            max.min(
                self.ready_again.len()
                    + extra_again.len()
                    + self.ready_fresh.len()
                    + extra_fresh.len(),
            ),
        );
        // Merge `ready_again` (descending) and `extra_again` (descending).
        let mut r_again = self.ready_again.iter().rev().peekable();
        let mut e_again = extra_again.iter().peekable();
        while order.len() < max {
            match (r_again.peek(), e_again.peek()) {
                (Some(&(&rw, &ri)), Some(&&(ew, ei))) => {
                    if rw >= ew {
                        r_again.next();
                        order.push((ri, rw));
                    } else {
                        e_again.next();
                        order.push((ei, ew));
                    }
                }
                (Some(&(&rw, &ri)), None) => {
                    r_again.next();
                    order.push((ri, rw));
                }
                (None, Some(&&(ew, ei))) => {
                    e_again.next();
                    order.push((ei, ew));
                }
                (None, None) => break,
            }
        }
        if order.len() < max {
            // Merge `ready_fresh` (ascending) and `extra_fresh` (ascending) into ascending Vec,
            // then alternate newest (`hi`) and oldest (`lo`).
            let mut fresh = Vec::with_capacity(self.ready_fresh.len() + extra_fresh.len());
            let mut r_fresh = self.ready_fresh.iter().peekable();
            let mut e_fresh = extra_fresh.iter().peekable();
            loop {
                match (r_fresh.peek(), e_fresh.peek()) {
                    (Some(&(&rw, &ri)), Some(&&(ew, ei))) => {
                        if rw <= ew {
                            r_fresh.next();
                            fresh.push((ri, rw));
                        } else {
                            e_fresh.next();
                            fresh.push((ei, ew));
                        }
                    }
                    (Some(&(&rw, &ri)), None) => {
                        r_fresh.next();
                        fresh.push((ri, rw));
                    }
                    (None, Some(&&(ew, ei))) => {
                        e_fresh.next();
                        fresh.push((ei, ew));
                    }
                    (None, None) => break,
                }
            }
            let (mut lo, mut hi) = (0, fresh.len());
            while lo < hi && order.len() < max {
                hi -= 1;
                order.push(fresh[hi]);
                if lo < hi && order.len() < max {
                    order.push(fresh[lo]);
                    lo += 1;
                }
            }
        }
        order
    }

    #[inline]
    fn make_due_slot(&self, index: u32, write: u64) -> DueSlot {
        match self.slots.get(&index) {
            Some(w) => DueSlot {
                index,
                write,
                mapped: w.mapped,
                checked: (w.checked.2 != 0).then_some(w.checked),
            },
            None => DueSlot {
                index,
                write,
                mapped: 0,
                checked: None,
            },
        }
    }

    fn due_batch_into(&self, frame: u64, max: usize, out: &mut Vec<DueSlot>) {
        out.clear();
        if max == 0 {
            return;
        }
        let no_unpromoted = frame >= self.ready_until
            && self.future.first().is_none_or(|&(due, _)| due > frame);
        if no_unpromoted {
            let cap = max.min(self.ready_again.len() + self.ready_fresh.len());
            out.reserve(cap.saturating_sub(out.capacity()));
            for (&write, &index) in self.ready_again.iter().rev().take(max) {
                out.push(self.make_due_slot(index, write));
            }
            if out.len() < max {
                let mut fresh = self.ready_fresh.iter();
                while out.len() < max {
                    let Some((&write, &index)) = fresh.next_back() else {
                        break;
                    };
                    out.push(self.make_due_slot(index, write));
                    if out.len() < max {
                        let Some((&write, &index)) = fresh.next() else {
                            break;
                        };
                        out.push(self.make_due_slot(index, write));
                    }
                }
            }
            return;
        }
        for (index, write) in self.due(frame, max) {
            out.push(self.make_due_slot(index, write));
        }
    }

    fn all_due(&self, frame: u64) -> Vec<(u32, bool)> {
        let mut out = Vec::new();
        for &index in self.ready_again.values().chain(self.ready_fresh.values()) {
            if let Some(w) = self.slots.get(&index)
                && w.due <= frame
            {
                out.push((index, w.mapped > 0 || w.checked.2 != 0));
            }
        }
        if frame > self.ready_until {
            for &(_, index) in self.future.range(..=(frame, u32::MAX)) {
                if let Some(w) = self.slots.get(&index) {
                    out.push((index, w.mapped > 0 || w.checked.2 != 0));
                }
            }
        }
        out
    }

    fn done(&mut self, index: u32, write: u64) -> bool {
        if self.slots.get(&index).is_some_and(|w| w.write == write) {
            let old = self.slots.remove(&index).unwrap();
            self.index_remove(index, old);
            self.held.remove(&index);
            true
        } else {
            false
        }
    }

    fn later(&mut self, index: u32, write: u64, frame: u64, most: u64) {
        if let Some(&old) = self.slots.get(&index).filter(|w| w.write == write) {
            self.index_remove(index, old);
            let mut w = old;
            w.due = frame + (1u64 << w.looks.min(16)).min(most.max(1));
            w.looks = w.looks.saturating_add(1);
            self.slots.insert(index, w);
            self.index_insert(index, w);
        }
    }

    fn hold(&mut self, index: u32, write: u64, due: u64) {
        if let Some(&old) = self.slots.get(&index).filter(|w| w.write == write) {
            let new_due = old.due.max(due);
            if new_due != old.due {
                self.index_remove(index, old);
                let mut w = old;
                w.due = new_due;
                self.slots.insert(index, w);
                self.index_insert(index, w);
            }
            self.held.insert(index, write);
        }
    }

    fn room(&mut self, frame: u64) -> usize {
        let held = std::mem::take(&mut self.held);
        let mut back = 0;
        for (index, write) in held {
            if let Some(&old) = self.slots.get(&index).filter(|w| w.write == write) {
                let new_due = old.due.min(frame);
                if new_due != old.due {
                    self.index_remove(index, old);
                    let mut w = old;
                    w.due = new_due;
                    self.slots.insert(index, w);
                    self.index_insert(index, w);
                }
                back += 1;
            }
        }
        back
    }

    fn partly(&mut self, index: u32, write: u64, due: u64, mapped: u64) {
        if let Some(&old) = self.slots.get(&index).filter(|w| w.write == write) {
            self.index_remove(index, old);
            let mut w = old;
            (w.due, w.looks, w.mapped) = (due, 0, mapped.max(1));
            self.slots.insert(index, w);
            self.index_insert(index, w);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn recheck(
        &mut self,
        index: u32,
        write: u64,
        due: u64,
        at: (u64, u64),
        print: u64,
        looks: u8,
        uploaded: u64,
    ) {
        if let Some(&old) = self.slots.get(&index).filter(|w| w.write == write) {
            self.index_remove(index, old);
            let mut w = old;
            (w.due, w.looks, w.mapped) = (due, 1, 0);
            w.checked = (at.0, at.1, print, looks, uploaded);
            self.slots.insert(index, w);
            self.index_insert(index, w);
        }
    }

    fn changed(&mut self, index: u32, frame: u64, due: u64) {
        self.writes += 1;
        let write = self.writes;
        let due = self.slots.get(&index).map_or(due, |w| w.due.min(due));
        let w = Waiting {
            due,
            written: frame,
            write,
            looks: 0,
            mapped: 0,
            checked: (0, 0, 0, 0, 0),
        };
        if let Some(old) = self.slots.insert(index, w) {
            self.index_remove(index, old);
        }
        self.held.remove(&index);
        self.index_insert(index, w);
    }

    #[cfg(test)]
    fn mapped(&self, index: u32) -> u64 {
        self.slots.get(&index).map_or(0, |w| w.mapped)
    }

    fn census(&self, frame: u64) -> [u64; 5] {
        let mut c = [
            self.slots.len() as u64,
            0,
            self.looks_only_count,
            self.mapped_count,
            0,
        ];
        if frame >= self.ready_until {
            c[1] = (self.ready_again.len() + self.ready_fresh.len()) as u64;
            if let Some(&oldest_written) = self.ready_written.keys().next() {
                c[4] = frame.saturating_sub(oldest_written);
            }
            if frame > self.ready_until && self.future.first().is_some_and(|&(due, _)| due <= frame)
            {
                for &(_, index) in self.future.range(..=(frame, u32::MAX)) {
                    if let Some(w) = self.slots.get(&index) {
                        c[1] += 1;
                        c[4] = c[4].max(frame.saturating_sub(w.written));
                    }
                }
            }
        } else {
            for &index in self.ready_again.values().chain(self.ready_fresh.values()) {
                if let Some(w) = self.slots.get(&index)
                    && w.due <= frame
                {
                    c[1] += 1;
                    c[4] = c[4].max(frame.saturating_sub(w.written));
                }
            }
        }
        c
    }
}

pub(crate) fn is_texture_view(view: &View) -> bool {
    match view {
        View::Placed(false, resource, _) => resource[0] != 1,
        View::Shader(r, _) => crate::resource::buffer_bytes(*r as crate::object::This).is_none(),
        _ => false,
    }
}

pub(crate) static REFUSED_VIEWS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn device_removed() -> Option<String> {
    let host = host()?;
    // SAFETY: a plain query.
    let reason = unsafe { host.device.GetDeviceRemovedReason() };
    reason.err().map(|e| e.to_string())
}

pub(crate) static PLACED_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static OBJECT_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn view_stride(view: &View) -> Option<u32> {
    const BUFFER: u32 = 1;
    let (words, unordered): (&[u32; 10], bool) = match view {
        View::Shader(_, Some(d)) if d[1] == BUFFER => (d, false),
        View::Unordered(_, _, Some(d)) if d[1] == BUFFER => (d, true),
        View::Placed(unordered, resource, d) if resource[0] == BUFFER => {
            (d[..10].try_into().ok()?, *unordered)
        }
        _ => return None,
    };
    // SAFETY: buffer views' union members.
    let (stride, raw, format) = unsafe {
        if unordered {
            let d = uav_desc(words);
            let b = d.Anonymous.Buffer;
            (
                b.StructureByteStride,
                b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0,
                d.Format,
            )
        } else {
            let d = srv_desc(words);
            let b = d.Anonymous.Buffer;
            (
                b.StructureByteStride,
                b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0,
                d.Format,
            )
        }
    };
    Some(if stride > 0 {
        stride
    } else if raw {
        1
    } else {
        crate::resource::element(format.0 as u32).map_or(0, |(bytes, _)| bytes as u32)
    })
}

static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!(
                "{:#x} past its end (slot {})",
                va - start - bytes,
                (va - start) / step
            )
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

const PORTABLE_VIEWS: u32 = 1_000_000;

const UNCAPPED_VIEWS: u32 = 1 << 21;

pub(crate) fn probe_view_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(host) = host() else { return };
        let fits = |count: u32| {
            // SAFETY: plain heap creation; the heap is released at once.
            let made: windows::core::Result<ID3D12DescriptorHeap> = unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            };
            made.is_ok()
        };
        let views = if fits(UNCAPPED_VIEWS) {
            UNCAPPED_VIEWS
        } else {
            PORTABLE_VIEWS
        };
        tracing::info!(
            views,
            "host view heaps hold {views} views; view indices are clamped to the null slot at the end from the first shader on"
        );
        oathkeep_shader::emit::set_view_heap_limit(views - 1);
    });
}

pub(crate) const DYNAMIC: u32 = 1 << 18;

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        let extra = if sampler { 0 } else { DYNAMIC };
        if !sampler {
            probe_view_limit();
        }
        let null = if sampler {
            0
        } else {
            oathkeep_shader::emit::view_heap_limit()
        };
        let (heap, count, size) = if null == 0 {
            match make(count + extra) {
                Ok(heap) => (heap, count, count + extra),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else {
            let size = null + 1;
            let fitted = if count + extra <= null {
                count
            } else {
                tracing::warn!(
                    count,
                    "host view heap capped at {size}; views past it read a null slot"
                );
                null - extra
            };
            match make(size) {
                Ok(heap) => (heap, fitted, size),
                Err(e) => {
                    tracing::warn!(error = %e, count, size, "no host view heap");
                    return None;
                }
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(size) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let strides = (!sampler)
            .then(|| {
                let bytes = u64::from(oathkeep_shader::emit::SLOT_STRIDES_COUNT) * 4;
                let (buffer, data) = host.upload_buffer(bytes)?;
                // SAFETY: the buffer's whole mapping; a live buffer.
                let gpu = unsafe {
                    std::ptr::write_bytes(data, 0, bytes as usize);
                    buffer.GetGPUVirtualAddress()
                };
                crate::host::name(&buffer, || "element strides by view heap slot".to_owned());
                Some((buffer, gpu, data as usize))
            })
            .flatten();
        let h = HostHeap {
            heap,
            cpu,
            gpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made_keys: (0..count)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
            made_locks: Box::new(std::array::from_fn(|_| Mutex::default())),
            kept: {
                let total_slots = (count + extra) as usize;
                Box::new(std::array::from_fn(|stripe| {
                    let stripe_len = if total_slots > stripe {
                        (total_slots - stripe).div_ceil(MADE_LOCK_STRIPES)
                    } else {
                        0
                    };
                    Mutex::new(vec![None; stripe_len])
                }))
            },
            has_kept: bit_words(count + extra),
            is_buffer: bit_words(if sampler { 0 } else { count }),
            in_texture_queue: bit_words(if sampler { 0 } else { count }),
            in_placed: bit_words(if sampler { 0 } else { count }),
            next: Mutex::new(DynamicSlots::new(if sampler { 0 } else { extra })),
            free: std::sync::atomic::AtomicU32::new(extra),
            too_long: std::sync::atomic::AtomicU32::new(u32::MAX),
            strides,
            buffers: Mutex::default(),
            changed: Mutex::default(),
            textures: Mutex::default(),
            tabled: (0..if sampler { 0 } else { count })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            placed: {
                let spans = std::sync::Arc::<Mutex<Spans>>::default();
                if !sampler {
                    PLACED_SPANS
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(std::sync::Arc::downgrade(&spans));
                }
                spans
            },
        };
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, size) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            "host descriptor heap made null"
        );
        Some(h)
    }

    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: BLOCK,
                        ..Default::default()
                    })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device
                    .CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device
                    .CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::DynamicAsked, 1);
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let enough = self.free.load(std::sync::atomic::Ordering::Relaxed) >= n
            && n < self.too_long.load(std::sync::atomic::Ordering::Relaxed);
        let found = if enough { guard.alloc(n) } else { None };
        let Some(first) = found else {
            if enough {
                self.too_long
                    .fetch_min(n, std::sync::atomic::Ordering::Relaxed);
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(n, "no free dynamic descriptor slots: a table is not made")
            });
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::DynamicRefused, 1);
                add(C::DynamicNs, started.elapsed().as_nanos() as u64);
            }
            return None;
        };
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::DynamicNs,
            started.elapsed().as_nanos() as u64,
        );
        self.free
            .fetch_sub(n, std::sync::atomic::Ordering::Relaxed);
        Some(self.count + first)
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    pub(crate) fn strides_gpu(&self) -> Option<u64> {
        self.strides.as_ref().map(|s| s.1)
    }

    pub(crate) fn strides_buffer(&self) -> Option<&ID3D12Resource> {
        self.strides.as_ref().map(|s| &s.0)
    }

    #[cfg(test)]
    pub(crate) fn texture_slots_due(&self, frame: u64, max: usize) -> (Vec<(u32, u64)>, [u64; 5]) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.promote(frame);
        (
            if max == 0 {
                Vec::new()
            } else {
                textures.due(frame, max)
            },
            textures.census(frame),
        )
    }

    #[inline]
    fn made_lock(&self, index: u32) -> &Mutex<std::collections::HashMap<u32, u64>> {
        &self.made_locks[(index as usize) & (MADE_LOCK_STRIPES - 1)]
    }

    fn apply_texture_updates_locked(
        &self,
        textures: &mut TextureQueue,
        updates: &mut Vec<TextureSlotUpdate>,
    ) {
        for update in updates.drain(..) {
            match update {
                TextureSlotUpdate::Done {
                    index,
                    write,
                    again,
                } => match again {
                    Some((frame, mapped)) => textures.partly(index, write, frame, mapped),
                    None => {
                        if textures.done(index, write) {
                            bit_set(&self.in_texture_queue, index, false);
                        }
                    }
                },
                TextureSlotUpdate::Recheck {
                    index,
                    write,
                    due,
                    at,
                    print,
                    looks,
                    uploaded,
                } => textures.recheck(index, write, due, at, print, looks, uploaded),
                TextureSlotUpdate::Later {
                    index,
                    write,
                    frame,
                    most,
                } => textures.later(index, write, frame, most),
                TextureSlotUpdate::Held { index, write, due } => textures.hold(index, write, due),
            }
        }
    }

    pub(crate) fn texture_apply_updates(&self, updates: &mut Vec<TextureSlotUpdate>) {
        if updates.is_empty() {
            return;
        }
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        self.apply_texture_updates_locked(&mut textures, updates);
    }

    pub(crate) fn texture_batch_step(
        &self,
        frame: u64,
        max: usize,
        updates: &mut Vec<TextureSlotUpdate>,
        out: &mut Vec<DueSlot>,
    ) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        if !updates.is_empty() {
            self.apply_texture_updates_locked(&mut textures, updates);
        }
        textures.due_batch_into(frame, max, out);
    }

    pub(crate) fn texture_slots_due_batch(
        &self,
        frame: u64,
        max: usize,
        promote_and_census: bool,
        out: &mut Vec<DueSlot>,
    ) -> Option<[u64; 5]> {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        let census = if promote_and_census {
            textures.promote(frame);
            Some(textures.census(frame))
        } else {
            None
        };
        textures.due_batch_into(frame, max, out);
        census
    }

    pub(crate) fn texture_slots_all_due(&self, frame: u64) -> Vec<(u32, bool)> {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.all_due(frame)
    }

    #[cfg(test)]
    pub(crate) fn texture_slot_done(&self, index: u32, write: u64, again: Option<(u64, u64)>) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match again {
            Some((frame, mapped)) => textures.partly(index, write, frame, mapped),
            None => {
                if textures.done(index, write) {
                    bit_set(&self.in_texture_queue, index, false);
                }
            }
        }
    }

    pub(crate) fn texture_slots_room(&self, frame: u64) -> usize {
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .room(frame)
    }

    pub(crate) fn texture_slots_remapped(&self, frame: u64) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        self.texture_slots_page_written(frame);
        let mut ranges = std::mem::take(&mut *REMAPPED.lock().unwrap_or_else(|e| e.into_inner()));
        if ranges.is_empty() {
            return 0;
        }
        coalesce_u64_ranges(&mut ranges);
        if ranges.is_empty() {
            return 0;
        }
        let mut slots = Vec::new();
        {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            for &(start, end) in &ranges {
                placed.extend_slots_in(start, end, &mut slots);
            }
        }
        if slots.is_empty() {
            return 0;
        }
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        if slots.is_empty() {
            return 0;
        }
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        for &slot in &slots {
            textures.changed(slot, frame, frame + 1);
            bit_set(&self.in_texture_queue, slot, true);
        }
        STALE[3].fetch_add(slots.len() as u64, Relaxed);
        slots.len()
    }

    fn texture_slots_page_written(&self, frame: u64) -> usize {
        let mut ranges =
            std::mem::take(&mut *PAGE_WRITTEN.lock().unwrap_or_else(|e| e.into_inner()));
        if ranges.is_empty() {
            return 0;
        }
        coalesce_u64_ranges(&mut ranges);
        if ranges.is_empty() {
            return 0;
        }
        self.slots_written_under(&ranges, frame, crate::watch::page_writes() & 2 != 0)[2]
    }

    fn slots_written_under(&self, ranges: &[(u64, u64)], frame: u64, due: bool) -> [usize; 3] {
        use std::sync::atomic::Ordering::Relaxed;
        let mut slots = Vec::new();
        {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            for &(start, end) in ranges {
                placed.extend_slots_holding(start, end, &mut slots);
            }
        }
        if slots.is_empty() {
            return [0, 0, 0];
        }
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let held = slots.len();
        if held == 0 {
            return [0, 0, 0];
        }
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|slot| {
            textures
                .slots
                .get(slot)
                .is_none_or(|w| w.mapped > 0 || w.checked.2 != 0)
        });
        PAGE_WRITES[2].fetch_add(held as u64, Relaxed);
        PAGE_WRITES[3].fetch_add(slots.len() as u64, Relaxed);
        if !due {
            return [held, slots.len(), 0];
        }
        for &slot in &slots {
            textures.changed(slot, frame, frame + PAGE_WRITE_AFTER);
            bit_set(&self.in_texture_queue, slot, true);
        }
        [held, slots.len(), slots.len()]
    }

    pub(crate) fn texture_slot_made_from(&self, index: u32, handle: u64) -> bool {
        if index >= self.count {
            return false;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let key = u64::from(generation) << 32 | u64::from(i);
        if tag != VIEW_TAG {
            return false;
        }
        self.made_keys
            .get(index as usize)
            .is_some_and(|slot| {
                let made = slot.load(std::sync::atomic::Ordering::Acquire);
                made != 0 && made == key
            })
            && bit_get(&self.has_kept, index)
    }

    pub(crate) fn texture_slots_waiting(&self, frame: u64) -> usize {
        if self.sampler {
            return 0;
        }
        let oldest = WAITING_OLDEST.load(std::sync::atomic::Ordering::Acquire);
        if oldest == 0 || !crate::host::long_ago(oldest, 1000, 2) {
            return 0;
        }
        let starts: Vec<u64> = {
            let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
            if waiting.is_empty() {
                WAITING_OLDEST.store(0, std::sync::atomic::Ordering::Release);
                return 0;
            }
            let mut starts = Vec::new();
            let mut next_oldest = u64::MAX;
            for (&start, &since) in waiting.iter() {
                if starts.len() < Self::WAITING_A_PASS && crate::host::long_ago(since, 1000, 2) {
                    starts.push(start);
                } else if since < next_oldest {
                    next_oldest = since;
                }
            }
            for start in &starts {
                waiting.remove(start);
            }
            let updated_oldest = if waiting.is_empty() {
                0
            } else if next_oldest == u64::MAX {
                oldest
            } else {
                next_oldest
            };
            WAITING_OLDEST.store(updated_oldest, std::sync::atomic::Ordering::Release);
            starts
        };
        if starts.is_empty() {
            return 0;
        }
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            starts
                .iter()
                .flat_map(|&start| placed.slots_at(start))
                .collect()
        };
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        for &slot in &slots {
            textures.changed(slot, frame, frame + 1);
            bit_set(&self.in_texture_queue, slot, true);
        }
        crate::tiles::STATS[10].fetch_add(slots.len() as u64, std::sync::atomic::Ordering::Relaxed);
        slots.len()
    }

    const WAITING_A_PASS: usize = 128;

    pub(crate) fn texture_views_remade(&self) -> usize {
        if self.sampler {
            return 0;
        }
        let mut starts = std::mem::take(&mut *RESHOWN.lock().unwrap_or_else(|e| e.into_inner()));
        if starts.is_empty() {
            return 0;
        }
        starts.sort_unstable();
        starts.dedup();
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            starts
                .iter()
                .flat_map(|&start| placed.slots_at(start))
                .collect()
        };
        if slots.is_empty() {
            return 0;
        }
        slots.sort_unstable();
        slots.dedup();
        let mut remade = 0;
        for slot in slots {
            let Some(entry) = self.made_keys.get(slot as usize) else {
                continue;
            };
            if entry.load(std::sync::atomic::Ordering::Acquire) == 0 {
                continue;
            }
            let _made_guard = self.made_lock(slot).lock().unwrap_or_else(|e| e.into_inner());
            let key = entry.load(std::sync::atomic::Ordering::Relaxed);
            if key == 0 {
                continue;
            }
            let Some(view) = view_at(key as u32, (key >> 32) as u32) else {
                continue;
            };
            if let Some(kept) = make(&view, self.cpu(slot)) {
                self.keep(slot, kept);
                remade += 1;
            }
        }
        remade
    }

    pub(crate) fn texture_slot_gone(&self, index: u32) -> bool {
        if self
            .made_keys
            .get(index as usize)
            .is_none_or(|s| s.load(std::sync::atomic::Ordering::Acquire) == 0)
        {
            return false;
        }
        let mut deferred = self
            .made_lock(index)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self
            .made_keys
            .get(index as usize)
            .is_none_or(|s| s.load(std::sync::atomic::Ordering::Relaxed) == 0)
        {
            return false;
        }
        deferred.remove(&index);
        self.unmake(index);
        STALE[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    pub(crate) fn unmake_slot(&self, index: u32) {
        let mut deferred = self
            .made_lock(index)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        deferred.remove(&index);
        self.unmake(index);
    }

    fn unmake(&self, index: u32) {
        let Some(slot) = self.made_keys.get(index as usize) else {
            return;
        };
        let key = slot.swap(0, std::sync::atomic::Ordering::Release);
        if key == 0 {
            return;
        }
        match view_at(key as u32, (key >> 32) as u32) {
            Some(view) => null_for(&view, self.cpu(index)),
            None => null_view(self.cpu(index)),
        }
        self.keep(index, None);
    }

    pub(crate) fn texture_slot_tabled(&self, index: u32, read: bool) {
        self.tabled_in(index, read, crate::frame::current());
    }

    fn tabled_in(&self, index: u32, read: bool, frame: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(stamp) = self.tabled.get(index as usize) else {
            return;
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        let old = stamp.load(Relaxed);
        if !read {
            if old & !TABLED_READ != now {
                stamp.store(now, Relaxed);
            }
            return;
        }
        if old != now | TABLED_READ {
            stamp.store(now | TABLED_READ, Relaxed);
            if tabled_out() && bit_get(&self.in_texture_queue, index) {
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                if textures.remove(index) {
                    bit_set(&self.in_texture_queue, index, false);
                    TABLED_WRITES[1].fetch_add(1, Relaxed);
                }
            }
        }
    }

    fn tabled_lately(&self, index: u32, frame: u64) -> bool {
        let stamp = self.tabled_stamp(index) & !TABLED_READ;
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        tabled_out() && stamp != 0 && u64::from(now.wrapping_sub(stamp)) <= TABLED_FRAMES
    }

    fn tabled_stamp(&self, index: u32) -> u32 {
        self.tabled
            .get(index as usize)
            .map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn queue_texture(&self, index: u32, texture: bool, seen: bool, frame: u64) {
        let stamp = if tabled_out() {
            self.tabled_stamp(index)
        } else {
            0
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        let lately =
            stamp != 0 && u64::from(now.wrapping_sub(stamp & !TABLED_READ)) <= TABLED_FRAMES;
        let read_now = seen && stamp == now | TABLED_READ;
        if texture && !read_now {
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            let due = if lately && !seen {
                TABLED_WRITES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                frame + TABLED_GRACE
            } else {
                0
            };
            textures.written(index, frame, due, seen);
            bit_set(&self.in_texture_queue, index, true);
        } else if bit_get(&self.in_texture_queue, index) {
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            if textures.remove(index) {
                bit_set(&self.in_texture_queue, index, false);
            }
        }
    }

    pub(crate) fn buffer_slots(&self) -> Vec<u32> {
        let buffers = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
        buffers.iter().copied().collect()
    }

    pub(crate) fn buffer_changes(&self) -> Vec<u32> {
        let mut changed = self.changed.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *changed).into_iter().collect()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn note_buffer(&self, index: u32, view: Option<&View>, seen: bool) -> Vec<u32> {
        self.note_buffer_inner(index, view, seen, true)
    }

    fn note_buffer_inner(
        &self,
        index: u32,
        view: Option<&View>,
        seen: bool,
        notify_changed: bool,
    ) -> Vec<u32> {
        if self.sampler {
            return Vec::new();
        }
        let texture = match view {
            Some(View::Placed(false, resource, _)) if resource[0] != 1 => {
                PLACED_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            Some(View::Shader(r, _))
                if crate::resource::buffer_bytes(*r as crate::object::This).is_none() =>
            {
                OBJECT_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        };
        let frame = crate::frame::current();
        self.queue_texture(index, texture, seen, frame);
        let placed_view = match view {
            Some(View::Placed(false, resource, desc))
                if resource[0] != 1 && !self.tabled_lately(index, frame) =>
            {
                Some((
                    resource,
                    u64::from(desc[10]) | u64::from(desc[11]) << 32,
                    desc[4],
                ))
            }
            _ => None,
        };
        let stale = if let Some((resource, start, first)) = placed_view {
            use std::sync::atomic::Ordering::Relaxed;
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            let span = placed
                .bytes(resource)
                .map(|bytes| (start, start.saturating_add(bytes), first));
            match span {
                Some((start, end, first)) => {
                    SPANNED[0].fetch_min(start, Relaxed);
                    SPANNED[1].fetch_max(end, Relaxed);
                    placed.rule = stale_rule();
                    let stale = placed.note(index, start, end, !seen);
                    for &slot in &stale {
                        bit_set(&self.in_placed, slot, false);
                    }
                    let present = placed.by_slot.contains_key(&index);
                    bit_set(&self.in_placed, index, present);
                    placed.shows(index, first);
                    stale
                }
                None => {
                    placed.forget(index);
                    bit_set(&self.in_placed, index, false);
                    Vec::new()
                }
            }
        } else if bit_get(&self.in_placed, index) {
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            placed.forget(index);
            bit_set(&self.in_placed, index, false);
            Vec::new()
        } else {
            Vec::new()
        };
        if !stale.is_empty() {
            STALE[0].fetch_add(stale.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            for &slot in &stale {
                if textures.remove(slot) {
                    bit_set(&self.in_texture_queue, slot, false);
                }
            }
        }
        let stride = view.and_then(view_stride);
        if let Some((_, _, data)) = &self.strides
            && index < oathkeep_shader::emit::SLOT_STRIDES_COUNT
        {
            // SAFETY: inside the mapping; shaders read it while we write (a
            // dword, either value is one the slot had).
            unsafe {
                (*data as *mut u32)
                    .add(index as usize)
                    .write_volatile(stride.unwrap_or(0))
            };
        }
        let was_buffer = bit_get(&self.is_buffer, index);
        let was = if stride.is_some() {
            if !was_buffer {
                bit_set(&self.is_buffer, index, true);
                let mut buffers = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
                buffers.insert(index);
            }
            true
        } else if was_buffer {
            bit_set(&self.is_buffer, index, false);
            let mut buffers = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
            buffers.remove(&index)
        } else {
            false
        };
        if was && notify_changed {
            self.changed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(index);
        }
        stale
    }

    pub(crate) fn release(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let end = at.saturating_add(n).min(self.dynamic);
        {
            let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
            let given = guard.free(at, end.saturating_sub(at));
            self.free
                .fetch_add(given, std::sync::atomic::Ordering::Relaxed);
            self.too_long
                .store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
        }
        let kept_start = self.count + at;
        let kept_end = self.count + end;
        if !bit_any_in_range(&self.has_kept, kept_start, kept_end) {
            return;
        }
        let mut retired = Vec::new();
        let span_len = (kept_end - kept_start) as usize;
        if span_len <= MADE_LOCK_STRIPES {
            for idx in kept_start..kept_end {
                if !bit_get(&self.has_kept, idx) {
                    continue;
                }
                let stripe = idx as usize % MADE_LOCK_STRIPES;
                let pos = idx as usize / MADE_LOCK_STRIPES;
                let mut kept = self.kept[stripe].lock().unwrap_or_else(|e| e.into_inner());
                if let Some(slot) = kept.get_mut(pos)
                    && let Some(old) = slot.take()
                {
                    bit_set(&self.has_kept, idx, false);
                    retired.push(old);
                }
            }
        } else {
            for stripe in 0..MADE_LOCK_STRIPES {
                let rem = kept_start as usize % MADE_LOCK_STRIPES;
                let delta = (stripe + MADE_LOCK_STRIPES - rem) % MADE_LOCK_STRIPES;
                let first_idx = kept_start as usize + delta;
                if first_idx >= kept_end as usize {
                    continue;
                }
                let mut kept = self.kept[stripe].lock().unwrap_or_else(|e| e.into_inner());
                let mut idx = first_idx;
                while idx < kept_end as usize {
                    if bit_get(&self.has_kept, idx as u32)
                        && let Some(slot) = kept.get_mut(idx / MADE_LOCK_STRIPES)
                        && let Some(old) = slot.take()
                    {
                        bit_set(&self.has_kept, idx as u32, false);
                        retired.push(old);
                    }
                    idx += MADE_LOCK_STRIPES;
                }
            }
        }
        crate::host::retire_batch(retired);
    }

    /// Writes a descriptor directly into a dynamic slot owned by the caller's command list,
    /// bypassing `self.kept` locking/retirement. Returns `(kept_resource, made_shadow, made_placed)`
    /// so the caller can keep the resource on the command list (deduplicated) and reuse
    /// `made_shadow` / `made_placed` without a second global lookup.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn make_at_dynamic(
        &self,
        index: u32,
        view: &View,
    ) -> (
        Option<ID3D12Resource>,
        Option<std::sync::Arc<crate::shadow::Shadow>>,
        Option<View>,
    ) {
        let (kept, shadow, placed) = make_detailed(view, self.cpu(index));
        if kept.is_none() {
            null_for(view, self.cpu(index));
        }
        (kept.flatten(), shadow, placed)
    }

    /// Looks up a mirrored slot's cached `StageAction` directly from the CPU staging
    /// cache when `made_keys[index]` already matches the pre-read title descriptor `words`.
    /// Bypasses `VIEWS` RwLock, `View::clone`, `shadow::find`, and `placed_texture` on hits.
    pub(crate) fn lookup_mirrored_stage_words(
        &self,
        index: u32,
        words: [u32; 3],
    ) -> Option<(StageAction, Option<oathkeep_shader::ResourceType>, bool)> {
        if self.sampler || index >= self.count {
            return None;
        }
        let [tag, i, generation] = words;
        if tag != VIEW_TAG {
            return None;
        }
        let made_key = (u64::from(generation) << 32) | u64::from(i);
        if made_key == 0
            || self.made_keys[index as usize].load(std::sync::atomic::Ordering::Acquire) != made_key
        {
            return None;
        }
        let stage = cpu_stage_heap()?;
        let key = (u64::from(i) << 32) | u64::from(generation);
        let stripe_idx = ((key ^ (key >> 17) ^ (key >> 33)) as usize) & (STAGE_STRIPES - 1);
        let guard = stage.stripes[stripe_idx]
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let &local = guard.map.get(&key)?;
        let entry = guard.entries[local].as_ref()?;
        if entry.key != key || !entry.is_valid() {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::StageCacheHits, 1);
        Some((entry.action.clone(), entry.view_kind, entry.is_texture))
    }

    #[allow(dead_code)]
    pub(crate) fn lookup_mirrored_stage(
        &self,
        index: u32,
        handle: u64,
    ) -> Option<(StageAction, Option<oathkeep_shader::ResourceType>, bool)> {
        if self.sampler || index >= self.count {
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let words = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        self.lookup_mirrored_stage_words(index, words)
    }

    /// Batched fast path for `fresh_tables`: looks up all `VIEW_TAG` entries in `used_k`/`words_list`,
    /// acquires unique stage-cache stripe read locks in strictly ascending stripe order
    /// (SRWLock-safe, deadlock-free, and eviction-safe), copies all valid hits in a single
    /// `ID3D12Device::CopyDescriptors` COM call per chunk (up to 32 entries), and writes
    /// `Some(DynamicMade)` into the matching `out[i]` slot. Non-`VIEW_TAG` or cache-miss
    /// entries remain `None` for the caller to handle via `copy_or_make_dynamic`.
    pub(crate) fn copy_dynamic_batch(
        &self,
        first: u32,
        trim: u32,
        used_k: &[u32],
        words_list: &[[u32; 3]],
        out: &mut Vec<BatchSlot>,
        mut already_kept: impl FnMut(usize) -> bool,
    ) {
        out.clear();
        out.resize_with(used_k.len(), || BatchSlot::Unprobed);
        if self.sampler || used_k.len() < 2 {
            return;
        }
        let Some(host) = host() else {
            return;
        };
        let Some(stage) = cpu_stage_heap() else {
            return;
        };
        const CHUNK: usize = 32;
        let mut base = 0usize;
        while base < used_k.len() {
            let end = (base + CHUNK).min(used_k.len());
            let mut candidates = [(0usize, 0u32, 0u64, 0usize); CHUNK];
            let mut n_cand = 0usize;
            let mut stripes = [0usize; CHUNK];
            let mut n_stripes = 0usize;
            for i in base..end {
                let words = words_list[i];
                if words[0] != VIEW_TAG || crate::descriptor::constant_from_words(words).is_some() {
                    continue;
                }
                let key = (u64::from(words[1]) << 32) | u64::from(words[2]);
                let stripe_idx =
                    ((key ^ (key >> 17) ^ (key >> 33)) as usize) & (STAGE_STRIPES - 1);
                candidates[n_cand] = (i, first + (used_k[i] - trim), key, stripe_idx);
                stripes[n_stripes] = stripe_idx;
                n_cand += 1;
                n_stripes += 1;
            }
            if n_cand >= 2 {
                stripes[..n_stripes].sort_unstable();
                let mut uniq = 0usize;
                for j in 0..n_stripes {
                    if uniq == 0 || stripes[j] != stripes[uniq - 1] {
                        stripes[uniq] = stripes[j];
                        uniq += 1;
                    }
                }
                // Acquire stripe read locks in strictly ascending stripe index order:
                // no duplicate stripe locks (SRWLock-safe) and total lock order (deadlock-free).
                let mut guards: [Option<std::sync::RwLockReadGuard<'_, StageStripe>>; CHUNK] =
                    [const { None }; CHUNK];
                for s in 0..uniq {
                    guards[s] = Some(
                        stage.stripes[stripes[s]]
                            .read()
                            .unwrap_or_else(|e| e.into_inner()),
                    );
                }
                let mut dsts = [D3D12_CPU_DESCRIPTOR_HANDLE::default(); CHUNK];
                let mut srcs = [D3D12_CPU_DESCRIPTOR_HANDLE::default(); CHUNK];
                let mut n_copy = 0usize;
                for &(out_i, dst_idx, key, stripe_idx) in &candidates[..n_cand] {
                    let pos = stripes[..uniq].binary_search(&stripe_idx).unwrap();
                    let guard = guards[pos].as_ref().unwrap();
                    if let Some(&local) = guard.map.get(&key)
                        && let Some(entry) = guard.entries[local].as_ref()
                        && entry.key == key
                    {
                        if entry.is_valid() {
                            dsts[n_copy] = self.cpu(dst_idx);
                            srcs[n_copy] = stage.cpu(entry.slot);
                            n_copy += 1;
                            let kept = if entry.kept_raw != 0 && !already_kept(entry.kept_raw) {
                                entry.kept.clone()
                            } else {
                                None
                            };
                            out[out_i] = BatchSlot::Hit(DynamicMade {
                                found: true,
                                kept,
                                action: entry.action.clone(),
                                view_kind: entry.view_kind,
                                is_texture: entry.is_texture,
                            });
                        } else {
                            out[out_i] = BatchSlot::MissEpoch(if entry.is_buffer { 1 } else { 2 });
                        }
                    } else {
                        out[out_i] = BatchSlot::MissNew;
                    }
                }
                if n_copy > 0 {
                    // SAFETY: all `dsts` are freshly allocated dynamic shader-visible slots owned
                    // exclusively by this recording thread; all `srcs` are live CPU staging slots
                    // protected from concurrent eviction by the held stripe read guards.
                    unsafe {
                        if n_copy == 1 {
                            host.device.CopyDescriptorsSimple(
                                1,
                                dsts[0],
                                srcs[0],
                                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                            );
                        } else {
                            host.device.CopyDescriptors(
                                n_copy as u32,
                                dsts.as_ptr(),
                                None,
                                n_copy as u32,
                                srcs.as_ptr(),
                                None,
                                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                            );
                        }
                    }
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::StageCacheHits, n_copy as u64);
                }
                drop(guards);
            }
            base = end;
        }
    }

    /// Copies a cached CPU staging descriptor into dynamic slot `index` via
    /// `CopyDescriptorsSimple`, optionally skipping the initial stripe read-lock probe
    /// when `copy_dynamic_batch` already established `miss_kind` (`0` = new, `1` = buf_epoch,
    /// `2` = tex_epoch). Brand-new misses pass through a lock-free 2-hit admission filter
    /// (`StageHeap::recent_miss`) so one-shot transient views render directly into `dst`
    /// without acquiring the stripe write lock or polluting the staging cache.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn copy_or_make_dynamic_probed(
        &self,
        index: u32,
        words: [u32; 3],
        probed_miss: Option<u8>,
        already_kept: impl FnOnce(usize) -> bool,
    ) -> DynamicMade {
        self.copy_or_make_dynamic_probed_with_shadow(
            index,
            words,
            probed_miss,
            already_kept,
            crate::shadow::find,
        )
    }

    /// Same as `copy_or_make_dynamic_probed`, but routes buffer shadow lookups through the
    /// caller's per-command-list `find_shadow` cache instead of acquiring the global `SHADOWS`
    /// RwLock on every stage-cache miss.
    pub(crate) fn copy_or_make_dynamic_probed_with_shadow(
        &self,
        index: u32,
        words: [u32; 3],
        probed_miss: Option<u8>,
        already_kept: impl FnOnce(usize) -> bool,
        mut find_shadow: impl FnMut(u64, u64) -> Option<std::sync::Arc<crate::shadow::Shadow>>,
    ) -> DynamicMade {
        if self.sampler || words[0] != VIEW_TAG {
            let Some(view) = crate::descriptor::view_from_words(words) else {
                self.make_null_dynamic(index);
                return DynamicMade::missing();
            };
            let is_texture = is_texture_view(&view);
            let view_kind = stage_view_kind(&view);
            let (kept, shadow, placed) =
                make_detailed_with_shadow(&view, self.cpu(index), None, &mut find_shadow);
            if kept.is_none() {
                null_for(&view, self.cpu(index));
            }
            let kept = kept.flatten();
            let resolved = placed.as_ref().unwrap_or(&view);
            let action = if let Some((shadow, address, len)) =
                buffer_copy_with_hint(resolved, shadow)
            {
                StageAction::Buffer {
                    shadow,
                    address,
                    len,
                }
            } else if let View::Shader(resource, _) | View::Unordered(resource, _, _) = resolved {
                StageAction::Texture {
                    resource: *resource,
                }
            } else {
                StageAction::None
            };
            return DynamicMade {
                found: true,
                kept,
                action,
                view_kind,
                is_texture,
            };
        }
        let Some(host) = host() else {
            return DynamicMade::missing();
        };
        let Some(stage) = cpu_stage_heap() else {
            let Some(view) = crate::descriptor::view_from_words(words) else {
                self.make_null_dynamic(index);
                return DynamicMade::missing();
            };
            let is_texture = is_texture_view(&view);
            let view_kind = stage_view_kind(&view);
            let (kept, shadow, placed) =
                make_detailed_with_shadow(&view, self.cpu(index), None, &mut find_shadow);
            if kept.is_none() {
                null_for(&view, self.cpu(index));
            }
            let kept = kept.flatten();
            let resolved = placed.as_ref().unwrap_or(&view);
            let action = if let Some((shadow, address, len)) =
                buffer_copy_with_hint(resolved, shadow)
            {
                StageAction::Buffer {
                    shadow,
                    address,
                    len,
                }
            } else if let View::Shader(resource, _) | View::Unordered(resource, _, _) = resolved {
                StageAction::Texture {
                    resource: *resource,
                }
            } else {
                StageAction::None
            };
            return DynamicMade {
                found: true,
                kept,
                action,
                view_kind,
                is_texture,
            };
        };
        let key = (u64::from(words[1]) << 32) | u64::from(words[2]);
        let stripe_idx = ((key ^ (key >> 17) ^ (key >> 33)) as usize) & (STAGE_STRIPES - 1);
        let dst = self.cpu(index);
        let mut miss_kind: u8 = probed_miss.unwrap_or(0); // 0 = new, 1 = buf_epoch, 2 = tex_epoch
        if probed_miss.is_none() {
            let guard = stage.stripes[stripe_idx]
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(&local) = guard.map.get(&key)
                && let Some(entry) = guard.entries[local].as_ref()
                && entry.key == key
            {
                if entry.is_valid() {
                    // SAFETY: dst is our dynamic shader-visible slot; src is a live CPU staging
                    // slot protected from concurrent overwrite by the stripe read lock.
                    unsafe {
                        host.device.CopyDescriptorsSimple(
                            1,
                            dst,
                            stage.cpu(entry.slot),
                            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        );
                    }
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::StageCacheHits, 1);
                    let kept = if entry.kept_raw != 0 && !already_kept(entry.kept_raw) {
                        entry.kept.clone()
                    } else {
                        None
                    };
                    return DynamicMade {
                        found: true,
                        kept,
                        action: entry.action.clone(),
                        view_kind: entry.view_kind,
                        is_texture: entry.is_texture,
                    };
                }
                miss_kind = if entry.is_buffer { 1 } else { 2 };
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            crate::copystat::add(crate::copystat::C::StageCacheMisses, 1);
            crate::copystat::add(
                match miss_kind {
                    1 => crate::copystat::C::StageCacheMissBufEpoch,
                    2 => crate::copystat::C::StageCacheMissTexEpoch,
                    _ => crate::copystat::C::StageCacheMissNew,
                },
                1,
            );
        }
        let Some(view) = crate::descriptor::view_from_words(words) else {
            self.make_null_dynamic(index);
            return DynamicMade::missing();
        };
        let is_texture = is_texture_view(&view);
        let view_kind = stage_view_kind(&view);
        if miss_kind == 0 {
            let recent_slot = (key.wrapping_mul(0xbf58_476d_1ce4_e5b9) >> 59) as usize;
            let prev = stage.recent_miss[stripe_idx][recent_slot]
                .swap(key, std::sync::atomic::Ordering::Relaxed);
            if prev != key {
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::StageCacheBypass, 1);
                let (kept, made_shadow, made_placed) =
                    make_detailed_with_shadow(&view, dst, None, &mut find_shadow);
                let Some(kept_res) = kept else {
                    null_for(&view, dst);
                    return DynamicMade {
                        found: true,
                        kept: None,
                        action: StageAction::None,
                        view_kind,
                        is_texture,
                    };
                };
                let resolved = made_placed.as_ref().unwrap_or(&view);
                let action = if let Some((shadow, address, len)) =
                    buffer_copy_with_hint(resolved, made_shadow)
                {
                    StageAction::Buffer {
                        shadow,
                        address,
                        len,
                    }
                } else if let View::Shader(resource, _) | View::Unordered(resource, _, _) = resolved
                {
                    StageAction::Texture {
                        resource: *resource,
                    }
                } else {
                    StageAction::None
                };
                let kept_raw = kept_res
                    .as_ref()
                    .map_or(0, |r| windows::core::Interface::as_raw(r) as usize);
                let ret_kept = if kept_raw != 0 && !already_kept(kept_raw) {
                    kept_res
                } else {
                    None
                };
                return DynamicMade {
                    found: true,
                    kept: ret_kept,
                    action,
                    view_kind,
                    is_texture,
                };
            }
        }
        enum PreStageEpoch {
            Buf(u64),
            Tex(u8, u64),
        }
        let mut pre_placed: Option<View> = None;
        let pre_epoch = match &view {
            View::Shader(resource, _) | View::Unordered(resource, _, _) if is_texture => {
                let addr = crate::resource::texture_memory(*resource as crate::object::This)
                    .map_or(0, |(ptr, _)| ptr as u64);
                let b = tex_epoch_bucket(addr);
                PreStageEpoch::Tex(
                    b as u8,
                    STAGE_TEX_EPOCHS[b].load(std::sync::atomic::Ordering::Acquire),
                )
            }
            View::Placed(..) => {
                if let Some(resolved) = placed_texture(&view) {
                    let ep = if let View::Shader(resource, _) | View::Unordered(resource, _, _) =
                        &resolved
                    {
                        let addr =
                            crate::resource::texture_memory(*resource as crate::object::This)
                                .map_or(0, |(ptr, _)| ptr as u64);
                        let b = tex_epoch_bucket(addr);
                        PreStageEpoch::Tex(
                            b as u8,
                            STAGE_TEX_EPOCHS[b].load(std::sync::atomic::Ordering::Acquire),
                        )
                    } else {
                        let b = tex_epoch_bucket(0);
                        PreStageEpoch::Tex(
                            b as u8,
                            STAGE_TEX_EPOCHS[b].load(std::sync::atomic::Ordering::Acquire),
                        )
                    };
                    pre_placed = Some(resolved);
                    ep
                } else if let Some((address, len)) = buffer_range(&view)
                    && len > 0
                {
                    PreStageEpoch::Buf(crate::shadow::epoch_for_range(address, len))
                } else {
                    let b = tex_epoch_bucket(0);
                    PreStageEpoch::Tex(
                        b as u8,
                        STAGE_TEX_EPOCHS[b].load(std::sync::atomic::Ordering::Acquire),
                    )
                }
            }
            _ => {
                if let Some((address, len)) = buffer_range(&view)
                    && len > 0
                {
                    PreStageEpoch::Buf(crate::shadow::epoch_for_range(address, len))
                } else {
                    let b = tex_epoch_bucket(0);
                    PreStageEpoch::Tex(
                        b as u8,
                        STAGE_TEX_EPOCHS[b].load(std::sync::atomic::Ordering::Acquire),
                    )
                }
            }
        };
        let mut guard = stage.stripes[stripe_idx]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(&local) = guard.map.get(&key)
            && let Some(entry) = guard.entries[local].as_ref()
            && entry.key == key
            && entry.is_valid()
        {
            // SAFETY: dst is our dynamic slot; src is guarded by stripe write lock.
            unsafe {
                host.device.CopyDescriptorsSimple(
                    1,
                    dst,
                    stage.cpu(entry.slot),
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                );
            }
            let kept = if entry.kept_raw != 0 && !already_kept(entry.kept_raw) {
                entry.kept.clone()
            } else {
                None
            };
            return DynamicMade {
                found: true,
                kept,
                action: entry.action.clone(),
                view_kind: entry.view_kind,
                is_texture: entry.is_texture,
            };
        }
        let local = if let Some(&existing) = guard.map.get(&key) {
            existing
        } else {
            let at = guard.cursor;
            guard.cursor = (at + 1) % SLOTS_PER_STRIPE;
            at
        };
        let slot = (stripe_idx * SLOTS_PER_STRIPE + local) as u32;
        let (kept, made_shadow, made_placed) =
            make_detailed_with_shadow(&view, stage.cpu(slot), pre_placed, &mut find_shadow);
        let Some(kept_res) = kept else {
            drop(guard);
            null_for(&view, dst);
            return DynamicMade {
                found: true,
                kept: None,
                action: StageAction::None,
                view_kind,
                is_texture,
            };
        };
        // SAFETY: dst is our dynamic slot; src was just populated in our CPU staging heap.
        unsafe {
            host.device.CopyDescriptorsSimple(
                1,
                dst,
                stage.cpu(slot),
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            );
        }
        let resolved = made_placed.as_ref().unwrap_or(&view);
        let (action, is_buffer, tex_bucket, epoch) = if let Some((shadow, address, len)) =
            buffer_copy_with_hint(resolved, made_shadow)
        {
            let ep = match pre_epoch {
                PreStageEpoch::Buf(ep) => ep,
                PreStageEpoch::Tex(..) => 0,
            };
            (
                StageAction::Buffer {
                    shadow,
                    address,
                    len,
                },
                true,
                0u8,
                ep,
            )
        } else if let View::Shader(resource, _) | View::Unordered(resource, _, _) = resolved {
            let (b, ep) = match pre_epoch {
                PreStageEpoch::Tex(b, ep) => (b, ep),
                PreStageEpoch::Buf(_) => {
                    let addr = crate::resource::texture_memory(*resource as crate::object::This)
                        .map_or(0, |(ptr, _)| ptr as u64);
                    (tex_epoch_bucket(addr) as u8, 0)
                }
            };
            (
                StageAction::Texture {
                    resource: *resource,
                },
                false,
                b,
                ep,
            )
        } else {
            let (b, ep) = match pre_epoch {
                PreStageEpoch::Tex(b, ep) => (b, ep),
                PreStageEpoch::Buf(_) => (tex_epoch_bucket(0) as u8, 0),
            };
            (StageAction::None, false, b, ep)
        };
        if let Some(old) = guard.entries[local].take()
            && old.key != key
        {
            guard.map.remove(&old.key);
        }
        let kept_raw = kept_res
            .as_ref()
            .map_or(0, |r| windows::core::Interface::as_raw(r) as usize);
        guard.map.insert(key, local);
        guard.entries[local] = Some(StageEntry {
            key,
            slot,
            epoch,
            is_buffer,
            is_texture,
            tex_bucket,
            view_kind,
            kept_raw,
            kept: kept_res.clone(),
            action: action.clone(),
        });
        let ret_kept = if kept_raw != 0 && !already_kept(kept_raw) {
            kept_res
        } else {
            None
        };
        DynamicMade {
            found: true,
            kept: ret_kept,
            action,
            view_kind,
            is_texture,
        }
    }

    pub(crate) fn make_null_dynamic(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
    }

    pub(crate) fn constant_at_dynamic(&self, index: u32, gpu: u64, size: u32) {
        let Some(host) = host() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: size.next_multiple_of(256),
        };
        // SAFETY: dynamic slot owned by the caller's command list; backing buffer is kept on the list.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.keep(index, None);
    }

    fn keep(&self, index: u32, resource: Option<ID3D12Resource>) {
        if let Some(old) = self.keep_take(index, resource) {
            crate::host::retire(old);
        }
    }

    fn keep_into(
        &self,
        index: u32,
        resource: Option<ID3D12Resource>,
        retired: Option<&mut Vec<ID3D12Resource>>,
    ) {
        if let Some(old) = self.keep_take(index, resource) {
            match retired {
                Some(sink) => sink.push(old),
                None => crate::host::retire(old),
            }
        }
    }

    fn keep_take(&self, index: u32, resource: Option<ID3D12Resource>) -> Option<ID3D12Resource> {
        let present = resource.is_some();
        if !present && !bit_get(&self.has_kept, index) {
            return None;
        }
        let stripe = index as usize % MADE_LOCK_STRIPES;
        let pos = index as usize / MADE_LOCK_STRIPES;
        let mut unchanged = None;
        let old = {
            let mut kept = self.kept[stripe].lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = kept.get_mut(pos) {
                if let (Some(cur), Some(new)) = (slot.as_ref(), resource.as_ref())
                    && windows::core::Interface::as_raw(cur)
                        == windows::core::Interface::as_raw(new)
                {
                    unchanged = resource;
                    None
                } else {
                    bit_set(&self.has_kept, index, present);
                    std::mem::replace(slot, resource)
                }
            } else {
                unchanged = resource;
                None
            }
        };
        drop(unchanged);
        old
    }

    pub(crate) fn raw_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index))
        };
        self.keep(index, Some(buffer.clone()));
    }

    pub(crate) fn null_raw_at(&self, index: u32) {
        null_buffer_view(self.cpu(index));
        self.keep(index, None);
    }

    pub(crate) fn raw_uav_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device.CreateUnorderedAccessView(
                buffer,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
        self.keep(index, Some(buffer.clone()));
    }

    pub(crate) fn null_raw_uav_at(&self, index: u32) {
        self.keep(index, None);
        let Some(host) = host() else { return };
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 1,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        // SAFETY: a slot of our heap; a null resource is allowed with a desc.
        unsafe {
            host.device.CreateUnorderedAccessView(
                None::<&ID3D12Resource>,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
    }

    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: self.gpu + u64::from(index) * u64::from(self.step),
        }
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    fn unmake_stale(&self, index: u32, stale: Vec<u32>) {
        for slot in stale {
            if slot != index {
                let mut deferred = self
                    .made_lock(slot)
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !bit_get(&self.in_placed, slot) {
                    deferred.remove(&slot);
                    self.unmake(slot);
                }
            }
        }
    }

    fn mirror_inner(
        &self,
        index: u32,
        handle: u64,
        for_draw: bool,
        notify_changed: bool,
        retired: Option<&mut Vec<ID3D12Resource>>,
    ) -> Option<(
        View,
        Option<std::sync::Arc<crate::shadow::Shadow>>,
        Option<View>,
        Option<usize>,
        bool,
    )> {
        if index >= self.count {
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let key = u64::from(generation) << 32 | u64::from(i);
        if tag == VIEW_TAG
            && key != 0
            && self.made_keys[index as usize].load(std::sync::atomic::Ordering::Acquire) == key
        {
            return view_at(i, generation).map(|view| (view, None, None, None, false));
        }
        let mut stale = Vec::new();
        let mut made_shadow = None;
        let mut made_placed = None;
        let mut kept_raw = None;
        let mut freshly_made = false;
        let view = {
            let mut deferred = self
                .made_lock(index)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
            let cur = self.made_keys[index as usize].load(std::sync::atomic::Ordering::Relaxed);
            let Some(view) = view else {
                deferred.remove(&index);
                if cur != 0 {
                    self.made_keys[index as usize].store(0, std::sync::atomic::Ordering::Release);
                    self.make_null(index);
                    let _ = self.note_buffer_inner(index, None, false, notify_changed);
                } else if for_draw {
                    let _ = self.note_buffer_inner(index, None, false, notify_changed);
                }
                return None;
            };
            if cur != key {
                if !for_draw
                    && cur == 0
                    && !is_texture_view(&view)
                    && deferred.get(&index) == Some(&key)
                {
                    return Some((view, None, None, None, false));
                }
                let already_deferred_buffer =
                    for_draw && view.is_buffer() && deferred.remove(&index) == Some(key);
                if !already_deferred_buffer {
                    stale = self.note_buffer_inner(index, Some(&view), for_draw, notify_changed);
                }
                let waiting = if for_draw {
                    None
                } else {
                    view.waiting_texture()
                };
                if let Some(texture) = waiting {
                    super::wait_for(texture, handle);
                }
                let later = !for_draw && (view.is_buffer() || waiting.is_some());
                if later
                    && keep_views()
                    && view_at(cur as u32, (cur >> 32) as u32)
                        .is_some_and(|old| same_placed_texture(&old, &view))
                {
                    deferred.remove(&index);
                    KEPT_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    drop(deferred);
                    self.unmake_stale(index, stale);
                    return Some((view, None, None, None, false));
                }
                if later {
                    self.made_keys[index as usize].store(0, std::sync::atomic::Ordering::Release);
                    if view.is_buffer() {
                        deferred.insert(index, key);
                    } else {
                        deferred.remove(&index);
                    }
                } else {
                    deferred.remove(&index);
                }
                let kept = if later {
                    None
                } else {
                    let (kept, shadow, placed) = make_detailed(&view, self.cpu(index));
                    made_shadow = shadow;
                    made_placed = placed;
                    freshly_made = true;
                    kept
                };
                if kept.is_none() && !self.sampler {
                    null_for(&view, self.cpu(index));
                }
                let kept_res = kept.flatten();
                kept_raw = kept_res
                    .as_ref()
                    .map(|r| windows::core::Interface::as_raw(r) as usize);
                self.keep_into(index, kept_res, retired);
                if !later {
                    self.made_keys[index as usize].store(key, std::sync::atomic::Ordering::Release);
                }
            }
            view
        };
        if !stale.is_empty() {
            self.unmake_stale(index, stale);
        }
        Some((view, made_shadow, made_placed, kept_raw, freshly_made))
    }

    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        self.mirror_inner(index, handle, for_draw, true, None)
            .map(|(view, ..)| view)
    }

    pub(crate) fn made_key_matches_words(&self, index: u32, words: [u32; 3]) -> bool {
        if words[0] != VIEW_TAG || index >= self.count {
            return false;
        }
        let key = (u64::from(words[2]) << 32) | u64::from(words[1]);
        key != 0
            && self.made_keys[index as usize].load(std::sync::atomic::Ordering::Acquire) == key
    }

    pub(crate) fn made_key(&self, index: u32) -> u64 {
        self.made_keys
            .get(index as usize)
            .map_or(0, |k| k.load(std::sync::atomic::Ordering::Acquire))
    }

    pub(crate) fn unchanged_slot_key(&self, index: u32, handle: u64) -> Option<u64> {
        if index >= self.count {
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let key = u64::from(generation) << 32 | u64::from(i);
        if tag == VIEW_TAG
            && key != 0
            && self.made_keys[index as usize].load(std::sync::atomic::Ordering::Acquire) == key
        {
            Some(key)
        } else {
            None
        }
    }

    pub(crate) fn mirror_buffer_into(
        &self,
        index: u32,
        handle: u64,
        retired: &mut Vec<ID3D12Resource>,
    ) -> Option<MirroredBuffer> {
        // Called from `heap_buffers`, which immediately updates its own `BufferSpanCache`
        // for `index`, so pass `notify_changed = false` to avoid locking `self.changed`
        // and self-requeueing `index` for the next frame.
        let (view, made_shadow, _, kept_raw, freshly_made) =
            self.mirror_inner(index, handle, true, false, Some(retired))?;
        let copy = buffer_copy_with_hint(&view, made_shadow);
        #[cfg(feature = "diagnostics")]
        let shows = copy.as_ref().and_then(|(shadow, _, _)| {
            if freshly_made {
                kept_raw.map(|raw| raw == windows::core::Interface::as_raw(&shadow.buffer) as usize)
            } else {
                self.shows(index, &shadow.buffer)
            }
        });
        #[cfg(not(feature = "diagnostics"))]
        let _ = (kept_raw, freshly_made);
        Some(MirroredBuffer {
            copy,
            #[cfg(feature = "diagnostics")]
            shows,
        })
    }

    pub(crate) fn mirror_texture(&self, index: u32, handle: u64) -> Option<(View, Option<View>)> {
        let (view, _, made_placed, _, freshly_made) =
            self.mirror_inner(index, handle, true, true, None)?;
        let placed = if freshly_made {
            made_placed
        } else {
            placed_texture(&view)
        };
        Some((view, placed))
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn shows(&self, index: u32, buffer: &ID3D12Resource) -> Option<bool> {
        let stripe = index as usize % MADE_LOCK_STRIPES;
        let pos = index as usize / MADE_LOCK_STRIPES;
        let kept = self.kept[stripe].lock().unwrap_or_else(|e| e.into_inner());
        let shown = kept.get(pos)?.as_ref()?;
        Some(windows::core::Interface::as_raw(shown) == windows::core::Interface::as_raw(buffer))
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn texture_slot_state(&self, index: u32, frame: u64) -> String {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match textures.slots.get(&index) {
            None => "not queued".into(),
            Some(w) => format!(
                "queued due={} written@{} looks={} part_mapped={} recheck={}",
                if w.due <= frame {
                    "now".to_string()
                } else {
                    format!("@{}", w.due)
                },
                w.written,
                w.looks,
                w.mapped,
                w.checked.2 != 0
            ),
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = self
            .made_keys
            .get(index as usize)?
            .load(std::sync::atomic::Ordering::Acquire);
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

pub(crate) fn buffer_extent(view: &View) -> Option<(u64, u64)> {
    match view {
        View::Shader(r, Some(_)) | View::Unordered(r, _, Some(_)) => {
            let (data, len) = crate::resource::buffer_bytes(*r as crate::object::This)?;
            Some((data as u64, len))
        }
        View::Placed(_, resource, desc) if resource[0] == 1 => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            u64::from(resource[4]) | u64::from(resource[5]) << 32,
        )),
        _ => None,
    }
}

fn view_extent() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_VIEW_EXTENT").is_none_or(|v| v != "0"))
}

pub(crate) fn buffer_copy_with_hint(
    view: &View,
    hint: Option<std::sync::Arc<crate::shadow::Shadow>>,
) -> Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)> {
    let (address, len) = buffer_range(view)?;
    if len == 0 {
        return None;
    }
    if let Some(shadow) = hint {
        return Some((shadow, address, len));
    }
    if view_extent()
        && let Some((base, width)) = buffer_extent(view)
        && address >= base
        && address - base < width.max(1)
    {
        if let Some(shadow) = crate::shadow::find(base, width) {
            return Some((shadow, address, len));
        }
    }
    crate::shadow::find(address, len).map(|shadow| (shadow, address, len))
}

fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

type Kept = Option<ID3D12Resource>;

fn raw_window(offset: u64, first: u64, count: u32, stride: u32) -> Option<(u64, u32)> {
    if stride == 0 {
        return None;
    }
    let start = first.checked_mul(u64::from(stride))?.checked_add(offset)?;
    let bytes = u64::from(count) * u64::from(stride);
    (start % 16 == 0 && bytes % 4 == 0)
        .then(|| {
            u32::try_from(bytes / 4)
                .ok()
                .map(|dwords| (start / 4, dwords))
        })
        .flatten()
}

fn buffer_view_ok(
    format: DXGI_FORMAT,
    first: u64,
    count: u32,
    stride: u32,
    raw: bool,
    width: u64,
) -> bool {
    use crate::resource::family;
    let f = format.0 as u32;
    let element = if raw {
        if format != DXGI_FORMAT_R32_TYPELESS || stride != 0 || !first.is_multiple_of(4) {
            return false;
        }
        4
    } else if stride > 0 {
        if format != DXGI_FORMAT_UNKNOWN || stride > 2048 || !stride.is_multiple_of(4) {
            return false;
        }
        u64::from(stride)
    } else {
        let typeless = family(f) == f && family(f + 1) == f;
        match crate::resource::element(f) {
            Some((bytes, 1)) if f != 0 && !typeless && bytes > 0 => bytes,
            _ => return false,
        }
    };
    count > 0
        && first
            .checked_add(u64::from(count))
            .and_then(|end| end.checked_mul(element))
            .is_some_and(|end| end <= width)
}

fn buffer_srv_with_shadow(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    find_shadow: &mut dyn FnMut(u64, u64) -> Option<std::sync::Arc<crate::shadow::Shadow>>,
) -> Option<(Kept, std::sync::Arc<crate::shadow::Shadow>)> {
    let shadow = find_shadow(address, len)?;
    let offset = shadow.offset(address);
    // SAFETY: a buffer view's union member.
    let structured = unsafe { desc.Anonymous.Buffer };
    let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
        .then(|| {
            raw_window(
                offset,
                structured.FirstElement,
                structured.NumElements,
                structured.StructureByteStride,
            )
        })
        .flatten();
    if let Some((first, count)) = window {
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
        desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
            FirstElement: first,
            NumElements: count,
            StructureByteStride: 0,
            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
        };
    }
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    if window.is_none() {
        if element == 0 || offset % element != 0 {
            once!(
                offset,
                element,
                "buffer view not on an element of its host copy"
            );
            return None;
        }
        buffer.FirstElement += offset / element;
    }
    let (first, count, stride) = (
        buffer.FirstElement,
        buffer.NumElements,
        buffer.StructureByteStride,
    );
    let raw = buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
    let width = shadow.host_width();
    if !buffer_view_ok(desc.Format, first, count, stride, raw, width) {
        REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        once!(
            format = desc.Format.0,
            first,
            count,
            stride,
            raw,
            width,
            "buffer view the host would refuse left null"
        );
        return None;
    }
    if desc.Shader4ComponentMapping != D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING {
        desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    let kept = Some(shadow.buffer.clone());
    Some((kept, shadow))
}

fn buffer_uav_with_shadow(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    find_shadow: &mut dyn FnMut(u64, u64) -> Option<std::sync::Arc<crate::shadow::Shadow>>,
) -> Option<(Kept, std::sync::Arc<crate::shadow::Shadow>)> {
    let shadow = find_shadow(address, len)?;
    if !shadow.allow_uav() {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let offset = shadow.offset(address);
    let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
        .then(|| {
            raw_window(
                offset,
                buffer.FirstElement,
                buffer.NumElements,
                buffer.StructureByteStride,
            )
        })
        .flatten();
    if let Some((first, count)) = window {
        buffer.FirstElement = first;
        buffer.NumElements = count;
        buffer.StructureByteStride = 0;
        buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    } else {
        if element == 0 || offset % element != 0 {
            once!(
                offset,
                element,
                "unordered buffer view not on an element of its host copy"
            );
            return None;
        }
        buffer.FirstElement += offset / element;
    }
    buffer.CounterOffsetInBytes = 0;
    let (first, count, stride) = (
        buffer.FirstElement,
        buffer.NumElements,
        buffer.StructureByteStride,
    );
    let raw = buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
    if window.is_some() {
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
    }
    let width = shadow.host_width();
    if !buffer_view_ok(desc.Format, first, count, stride, raw, width) {
        REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        once!(
            format = desc.Format.0,
            first,
            count,
            stride,
            raw,
            width,
            "unordered buffer view the host would refuse left null"
        );
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device.CreateUnorderedAccessView(
            &shadow.buffer,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
    let kept = Some(shadow.buffer.clone());
    Some((kept, shadow))
}

fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        if unordered {
            crate::resource::DEPTH_UNORDERED[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    if view != own && crate::resource::target_format(view) == own {
        if let Some(format) = format {
            *format = d.Format;
        }
        return true;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

fn twin_uav(
    twin: &ID3D12Resource,
    desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
) -> Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> {
    // SAFETY: a live resource.
    let d = unsafe { twin.GetDesc() };
    let mut desc = desc.cloned().unwrap_or_else(|| {
        let mut all = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        if d.DepthOrArraySize > 1 {
            all.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            all.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: u32::from(d.DepthOrArraySize),
                ..Default::default()
            };
        }
        all
    });
    if d.Format.0 == 60 {
        let mut plane = 0;
        match desc.ViewDimension {
            D3D12_UAV_DIMENSION_TEXTURE2D => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2D };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2D = view;
            }
            D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2DArray };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2DArray = view;
            }
            _ => {}
        }
        if plane != 1 && !matches!(desc.Format.0, 22 | 47) {
            return None;
        }
        desc.Format.0 = 62;
        return Some(desc);
    }
    desc.Format.0 = match (desc.Format.0, d.Format.0) {
        (0 | 53 | 55 | 19..=21 | 44..=46, 53) => 56,
        (0 | 39 | 40 | 19..=21 | 44..=46, 39) => 41,
        (other, _) => other,
    };
    Some(desc)
}

fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    make_detailed(view, at).0
}

fn make_detailed(
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> (
    Option<Kept>,
    Option<std::sync::Arc<crate::shadow::Shadow>>,
    Option<View>,
) {
    make_detailed_with_shadow(view, at, None, &mut crate::shadow::find)
}

fn make_detailed_with_shadow(
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    pre_placed: Option<View>,
    find_shadow: &mut dyn FnMut(u64, u64) -> Option<std::sync::Arc<crate::shadow::Shadow>>,
) -> (
    Option<Kept>,
    Option<std::sync::Arc<crate::shadow::Shadow>>,
    Option<View>,
) {
    let Some(host) = host() else {
        return (None, None, None);
    };
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            let aligned = size.next_multiple_of(256);
            let Some(shadow) = find_shadow(address, u64::from(size)) else {
                return (None, None, None);
            };
            let gpu = shadow.gpu(address);
            if !shadow.can_bind_cbv(address, size) {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return (None, None, None);
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: aligned,
            };
            // SAFETY: a slot of our heap; the range lies in the host copy.
            unsafe { device.CreateConstantBufferView(Some(&desc), at) };
            let kept = Some(shadow.buffer.clone());
            (Some(kept), Some(shadow), None)
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(texture) = crate::resource::host_resource(resource) {
                #[cfg(feature = "diagnostics")]
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() }.Format.0;
                    if matches!(own, 19..=22 | 44..=47) {
                        static SEEN: Mutex<Vec<(i32, Option<[u32; 10]>)>> = Mutex::new(Vec::new());
                        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                        if seen.len() < 64 && !seen.contains(&(own, *desc)) {
                            seen.push((own, *desc));
                            tracing::info!(
                                ?resource,
                                texture_format = own,
                                view = ?desc,
                                "shader view of a depth-stencil texture"
                            );
                        }
                    }
                }
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return (None, None, None);
                }
                if let Some(first) = crate::resource::first_shown(resource)
                    && first > 0
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() };
                    let whole = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: own.Format,
                        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                        Shader4ComponentMapping: 5768,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D12_TEX2D_SRV {
                                MostDetailedMip: 0,
                                MipLevels: u32::MAX,
                                PlaneSlice: 0,
                                ResourceMinLODClamp: 0.0,
                            },
                        },
                    };
                    crate::tiles::show_from(
                        desc.get_or_insert(whole),
                        first,
                        u32::from(own.MipLevels),
                    );
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                (Some(Some(texture.clone())), None, None)
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                match buffer_srv_with_shadow(data as u64, len, srv_desc(desc), at, find_shadow) {
                    Some((kept, shadow)) => (Some(kept), Some(shadow), None),
                    None => (None, None, None),
                }
            } else {
                (None, None, None)
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        match buffer_uav_with_shadow(
                            data as u64,
                            len,
                            uav_desc(desc),
                            at,
                            find_shadow,
                        ) {
                            Some((kept, shadow)) => (Some(kept), Some(shadow), None),
                            None => (None, None, None),
                        }
                    }
                    _ => (None, None, None),
                };
            };
            if crate::resource::depth_and_unordered(resource) {
                static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 512 {
                    tracing::info!(
                        ?resource,
                        view = ?desc,
                        "unordered access view of a depth texture"
                    );
                }
            }
            let mut desc = desc.as_ref().map(uav_desc);
            let twin = crate::resource::twin_resource(resource)
                .and_then(|twin| Some((twin, twin_uav(twin, desc.as_ref())?)))
                .or_else(|| {
                    let twin = crate::resource::twin_plane_resource(resource)?;
                    Some((twin, twin_uav(twin, desc.as_ref())?))
                });
            let texture = match twin {
                Some((twin, view)) => {
                    desc = Some(view);
                    twin
                }
                None => texture,
            };
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return (None, None, None);
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            (Some(Some(texture.clone())), None, None)
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            (Some(None), None, None)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            (Some(None), None, None)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(resolved) = pre_placed.or_else(|| placed_texture(view)) {
                let (kept, shadow, _) =
                    make_detailed_with_shadow(&resolved, at, None, find_shadow);
                return (kept, shadow, Some(resolved));
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                once!(
                    dimension = resource[0],
                    "placed unordered texture view not mirrored"
                );
                return (None, None, None);
            }
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            if len == 0 {
                return (None, None, None);
            }
            let made = if *unordered {
                buffer_uav_with_shadow(address, len, uav_desc(words), at, find_shadow)
            } else {
                buffer_srv_with_shadow(address, len, srv_desc(words), at, find_shadow)
            };
            match made {
                Some((kept, shadow)) => (Some(kept), Some(shadow), None),
                None => (None, None, None),
            }
        }
    }
}

pub(crate) static KEPT_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn keep_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_KEEP_VIEWS").is_none_or(|v| v != "0"))
}

fn same_placed_texture(a: &View, b: &View) -> bool {
    match (a, b) {
        (View::Placed(false, resource, desc), View::Placed(false, other, other_desc)) => {
            resource[0] != 1 && resource == other && desc[10..12] == other_desc[10..12]
        }
        _ => false,
    }
}

pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let texture = crate::resource::texture_at(address, resource)?;
    Some(View::Shader(texture as usize, desc[..10].try_into().ok()))
}

fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        View::Constant(..) => {
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
            // SAFETY: a slot of our heap; a zero location makes a null view.
            return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
        }
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    #[test]
    fn spans_say_the_next_start_and_the_lowest_level_viewed() {
        let mut spans = Spans::default();
        spans.note(1, 0x1_0000, 0x1_0000 + 0x5_6000, true);
        spans.shows(1, 3);
        spans.note(2, 0x1_0000, 0x1_0000 + 0x5_6000, true);
        spans.shows(2, 1);
        spans.note(3, 0x1_6000, 0x1_6000 + 0x5_6000, true);
        assert_eq!(
            spans.next_start(0x1_0000, 0x1_0000 + 0x5_6000),
            Some(0x1_6000)
        );
        assert_eq!(spans.next_start(0x1_6000, 0x1_6000 + 0x5_6000), None);
        assert_eq!(spans.first_viewed(0x1_0000), Some(1));
        assert_eq!(spans.first_viewed(0x1_6000), None);
        assert_eq!(spans.slots_at(0x1_0000), vec![1, 2]);
        spans.forget(2);
        assert_eq!(spans.first_viewed(0x1_0000), Some(3));
    }

    #[test]
    fn views_of_one_placed_texture_are_told_by_address_and_description() {
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], first_mip: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[4], desc[5], desc[10], desc[11]) =
                (98, 4, first_mip, 8 - first_mip, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let low = view(texture, 3, 0x1000_0000);
        assert!(same_placed_texture(&low, &view(texture, 0, 0x1000_0000)));
        assert!(!same_placed_texture(&low, &view(texture, 3, 0x2000_0000)));
        let mut wider = texture;
        wider[4] = 1024;
        assert!(!same_placed_texture(&low, &view(wider, 3, 0x1000_0000)));
        let mut buffer = texture;
        buffer[0] = 1;
        assert!(!same_placed_texture(
            &view(buffer, 3, 0x1000_0000),
            &view(buffer, 3, 0x1000_0000)
        ));
        let View::Placed(_, resource, desc) = low.clone() else {
            unreachable!()
        };
        assert!(!same_placed_texture(
            &low,
            &View::Placed(true, resource, desc)
        ));
        assert!(!same_placed_texture(&low, &View::Shader(0, None)));
    }

    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return;
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    #[test]
    fn a_structured_view_made_raw_passes_the_host_check() {
        let (first, count) = raw_window(32, 8, 100, 20).unwrap();
        assert_eq!((first, count), (48, 500));
        assert!(buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            first,
            count,
            0,
            true,
            4096
        ));
        assert!(!buffer_view_ok(
            DXGI_FORMAT_UNKNOWN,
            first,
            count,
            0,
            true,
            4096
        ));
        assert!(!buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            first,
            count,
            0,
            true,
            2000
        ));
        assert!(raw_window(4, 0, 100, 20).is_none());
        assert!(!buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            0,
            0,
            0,
            true,
            4096
        ));
        assert!(buffer_view_ok(DXGI_FORMAT_UNKNOWN, 1, 10, 12, false, 4096));
    }

    #[test]
    fn the_texture_queue_serves_the_newest_writes_first() {
        let mut q = TextureQueue::default();
        for i in 0..8u32 {
            q.written(10 + i, u64::from(i) + 1, 0, false);
        }
        q.written(5, 9, 0, false);
        let order: Vec<u32> = q.due(9, 100).into_iter().map(|d| d.0).collect();
        assert_eq!(order, [5, 10, 17, 11, 16, 12, 15, 13, 14]);
        assert_eq!(q.due(9, 2).len(), 2);
        assert_eq!(q.census(9), [9, 9, 0, 0, 8]);
        let write = |q: &TextureQueue, index: u32| {
            q.due(u64::MAX, 100)
                .iter()
                .find(|d| d.0 == index)
                .unwrap()
                .1
        };
        let w17 = write(&q, 17);
        let mut frame = 9;
        for wait in [1, 2, 4, 4] {
            q.later(17, w17, frame, 4);
            assert!(!q.due(frame + wait - 1, 100).iter().any(|d| d.0 == 17));
            frame += wait;
            assert_eq!(q.due(frame, 100)[0].0, 17);
        }
        assert_eq!(q.census(9)[2], 1);
        q.done(17, w17);
        assert!(!q.due(u64::MAX, 100).iter().any(|d| d.0 == 17));
        let w16 = write(&q, 16);
        q.partly(16, w16, 12, 4096);
        assert_eq!(q.mapped(16), 4096);
        assert_eq!(q.due(12, 100)[0].0, 16);
        q.written(16, 13, 0, false);
        q.done(16, w16);
        q.later(16, w16, 13, 4);
        assert_eq!(q.mapped(16), 0);
        assert_eq!(q.due(13, 100)[0].0, 16);
        let w15 = write(&q, 15);
        q.partly(15, w15, 20, 8192);
        q.written(15, 14, 0, true);
        assert_eq!(q.mapped(15), 8192);
        q.written(99, 14, 0, true);
        assert!(q.due(14, 100).iter().any(|d| d.0 == 99));
        q.written(98, 14, 16, false);
        assert!(!q.due(15, 100).iter().any(|d| d.0 == 98));
        assert!(q.due(16, 100).iter().any(|d| d.0 == 98));
    }

    #[test]
    fn slots_held_for_lack_of_room_come_back_when_there_is_room() {
        let mut q = TextureQueue::default();
        for i in 0..4u32 {
            q.written(20 + i, 1, 0, false);
        }
        let due = q.due(1, 100);
        assert_eq!(due.len(), 4);
        for &(index, write) in due.iter().filter(|d| d.0 < 22) {
            q.hold(index, write, 101);
        }
        let now: Vec<u32> = q.due(2, 100).into_iter().map(|d| d.0).collect();
        assert!(!now.contains(&20) && !now.contains(&21));
        assert!(now.contains(&22) && now.contains(&23));
        assert_eq!(q.census(2)[2], 0, "held slots do not wait for their memory");
        assert_eq!(q.due(101, 100), due);
        q.written(21, 3, 0, false);
        assert!(q.due(3, 100).iter().any(|d| d.0 == 21));
        assert_eq!(q.room(5), 1);
        assert!(q.due(5, 100).iter().any(|d| d.0 == 20));
        assert_eq!(q.room(6), 0);
    }

    #[test]
    fn packed_neighbours_stay() {
        let mut spans = Spans::default();
        assert_eq!(spans.rule, Rule::Start);
        let at = 0xb_86bf_8000u64;
        let (a, b, c) = (at, at + 0x2000, at + 0x6000);
        assert!(spans.note(105_998, a, a + 0x2c000, true).is_empty());
        assert!(spans.note(106_004, b, b + 0x2c000, true).is_empty());
        assert!(spans.note(108_521, c, c + 0x2c000, true).is_empty());
        assert!(spans.note(7, a, a + 0x2c000, true).is_empty());
        assert!(spans.note(106_004, b, b + 0x2c000, true).is_empty());
        let mut all = spans.slots_in(0, u64::MAX);
        all.sort_unstable();
        assert_eq!(all, [7, 105_998, 106_004, 108_521]);
        assert!(spans.note(9, b, b + 0x2c000, false).is_empty());
        assert_eq!(spans.by_slot.get(&9), Some(&b));
    }

    #[test]
    fn a_texture_written_at_the_start_of_another_replaces_it() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        assert_eq!(spans.note(3, at, at + 0xb000, true), [1]);
        assert_eq!(spans.note(4, at + 0x800, at + 0x1800, true), [3]);
        assert_eq!(spans.note(5, at + 0x1800, at + 0x4800, true), [2]);
        let mut left = spans.slots_in(0, u64::MAX);
        left.sort_unstable();
        assert_eq!(left, [4, 5]);
        assert!(spans.note(6, at + 0x2800, at + 0x3800, true).is_empty());
        assert!(spans.note(8, at + 0x900, at + 0x2900, false).is_empty());
        assert_eq!(spans.by_slot.get(&8), None);
        assert_eq!(spans.by_slot.len(), 3);
    }

    #[test]
    fn a_slot_rewritten_moves_to_its_new_span() {
        let mut spans = Spans::default();
        assert!(spans.note(1, 0x10000, 0x3c000, true).is_empty());
        assert!(spans.note(2, 0x12000, 0x3e000, true).is_empty());
        assert!(spans.note(1, 0x10000, 0x3c000, true).is_empty());
        assert_eq!(spans.slots_in(0x10000, 0x10001), [1]);
        assert!(spans.note(1, 0x90000, 0x91000, true).is_empty());
        assert_eq!(spans.slots_in(0x10000, 0x10001), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x12000, 0x12001), [2]);
        assert_eq!(spans.slots_in(0x90000, 0x90001), [1]);
        spans.forget(1);
        spans.forget(2);
        assert!(spans.by_start.is_empty() && spans.by_slot.is_empty());
    }

    #[test]
    fn memory_changed_under_packed_neighbours_names_the_ones_there() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        assert!(spans.note(3, at + 0x6000, at + 0x32000, true).is_empty());
        assert_eq!(spans.slots_in(at + 0x1000, at + 0x2000), [1]);
        assert_eq!(spans.slots_in(at + 0x3000, at + 0x4000), [2]);
        assert_eq!(spans.slots_in(at + 0x20000, at + 0x21000), [3]);
        assert_eq!(spans.slots_in(at, at + 0x8000), [1, 2, 3]);
        assert_eq!(spans.slots_in(at + 0x32000, at + 0x33000), [] as [u32; 0]);
    }

    #[test]
    fn a_write_at_the_start_of_a_packed_texture_names_it_alone() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        assert!(spans.note(3, at + 0x6000, at + 0x32000, true).is_empty());
        assert_eq!(spans.slots_in(at + 0x2000, at + 0x6000), [1, 2]);
        assert_eq!(spans.slots_holding(at + 0x2000, at + 0x6000), [2]);
        assert_eq!(spans.slots_holding(at + 0x1000, at + 0x3000), [1, 2]);
        assert_eq!(spans.slots_holding(at, at + 0x10000), [1, 2, 3]);
        assert_eq!(spans.slots_holding(at + 0x20000, at + 0x21000), [3]);
        assert_eq!(
            spans.slots_holding(at + 0x32000, at + 0x33000),
            [] as [u32; 0]
        );
        assert_eq!(
            spans.slots_holding(at + 0x2000, at + 0x2000),
            [] as [u32; 0]
        );
    }

    #[test]
    fn a_page_write_makes_an_uploaded_texture_slot_due_when_asked() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return;
        };
        let slots = |at: u64| -> Vec<u32> {
            heap.texture_slots_due(at, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        let at = 0x7fed_0000_0000u64;
        let mut desc = [0u32; 16];
        (desc[10], desc[11]) = (at as u32, (at >> 32) as u32);
        let view = View::Placed(
            false,
            [3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0],
            desc,
        );
        assert!(heap.note_buffer(1, Some(&view), false).is_empty());
        let written = [(at, at + 0x1000)];
        assert_eq!(heap.slots_written_under(&written, 50, true), [1, 0, 0]);
        let (queued, write) = heap.texture_slots_due(u64::MAX, 8).0[0];
        assert_eq!(queued, 1);
        heap.texture_slot_done(1, write, None);
        assert_eq!(slots(u64::MAX), []);
        assert_eq!(
            heap.slots_written_under(&[(at + 0x10_0000, at + 0x10_1000)], 60, true),
            [0, 0, 0]
        );
        assert_eq!(heap.slots_written_under(&written, 60, false), [1, 1, 0]);
        assert_eq!(slots(u64::MAX), []);
        assert_eq!(heap.slots_written_under(&written, 60, true), [1, 1, 1]);
        assert_eq!(slots(61), []);
        assert_eq!(slots(62), [1]);
    }

    #[test]
    fn never_stale_leaves_overlapping_views_out() {
        let mut spans = Spans {
            rule: Rule::Never,
            ..Default::default()
        };
        assert!(spans.note(1, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(2, 0x2000, 0x4000, true).is_empty());
        assert!(spans.note(3, 0x1000, 0x2000, true).is_empty());
        assert_eq!(spans.slots_in(0, u64::MAX), [1]);
    }

    #[test]
    fn a_newer_view_of_other_memory_makes_the_older_stale() {
        let mut spans = Spans {
            rule: Rule::Overlap,
            ..Default::default()
        };
        assert!(spans.note(1, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(2, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(3, 0x3000, 0x4000, true).is_empty());
        assert!(spans.note(4, 0x8000, 0x9000, true).is_empty());
        assert!(spans.note(2, 0x1000, 0x3000, true).is_empty());
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        assert_eq!(spans.slots_in(0x4000, 0x8000), [] as [u32; 0]);
        assert!(spans.note(9, 0x2000, 0x3800, false).is_empty());
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        let mut stale = spans.note(5, 0x2000, 0x4000, true);
        stale.sort_unstable();
        assert_eq!(stale, [1, 2, 3]);
        assert_eq!(spans.slots_in(0, u64::MAX), [5, 4]);
        assert_eq!(spans.note(6, 0x2800, 0x3000, true), [5]);
        assert_eq!(spans.note(7, 0x2800, 0x2c00, true), [6]);
        assert!(spans.note(7, 0x8000, 0x9000, true).is_empty());
        assert_eq!(spans.slots_in(0x2800, 0x2c00), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x8000, 0x8001), [4, 7]);
        spans.forget(4);
        spans.forget(7);
        assert!(spans.by_start.is_empty() && spans.by_slot.is_empty());
    }

    #[test]
    fn remapped_memory_makes_its_texture_slots_due() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return;
        };
        let slots = |at: u64| -> Vec<u32> {
            heap.texture_slots_due(at, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        let at = 0x7fee_0000_0000u64;
        let view = |address: u64, width: u32| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            View::Placed(
                false,
                [3, 0, 0, 0, width, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0],
                desc,
            )
        };
        let len = crate::resource::placed_bytes(&[
            3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0,
        ])
        .unwrap();
        assert!(heap.note_buffer(1, Some(&view(at, 64)), false).is_empty());
        let (queued, write) = heap.texture_slots_due(u64::MAX, 8).0[0];
        assert_eq!(queued, 1);
        heap.texture_slot_done(1, write, None);
        assert_eq!(slots(u64::MAX), []);
        remapped((at + 2 * len) as usize, 0x1000);
        remapped(at as usize, 0x1000);
        assert_eq!(heap.texture_slots_remapped(50), 1);
        assert_eq!(slots(50), []);
        assert_eq!(slots(51), [1]);
        assert_eq!(heap.note_buffer(2, Some(&view(at, 128)), false), [1]);
        assert_eq!(slots(u64::MAX), [2]);
    }

    #[test]
    fn slots_read_through_tables_leave_the_texture_queue() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return;
        };
        let frame = 100;
        let slots = |at: u64| -> Vec<u32> {
            heap.texture_slots_due(at, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        let texture = View::Placed(
            false,
            [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 28, 1, 0, 0x105, 0, 0],
            [0; 16],
        );
        assert!(is_texture_view(&texture));
        heap.queue_texture(2, true, false, frame);
        heap.queue_texture(3, true, false, frame);
        assert_eq!(slots(frame), [3, 2]);
        heap.tabled_in(3, true, frame);
        assert_eq!(slots(frame), [2]);
        heap.queue_texture(3, true, true, frame);
        assert_eq!(slots(frame), [2]);
        heap.queue_texture(3, true, false, frame + 1);
        assert_eq!(slots(frame + TABLED_GRACE), [2]);
        assert_eq!(slots(frame + 1 + TABLED_GRACE), [3, 2]);
        heap.tabled_in(3, true, frame + 1);
        assert_eq!(slots(u64::MAX), [2]);
        heap.tabled_in(2, false, frame + 1);
        assert_eq!(slots(frame + 1), [2]);
        heap.queue_texture(2, true, false, frame + 2);
        assert_eq!(slots(frame + 2), []);
        heap.queue_texture(2, true, false, frame + 3 + TABLED_FRAMES);
        assert_eq!(slots(frame + 3 + TABLED_FRAMES), [2]);
    }

    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    #[test]
    fn texture_queue_indexed_due_matches_priority_and_handles_future_slots() {
        let mut q = TextureQueue::default();
        // Queue 200 future-due slots plus 6 immediately-due slots.
        for i in 0..200u32 {
            q.written(1000 + i, 1, 500 + u64::from(i), false);
        }
        for i in 0..6u32 {
            q.written(10 + i, 1 + u64::from(i), 0, false);
        }
        q.promote(10);
        assert_eq!(q.census(10), [206, 6, 0, 0, 9]);
        // Fresh order alternates newest (15) and oldest (10).
        let due_slots: Vec<u32> = q.due(10, 10).into_iter().map(|d| d.0).collect();
        assert_eq!(due_slots, [15, 10, 14, 11, 13, 12]);
        assert_eq!(q.due(10, 0), []);
        assert_eq!(q.due(10, 3).into_iter().map(|d| d.0).collect::<Vec<_>>(), [15, 10, 14]);

        // Move slot 12 to `again` via `later` (due at 10 + 1 = 11) and slot 11 via `partly` (due at 11).
        let due_map: std::collections::HashMap<u32, u64> = q.due(10, 10).into_iter().collect();
        q.later(12, due_map[&12], 10, 16);
        q.partly(11, due_map[&11], 11, 4096);
        assert_eq!(q.census(10), [206, 4, 1, 1, 9]);

        // Stale write tokens are ignored across done/later/hold/partly/recheck.
        let stale = due_map[&12] + 9999;
        q.done(12, stale);
        q.later(12, stale, 10, 16);
        q.hold(12, stale, 999);
        q.partly(12, stale, 999, 8192);
        q.recheck(12, stale, 999, (0, 0), 1, 0, 10);
        assert_eq!(q.census(10)[0], 206);

        // At frame 11, both `again` slots become due and precede all `fresh` slots, newest write first.
        q.promote(11);
        let at11: Vec<u32> = q.due(11, 10).into_iter().map(|d| d.0).collect();
        assert_eq!(at11[..2], [12, 11]);
        assert_eq!(at11[2..], [15, 10, 14, 13]);
        assert_eq!(q.census(11), [206, 6, 1, 1, 10]);

        // `remove` and `done` clean up both ready and future slots.
        assert!(q.remove(1000));
        assert!(!q.remove(1000));
        q.done(12, due_map[&12]);
        assert_eq!(q.census(11), [204, 5, 0, 1, 10]);

        // `hold` pushes a ready slot into future; `room` brings it back.
        q.hold(15, due_map[&15], 200);
        assert!(!q.due(11, 10).iter().any(|d| d.0 == 15));
        assert_eq!(q.room(11), 1);
        assert!(q.due(11, 10).iter().any(|d| d.0 == 15));

        // Backward frame query after promotion remains exact.
        q.written(50, 11, 11, false);
        assert!(!q.due(10, 100).iter().any(|d| d.0 == 50));
        assert!(q.due(11, 100).iter().any(|d| d.0 == 50));

        // Promoting to frame 502 brings only 1001 and 1002 into due (1000 was removed).
        q.promote(502);
        let at502: std::collections::HashSet<u32> =
            q.all_due(502).into_iter().map(|(idx, _)| idx).collect();
        assert!(at502.contains(&1001));
        assert!(at502.contains(&1002));
        assert!(!at502.contains(&1003));
        assert!(!at502.contains(&1000));
    }

    #[test]
    fn coalesce_u64_ranges_merges_overlapping_and_adjacent() {
        let mut ranges = vec![
            (0x4000, 0x5000),
            (0x1000, 0x2000),
            (0x2000, 0x3000),
            (0x1500, 0x2500),
            (0x9000, 0x9000),
            (0x4800, 0x6000),
        ];
        coalesce_u64_ranges(&mut ranges);
        assert_eq!(ranges, [(0x1000, 0x3000), (0x4000, 0x6000)]);
    }

    #[test]
    fn texture_queue_batched_due_returns_metadata_and_drains_incrementally() {
        let mut q = TextureQueue::default();
        for i in 0..10u32 {
            q.written(100 + i, 1, 0, false);
        }
        q.promote(5);
        let initial = q.due(5, 10);
        let (partly_idx, partly_w) = initial[0];
        let (recheck_idx, recheck_w) = initial[1];
        q.partly(partly_idx, partly_w, 5, 8192);
        q.recheck(recheck_idx, recheck_w, 5, (0x1000, 4096), 0xdeadbeef, 2, 1);

        let mut batch = Vec::new();
        let mut seen = Vec::new();
        loop {
            q.due_batch_into(5, 3, &mut batch);
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= 3);
            for slot in &batch {
                if slot.index == partly_idx {
                    assert_eq!(slot.mapped, 8192);
                    assert_eq!(slot.checked, None);
                } else if slot.index == recheck_idx {
                    assert_eq!(slot.mapped, 0);
                    assert_eq!(slot.checked, Some((0x1000, 4096, 0xdeadbeef, 2, 1)));
                } else {
                    assert_eq!(slot.mapped, 0);
                    assert_eq!(slot.checked, None);
                }
                seen.push(slot.index);
                assert!(q.done(slot.index, slot.write));
            }
        }
        assert_eq!(seen.len(), 10);
        // `again` slots (`recheck_idx`, `partly_idx`) are served ahead of fresh slots.
        assert!(seen[..2].contains(&partly_idx));
        assert!(seen[..2].contains(&recheck_idx));
        assert_eq!(q.census(5)[1], 0);
    }

    #[test]
    fn dynamic_slots_bitset_handles_word_boundaries_and_wraparound() {
        let mut d = DynamicSlots::new(130);
        assert_eq!(d.alloc(0), None);
        assert_eq!(d.alloc(131), None);
        // Allocate across 64-bit word boundaries.
        assert_eq!(d.alloc(60), Some(0));
        assert_eq!(d.alloc(10), Some(60));
        assert_eq!(d.alloc(60), Some(70));
        assert_eq!(d.alloc(1), None);
        // Free a span straddling word 0/1 boundary and a small span at 0.
        assert_eq!(d.free(58, 14), 14);
        assert_eq!(d.free(58, 14), 0);
        assert_eq!(d.alloc(14), Some(58));
        // Free a run that straddles cursor (cursor is now 72).
        assert_eq!(d.free(70, 6), 6);
        assert_eq!(d.alloc(6), Some(70));
        // Free single slot at the very end (index 129, non-multiple-of-64 tail).
        assert_eq!(d.free(129, 10), 1);
        assert_eq!(d.alloc(1), Some(129));
        assert_eq!(d.alloc(1), None);
    }

    #[test]
    fn host_heap_batched_texture_updates_apply_and_step_atomically() {
        let Some(heap) = HostHeap::new(false, 16) else {
            return;
        };
        for i in 0..6u32 {
            heap.queue_texture(i, true, false, 10);
        }
        let mut batch = Vec::new();
        let census = heap.texture_slots_due_batch(10, 4, true, &mut batch).unwrap();
        assert_eq!(census[0], 6);
        assert_eq!(batch.len(), 4);

        let mut updates = vec![
            TextureSlotUpdate::Done {
                index: batch[0].index,
                write: batch[0].write,
                again: None,
            },
            TextureSlotUpdate::Later {
                index: batch[1].index,
                write: batch[1].write,
                frame: 10,
                most: 16,
            },
            TextureSlotUpdate::Held {
                index: batch[2].index,
                write: batch[2].write,
                due: 50,
            },
            TextureSlotUpdate::Recheck {
                index: batch[3].index,
                write: batch[3].write,
                due: 14,
                at: (0x1000, 4096),
                print: 0x1234,
                looks: 0,
                uploaded: 10,
            },
        ];
        let done_idx = batch[0].index;
        heap.texture_batch_step(10, 4, &mut updates, &mut batch);
        assert!(updates.is_empty());
        assert!(!bit_get(&heap.in_texture_queue, done_idx));
        // Only the remaining 2 untouched slots are still due at frame 10.
        assert_eq!(batch.len(), 2);
        for slot in &batch {
            updates.push(TextureSlotUpdate::Done {
                index: slot.index,
                write: slot.write,
                again: None,
            });
        }
        heap.texture_apply_updates(&mut updates);
        assert!(updates.is_empty());
        assert_eq!(heap.texture_slots_due(10, 16).0.len(), 0);
        // At frame 11, the `Later` slot comes back due.
        assert_eq!(heap.texture_slots_due(11, 16).0.len(), 1);
        // At frame 14, the `Recheck` slot comes back due as well.
        assert_eq!(heap.texture_slots_due(14, 16).0.len(), 2);
        // Calling room brings the `Held` slot back due immediately.
        assert_eq!(heap.texture_slots_room(14), 1);
        assert_eq!(heap.texture_slots_due(14, 16).0.len(), 3);
    }

    #[test]
    fn host_heap_sharded_kept_and_note_buffer_skip_self_requeue() {
        let Some(heap) = HostHeap::new(false, 136) else {
            return;
        };
        let Some(host) = host() else {
            return;
        };
        let Some((buf_a, _)) = host.shared_buffer(256) else {
            return;
        };
        let Some((buf_b, _)) = host.shared_buffer(256) else {
            return;
        };
        // Store resources across multiple 64-slot stripes (0, 64, 128, and dynamic slots).
        let dyn_start = heap.count;
        heap.keep(0, Some(buf_a.clone()));
        heap.keep(64, Some(buf_b.clone()));
        heap.keep(128, Some(buf_a.clone()));
        heap.keep(dyn_start + 1, Some(buf_a.clone()));
        heap.keep(dyn_start + 65, Some(buf_b.clone()));
        #[cfg(feature = "diagnostics")]
        {
            assert_eq!(heap.shows(0, &buf_a), Some(true));
            assert_eq!(heap.shows(64, &buf_b), Some(true));
            assert_eq!(heap.shows(128, &buf_a), Some(true));
            assert_eq!(heap.shows(dyn_start + 65, &buf_b), Some(true));
        }
        // Releasing a dynamic range spanning > 64 slots clears all stripes cleanly.
        heap.release(dyn_start, 70);
        assert!(!bit_get(&heap.has_kept, dyn_start + 1));
        assert!(!bit_get(&heap.has_kept, dyn_start + 65));
        assert!(bit_get(&heap.has_kept, 0));
        assert!(bit_get(&heap.has_kept, 64));

        // `note_buffer_inner` with `notify_changed = false` records buffer membership
        // without re-queueing the slot into `self.changed`.
        let mut raw_desc = [0u32; 14];
        raw_desc[0] = 1; // buffer resource
        let mut srv_desc = [0u32; 16];
        srv_desc[0] = 0; // format = UNKNOWN
        srv_desc[1] = 1; // dimension = BUFFER
        srv_desc[6] = 16; // structure byte stride
        let view = View::Placed(false, raw_desc, srv_desc);
        let _ = heap.note_buffer_inner(7, Some(&view), true, false);
        assert!(heap.buffer_slots().contains(&7));
        assert!(heap.buffer_changes().is_empty());
        let _ = heap.note_buffer_inner(7, Some(&view), false, true);
        assert_eq!(heap.buffer_changes(), vec![7]);
    }
}
