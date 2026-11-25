use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, s};
use windows_sys::Win32::System::Memory as mem;

const K64: usize = 64 << 10;
const MEM_WRITE_WATCH: u32 = 0x20_0000;

pub(crate) fn probe_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var("OATHKEEP_INPLACE_PROBE")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    })
}

pub(crate) fn level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var("OATHKEEP_INPLACE")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    })
}

fn most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_INPLACE_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(4096)
            << 20
    })
}

fn sections_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_INPLACE_SECTIONS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|mb| mb << 20)
            .unwrap_or_else(|| most().min(2048 << 20))
    })
}

fn min_bytes() -> u64 {
    static MIN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| {
        if let Some(kb) = std::env::var("OATHKEEP_INPLACE_MIN_KB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            return kb << 10;
        }
        std::env::var("OATHKEEP_INPLACE_MIN_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
            << 20
    })
}

fn try_reserve_budget(
    target: &AtomicU64,
    other: &AtomicU64,
    delta: u64,
    target_cap: u64,
    total_cap: u64,
) -> bool {
    if delta == 0 {
        return true;
    }
    let mut cur = target.load(Ordering::Relaxed);
    loop {
        let Some(next) = cur.checked_add(delta) else {
            return false;
        };
        if next > target_cap || next.saturating_add(other.load(Ordering::Relaxed)) > total_cap {
            return false;
        }
        match target.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
}

fn commit_allows(delta: u64) -> bool {
    let headroom = crate::resource::commit_headroom();
    if headroom == 0 {
        return true;
    }
    crate::host::available_commit().is_none_or(|avail| avail >= headroom.saturating_add(delta))
}

static STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

pub(crate) struct SectionHeap {
    section: usize,
    size: u64,
    heap: ID3D12Heap,
}

impl Drop for SectionHeap {
    fn drop(&mut self) {
        SECTION_BYTES.fetch_sub(self.size, Ordering::AcqRel);
        if let Ok(mut guard) = SECTION_HEAPS.lock()
            && let Some(map) = guard.as_mut()
            && let Some(SectionEntry::Open(weak)) = map.get(&self.section)
            && weak.strong_count() == 0
        {
            map.remove(&self.section);
        }
        crate::host::retire(self.heap.clone());
    }
}

pub(crate) struct PartHeap {
    bytes: u64,
    heap: ID3D12Heap,
}

impl Drop for PartHeap {
    fn drop(&mut self) {
        if self.bytes > 0 {
            PART_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
        }
        crate::host::retire(self.heap.clone());
    }
}

enum SectionEntry {
    Refused,
    Opening,
    Open(Weak<SectionHeap>),
}

static SECTION_HEAPS: Mutex<Option<std::collections::HashMap<usize, SectionEntry>>> =
    Mutex::new(None);
static SECTION_OPENED: Condvar = Condvar::new();
static SECTION_BYTES: AtomicU64 = AtomicU64::new(0);
static PART_BYTES: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
pub(crate) enum PlaceHeap {
    Part(PartHeap),
    Section(Arc<SectionHeap>),
}

pub(crate) struct Placed {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) heap: PlaceHeap,
    pub(crate) gpu: u64,
    pub(crate) bias: u64,
}

#[cfg(test)]
fn section_info(section: usize) -> Option<(u64, bool)> {
    views_read().sections.get(&section).copied()
}

pub(crate) fn section_run_in_place(start: u64, len: u64) -> bool {
    if level() < 3 {
        return false;
    }
    let at = start & !(K64 as u64 - 1);
    let bytes = (start + len.max(1)).next_multiple_of(K64 as u64) - at;
    let v = views_read();
    match cover_in(&v.views, at as usize, bytes as usize) {
        Cover::Run(section, off) => v
            .sections
            .get(&section)
            .is_some_and(|&(size, committed)| committed && off.checked_add(bytes).is_some_and(|e| e <= size)),
        _ => false,
    }
}

pub(crate) fn section_run_around(
    address: u64,
    end: u64,
    max_len: u64,
) -> Option<(u64, u64, bool)> {
    if level() < 3 || max_len < K64 as u64 {
        return None;
    }
    let at = (address & !(K64 as u64 - 1)) as usize;
    let need_end = end.max(address.saturating_add(1)).next_multiple_of(K64 as u64) as usize;
    if need_end <= at || (need_end - at) as u64 > max_len {
        return None;
    }
    let v = views_read();
    let (&base, &(bytes, section, offset)) = v
        .views
        .range(..=at)
        .next_back()
        .filter(|(base, view)| *base + view.0 > at)?;
    let (sec_size, committed) = v.sections.get(&section).copied()?;
    if !committed {
        return None;
    }
    let mut run_start = base;
    let mut run_off = offset;
    let mut run_end = base.saturating_add(bytes);
    // Extend forward through contiguous mappings of the same section.
    while let Some(&(next_bytes, next_sec, next_off)) = v.views.get(&run_end) {
        if next_sec != section || next_off != run_off + (run_end - run_start) as u64 {
            break;
        }
        run_end = run_end.saturating_add(next_bytes);
    }
    // Extend backward through contiguous mappings of the same section.
    while let Some((&prev_base, &(prev_bytes, prev_sec, prev_off))) =
        v.views.range(..run_start).next_back()
    {
        if prev_base.saturating_add(prev_bytes) != run_start
            || prev_sec != section
            || prev_off.saturating_add(prev_bytes as u64) != run_off
        {
            break;
        }
        run_start = prev_base;
        run_off = prev_off;
    }
    if run_end < need_end {
        return None;
    }
    // Clamp the span to `max_len` around `[at, need_end)` while preserving as
    // much forward headroom from `at` as possible.
    if (run_end - run_start) as u64 > max_len {
        let cap = max_len as usize;
        let new_start = if run_end - at <= cap {
            run_end - cap
        } else {
            run_end = at + cap;
            at
        };
        run_off = run_off.saturating_add((new_start - run_start) as u64);
        run_start = new_start;
    }
    let final_len = (run_end - run_start) as u64;
    if run_off.checked_add(final_len).is_none_or(|e| e > sec_size) {
        return None;
    }
    let next_mapped = v
        .views
        .range(..=run_end)
        .next_back()
        .is_some_and(|(b, view)| *b + view.0 > run_end);
    Some((run_start as u64, final_len, next_mapped))
}

fn section_heap(
    host: &crate::host::Host,
    section: usize,
    size: u64,
) -> Result<Arc<SectionHeap>, Option<windows::core::Error>> {
    {
        let mut heaps = SECTION_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match heaps.as_ref().and_then(|m| m.get(&section)) {
                Some(SectionEntry::Open(weak)) => {
                    if let Some(existing) = weak.upgrade() {
                        return Ok(existing);
                    }
                    break;
                }
                Some(SectionEntry::Refused) => return Err(None),
                Some(SectionEntry::Opening) => {
                    heaps = SECTION_OPENED
                        .wait(heaps)
                        .unwrap_or_else(|e| e.into_inner());
                }
                None => break,
            }
        }
        const SHARED_HEADROOM: u64 = 1024 << 20;
        if !commit_allows(size) {
            STATS[6].fetch_add(1, Ordering::Relaxed);
            return Err(None);
        }
        if let Some([_, _, shared_budget, shared_usage]) = host.video_memory()
            && shared_usage
                .saturating_add(size)
                .saturating_add(SHARED_HEADROOM)
                > shared_budget
        {
            STATS[6].fetch_add(1, Ordering::Relaxed);
            return Err(None);
        }
        if !try_reserve_budget(&SECTION_BYTES, &PART_BYTES, size, sections_most(), most()) {
            STATS[6].fetch_add(1, Ordering::Relaxed);
            return Err(None);
        }
        heaps
            .get_or_insert_with(Default::default)
            .insert(section, SectionEntry::Opening);
    }
    let opened = match open(&host.device, From::Section(section)) {
        Ok(heap) => heap,
        Err(e) => {
            SECTION_BYTES.fetch_sub(size, Ordering::AcqRel);
            let mut heaps = SECTION_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
            let map = heaps.get_or_insert_with(Default::default);
            // Transient OOM may clear after sweeps/reclaim; only permanently
            // mark non-OOM refusals (e.g. unsupported/reserved sections).
            if e.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
                map.remove(&section);
            } else {
                map.insert(section, SectionEntry::Refused);
            }
            SECTION_OPENED.notify_all();
            return Err(Some(e));
        }
    };
    let total = SECTION_BYTES.load(Ordering::Relaxed);
    tracing::debug!(
        section = format_args!("{section:#x}"),
        kb = size >> 10,
        total_mb = total >> 20,
        "title page-pool section opened in place as one host heap"
    );
    let arc = Arc::new(SectionHeap {
        section,
        size,
        heap: opened,
    });
    {
        let mut heaps = SECTION_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        heaps
            .get_or_insert_with(Default::default)
            .insert(section, SectionEntry::Open(Arc::downgrade(&arc)));
        SECTION_OPENED.notify_all();
    }
    Ok(arc)
}

fn place_section(
    host: &crate::host::Host,
    section: usize,
    sec_size: u64,
    section_off: u64,
    start: u64,
    at: u64,
    bytes: u64,
) -> Result<Placed, Option<windows::core::Error>> {
    if section_off.checked_add(bytes).is_none_or(|end| end > sec_size) {
        return Err(None);
    }
    let section_heap = section_heap(host, section, sec_size)?;
    let buffer = place(&host.device, &section_heap.heap, section_off, bytes).map_err(Some)?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + (start - at);
    Ok(Placed {
        buffer,
        heap: PlaceHeap::Section(section_heap),
        gpu,
        bias: start - at,
    })
}

pub(crate) fn place_part(
    device: &ID3D12Device,
    start: u64,
    len: u64,
) -> Result<Placed, Option<windows::core::Error>> {
    let at = start & !(K64 as u64 - 1);
    let bytes = (start + len.max(1)).next_multiple_of(K64 as u64) - at;
    if !private_whole(at as usize, bytes as usize) {
        return Err(None);
    }
    let heap = open(device, From::Part(at as usize, bytes as usize)).map_err(Some)?;
    let buffer = place(device, &heap, 0, bytes).map_err(Some)?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + (start - at);
    Ok(Placed {
        buffer,
        heap: PlaceHeap::Part(PartHeap { bytes: 0, heap }),
        gpu,
        bias: start - at,
    })
}

pub(crate) fn place_for(start: u64, len: u64, window: bool) -> Option<Placed> {
    let lvl = level();
    if lvl < if window { 1 } else { 2 } {
        return None;
    }
    let floor = min_bytes();
    if floor > 0 && len < floor {
        return None;
    }
    let at = start & !(K64 as u64 - 1);
    let bytes = (start + len.max(1)).next_multiple_of(K64 as u64) - at;
    let host = crate::host::host()?;
    let (covered, sec_info) = {
        let v = views_read();
        let covered = cover_in(&v.views, at as usize, bytes as usize);
        let info = match covered {
            Cover::Run(section, _) if lvl >= 3 => v.sections.get(&section).copied(),
            _ => None,
        };
        (covered, info)
    };
    let placed = match covered {
        Cover::None => {
            const SHARED_HEADROOM: u64 = 2048 << 20;
            if !commit_allows(bytes) {
                STATS[6].fetch_add(1, Ordering::Relaxed);
                return None;
            }
            if let Some([_, _, shared_budget, shared_usage]) = host.video_memory()
                && shared_usage
                    .saturating_add(bytes)
                    .saturating_add(SHARED_HEADROOM)
                    > shared_budget
            {
                STATS[6].fetch_add(1, Ordering::Relaxed);
                return None;
            }
            if !try_reserve_budget(&PART_BYTES, &SECTION_BYTES, bytes, most(), most()) {
                STATS[6].fetch_add(1, Ordering::Relaxed);
                return None;
            }
            match place_part(&host.device, start, len) {
                Ok(mut placed) => {
                    if let PlaceHeap::Part(part) = &mut placed.heap {
                        part.bytes = bytes;
                    }
                    Ok(placed)
                }
                Err(e) => {
                    PART_BYTES.fetch_sub(bytes, Ordering::AcqRel);
                    Err(e)
                }
            }
        }
        Cover::Run(section, section_off) if lvl >= 3 => match sec_info {
            Some((sec_size, true)) => {
                place_section(host, section, sec_size, section_off, start, at, bytes)
            }
            _ => Err(None),
        },
        _ => {
            STATS[4].fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    match placed {
        Ok(placed) => {
            let at = if window { 0 } else { 2 };
            STATS[at].fetch_add(1, Ordering::Relaxed);
            STATS[at + 1].fetch_add(len, Ordering::Relaxed);
            Some(placed)
        }
        Err(None) => {
            STATS[4].fetch_add(1, Ordering::Relaxed);
            None
        }
        Err(Some(e)) => {
            if STATS[5].fetch_add(1, Ordering::Relaxed) < 8 {
                tracing::warn!(start = format_args!("{start:#x}"), len, error = %e, "title memory not opened in place: a copy instead");
            }
            None
        }
    }
}

pub(crate) fn gone(window: bool, bytes: u64) {
    let at = if window { 0 } else { 2 };
    STATS[at].fetch_sub(1, Ordering::Relaxed);
    STATS[at + 1].fetch_sub(bytes, Ordering::Relaxed);
}

pub fn title_memory_freeing(at: usize, len: usize) {
    if STATS[0].load(Ordering::Relaxed) + STATS[2].load(Ordering::Relaxed) == 0 {
        return;
    }
    let gone = crate::shadow::in_place_let_go(at as u64, len as u64);
    STATS[7].fetch_add(gone, Ordering::Relaxed);
}

fn say() {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 10_000
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    let open_sections = SECTION_HEAPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |m| {
            m.values()
                .filter(|e| matches!(e, SectionEntry::Open(w) if w.strong_count() > 0))
                .count()
        });
    let section_mb = SECTION_BYTES.load(Ordering::Relaxed) >> 20;
    let part_mb = PART_BYTES.load(Ordering::Relaxed) >> 20;
    tracing::info!(
        "title memory in place: {} windows {} MB, {} title buffers {} MB ({} open sections {} MB, private heaps {} MB, caps {} / {} MB) (no copy, no refresh, no copy-back); turned down: {} view memory or not private memory committed whole, \
         {} refused by the host, {} over budget/headroom; {} let go because their memory changed",
        s[0],
        s[1] >> 20,
        s[2],
        s[3] >> 20,
        open_sections,
        section_mb,
        part_mb,
        sections_most() >> 20,
        most() >> 20,
        s[4],
        s[5],
        s[6],
        s[7]
    );
}

pub(crate) enum From {
    Region(usize),
    Part(usize, usize),
    Section(usize),
}

pub(crate) fn open(device: &ID3D12Device, from: From) -> windows::core::Result<ID3D12Heap> {
    // SAFETY: the calls take an address or a handle and check both
    // themselves; a refusal is an error code.
    unsafe {
        match from {
            From::Region(at) => device
                .cast::<ID3D12Device3>()?
                .OpenExistingHeapFromAddress(at as *const c_void),
            From::Part(at, bytes) => device
                .cast::<ID3D12Device13>()?
                .OpenExistingHeapFromAddress1(at as *const c_void, bytes),
            From::Section(handle) => device
                .cast::<ID3D12Device3>()?
                .OpenExistingHeapFromFileMapping(HANDLE(handle as *mut c_void)),
        }
    }
}

fn buffer_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

pub(crate) fn place(
    device: &ID3D12Device,
    heap: &ID3D12Heap,
    offset: u64,
    size: u64,
) -> windows::core::Result<ID3D12Resource> {
    let mut placed: Option<ID3D12Resource> = None;
    let flags =
        D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    // SAFETY: plain creation in a live heap.
    unsafe {
        device.CreatePlacedResource(
            heap,
            offset,
            &buffer_desc(size, flags),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut placed,
        )?
    };
    placed.ok_or_else(windows::core::Error::empty)
}

enum ShownPages {
    One(usize),
    Many(Vec<usize>),
}

impl ShownPages {
    fn push(&mut self, addr: usize) {
        match self {
            ShownPages::One(first) => *self = ShownPages::Many(vec![*first, addr]),
            ShownPages::Many(list) => list.push(addr),
        }
    }

    fn remove(&mut self, addr: usize) -> bool {
        match self {
            ShownPages::One(first) => *first == addr,
            ShownPages::Many(list) => {
                list.retain(|&shown| shown != addr);
                if list.len() == 1 {
                    *self = ShownPages::One(list[0]);
                    false
                } else {
                    list.is_empty()
                }
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            ShownPages::One(_) => 1,
            ShownPages::Many(list) => list.len(),
        }
    }

    fn as_slice(&self) -> &[usize] {
        match self {
            ShownPages::One(first) => std::slice::from_ref(first),
            ShownPages::Many(list) => list.as_slice(),
        }
    }
}

struct Views {
    views: BTreeMap<usize, (usize, usize, u64)>,
    pages: BTreeMap<(usize, u64), ShownPages>,
    sections: BTreeMap<usize, (u64, bool)>,
}

static VIEWS: RwLock<Views> = RwLock::new(Views {
    views: BTreeMap::new(),
    pages: BTreeMap::new(),
    sections: BTreeMap::new(),
});

fn views() -> std::sync::RwLockWriteGuard<'static, Views> {
    VIEWS.write().unwrap_or_else(|e| e.into_inner())
}

fn views_read() -> std::sync::RwLockReadGuard<'static, Views> {
    VIEWS.read().unwrap_or_else(|e| e.into_inner())
}

pub fn title_memory_section(section: usize, bytes: u64, committed: bool) {
    views().sections.insert(section, (bytes, committed));
    if let Ok(mut guard) = SECTION_HEAPS.lock()
        && let Some(map) = guard.as_mut()
        && matches!(map.get(&section), Some(SectionEntry::Refused))
    {
        map.remove(&section);
    }
}

pub fn title_memory_section_closing(section: usize) {
    views().sections.remove(&section);
    if let Ok(mut guard) = SECTION_HEAPS.lock()
        && let Some(map) = guard.as_mut()
    {
        map.remove(&section);
    }
}

pub fn title_memory_view(at: usize, len: usize, section: usize, offset: u64) {
    {
        let mut v = views();
        if let Some((old_bytes, old_section, old_offset)) = v.views.insert(at, (len, section, offset))
        {
            for k in 0..old_bytes.div_ceil(K64) {
                let key = (old_section, old_offset + (k * K64) as u64);
                if let Some(pages) = v.pages.get_mut(&key)
                    && pages.remove(at + k * K64)
                {
                    v.pages.remove(&key);
                }
            }
        }
        for k in 0..len.div_ceil(K64) {
            let key = (section, offset + (k * K64) as u64);
            let addr = at + k * K64;
            match v.pages.get_mut(&key) {
                Some(shown) => shown.push(addr),
                None => {
                    v.pages.insert(key, ShownPages::One(addr));
                }
            }
        }
    }
    crate::watch::views_mapped(section, offset, len);
}

pub(crate) fn views_gone(at: usize, len: usize) {
    let mut v = views();
    if v.views.is_empty() {
        return;
    }
    let end = at.saturating_add(len);
    let first = v
        .views
        .range(..=at)
        .next_back()
        .filter(|(base, view)| *base + view.0 > at)
        .map(|(base, _)| *base);
    let gone: Vec<usize> = first
        .into_iter()
        .chain(
            v.views
                .range(at.saturating_add(1)..end)
                .map(|(base, _)| *base),
        )
        .collect();
    for base in gone {
        let Some((bytes, section, offset)) = v.views.remove(&base) else {
            continue;
        };
        for k in 0..bytes.div_ceil(K64) {
            let key = (section, offset + (k * K64) as u64);
            if let Some(pages) = v.pages.get_mut(&key)
                && pages.remove(base + k * K64)
            {
                v.pages.remove(&key);
            }
        }
    }
}

fn coalesce_blocks(mut blocks: Vec<usize>) -> Vec<(usize, usize)> {
    if blocks.is_empty() {
        return Vec::new();
    }
    blocks.sort_unstable();
    blocks.dedup();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for block in blocks {
        match out.last_mut() {
            Some(last) if block == last.1 => last.1 = block + 1,
            _ => out.push((block, block + 1)),
        }
    }
    out
}

pub(crate) fn alias_blocks_between(
    start: usize,
    end: usize,
    max_views: usize,
) -> (Vec<usize>, Vec<(usize, usize)>, u64) {
    if start >= end {
        return (Vec::new(), Vec::new(), 0);
    }
    let v = views_read();
    if v.views.is_empty() {
        return (Vec::new(), Vec::new(), 0);
    }
    let mut overlapping: Vec<(usize, usize, usize, u64)> = Vec::new();
    if let Some((&base, &(bytes, section, offset))) = v.views.range(..=start).next_back()
        && base + bytes > start
    {
        overlapping.push((base, bytes, section, offset));
    }
    for (&base, &(bytes, section, offset)) in v.views.range(start.saturating_add(1)..end) {
        overlapping.push((base, bytes, section, offset));
    }
    if overlapping.is_empty() {
        return (Vec::new(), Vec::new(), 0);
    }
    let from = start / K64;
    let to = end.div_ceil(K64);
    let mut all = Vec::new();
    let mut others = Vec::new();
    let mut count = 0u64;
    for (base, bytes, section, offset) in overlapping {
        let first = (base / K64).max(from);
        let last = (base.saturating_add(bytes).div_ceil(K64)).min(to);
        for block in first..last {
            let page = block * K64;
            let within = page.wrapping_sub(base);
            if within >= bytes || !within.is_multiple_of(K64) {
                continue;
            }
            let Some(shown) = v.pages.get(&(section, offset + within as u64)) else {
                continue;
            };
            if shown.len() < 2 || shown.len() > max_views {
                continue;
            }
            count += 1;
            for &alias in shown.as_slice() {
                let alias_block = alias / K64;
                all.push(alias_block);
                if alias_block != block {
                    others.push(alias_block);
                }
            }
        }
    }
    all.sort_unstable();
    all.dedup();
    (all, coalesce_blocks(others), count)
}

pub(crate) fn alias_spans_of_blocks(blocks: &[usize]) -> (Vec<usize>, Vec<(usize, usize)>) {
    if blocks.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let v = views_read();
    if v.views.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut all = Vec::new();
    let mut others = Vec::new();
    for &block in blocks {
        let page = block * K64;
        let Some((&base, &(bytes, section, offset))) = v.views.range(..=page).next_back() else {
            continue;
        };
        let within = page.wrapping_sub(base);
        if within >= bytes || !within.is_multiple_of(K64) {
            continue;
        }
        let Some(shown) = v.pages.get(&(section, offset + within as u64)) else {
            continue;
        };
        for &alias in shown.as_slice() {
            let alias_block = alias / K64;
            all.push(alias_block);
            if alias_block != block {
                others.push(alias_block);
            }
        }
    }
    all.sort_unstable();
    all.dedup();
    (all, coalesce_blocks(others))
}

pub(crate) fn views_of_section(section: usize, offset: u64, len: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return Vec::new();
    }
    let v = views_read();
    let end = offset.saturating_add(len as u64);
    let mut pages: Vec<usize> = Vec::new();
    let mut at = offset - offset % K64 as u64;
    while at < end {
        if let Some(shown) = v.pages.get(&(section, at))
            && shown.len() <= crate::watch::MANY_VIEWS
        {
            pages.extend(shown.as_slice().iter().copied());
        }
        match at.checked_add(K64 as u64) {
            Some(next) => at = next,
            None => break,
        }
    }
    pages.sort_unstable();
    pages.dedup();
    let mut runs: Vec<(usize, usize)> = Vec::with_capacity(pages.len());
    for page in pages {
        match runs.last_mut() {
            Some((first, bytes)) if *first + *bytes == page => *bytes += K64,
            _ => runs.push((page, K64)),
        }
    }
    runs
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Cover {
    None,
    Holes,
    Scattered,
    Run(usize, u64),
}

fn cover_in(views: &BTreeMap<usize, (usize, usize, u64)>, at: usize, len: usize) -> Cover {
    let end = at + len.max(1);
    let Some((&base, &(bytes, section, offset))) = views
        .range(..=at)
        .next_back()
        .filter(|(base, view)| *base + view.0 > at)
    else {
        return Cover::None;
    };
    let first = (section, offset + (at - base) as u64);
    let (mut reached, mut one) = (base + bytes, true);
    while reached < end {
        let Some(&(bytes, s, o)) = views.get(&reached) else {
            return Cover::Holes;
        };
        one &= s == first.0 && o == first.1 + (reached - at) as u64;
        reached += bytes;
    }
    if one {
        Cover::Run(first.0, first.1)
    } else {
        Cover::Scattered
    }
}

pub(crate) fn cover(at: usize, len: usize) -> Cover {
    cover_in(&views_read().views, at, len)
}

pub(crate) fn private_whole(at: usize, len: usize) -> bool {
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == mem::MEM_COMMIT
        && info.Type == mem::MEM_PRIVATE
        && info.Protect == mem::PAGE_READWRITE
        && (info.BaseAddress as usize).saturating_add(info.RegionSize) >= at.saturating_add(len)
}

fn hr<T>(result: &windows::core::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(e) => format!("REFUSED {:#010x} ({})", e.code().0, e.message().trim()),
    }
}

struct Rig {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    done: u64,
    event: HANDLE,
}

impl Drop for Rig {
    fn drop(&mut self) {
        // SAFETY: our event.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

impl Rig {
    fn new(device: &ID3D12Device) -> windows::core::Result<Rig> {
        // SAFETY: plain creation of objects of our own.
        unsafe {
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })?;
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
            list.Close()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            Ok(Rig {
                device: device.clone(),
                queue,
                allocator,
                list,
                fence,
                done: 0,
                event,
            })
        }
    }

    fn run(
        &mut self,
        record: impl FnOnce(&ID3D12GraphicsCommandList),
    ) -> windows::core::Result<()> {
        // SAFETY: our own list and queue, one run at a time.
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            record(&self.list);
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.done += 1;
            self.queue.Signal(&self.fence, self.done)?;
            self.fence.SetEventOnCompletion(self.done, self.event)?;
            if WaitForSingleObject(self.event, 10_000).0 != 0 {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            self.device.GetDeviceRemovedReason()
        }
    }

    fn committed(
        &self,
        kind: D3D12_HEAP_TYPE,
        size: u64,
        state: D3D12_RESOURCE_STATES,
    ) -> windows::core::Result<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: kind,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &buffer_desc(size, D3D12_RESOURCE_FLAG_NONE),
                state,
                None,
                &mut buffer,
            )?
        };
        buffer.ok_or_else(windows::core::Error::empty)
    }

    fn fetch(
        &mut self,
        resource: &ID3D12Resource,
        offset: u64,
        size: usize,
    ) -> windows::core::Result<Vec<u8>> {
        let back = self.committed(
            D3D12_HEAP_TYPE_READBACK,
            size as u64,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )?;
        // SAFETY: two live buffers, the range inside both.
        self.run(|list| unsafe { list.CopyBufferRegion(&back, 0, resource, offset, size as u64) })?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the mapping holds `size` bytes until unmapped.
        unsafe {
            back.Map(0, None, Some(&mut data))?;
            let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec();
            back.Unmap(0, None);
            Ok(bytes)
        }
    }

    fn store(
        &mut self,
        resource: &ID3D12Resource,
        offset: u64,
        bytes: &[u8],
    ) -> windows::core::Result<()> {
        let up = self.committed(
            D3D12_HEAP_TYPE_UPLOAD,
            bytes.len() as u64,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the mapping holds the buffer's bytes until unmapped.
        unsafe {
            up.Map(0, None, Some(&mut data))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());
            up.Unmap(0, None);
        }
        // SAFETY: two live buffers, the range inside both.
        self.run(|list| unsafe {
            list.CopyBufferRegion(resource, offset, &up, 0, bytes.len() as u64)
        })
    }

    fn shade(&mut self, gpu: u64) -> windows::core::Result<()> {
        const SOURCE: &str = "#define RS \"UAV(u0)\"\nRWByteAddressBuffer b : register(u0);\n[RootSignature(RS)]\n[numthreads(1, 1, 1)]\nvoid main() { b.Store(8, b.Load(0) + b.Load(4)); }\n";
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; then the
        // blob's own bytes, which hold the root signature too.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("inplace"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
            let code = code.ok_or_else(windows::core::Error::empty)?;
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso =
                self.device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    })?;
            (root, pso)
        };
        // SAFETY: an open list of ours; the view names a live buffer.
        self.run(|list| unsafe {
            list.SetComputeRootSignature(&root);
            list.SetPipelineState(&pso);
            list.SetComputeRootUnorderedAccessView(0, gpu);
            list.Dispatch(1, 1, 1);
        })
    }

    fn copy_ms(&mut self, from: &ID3D12Resource, bytes: u64) -> windows::core::Result<f64> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let mut to: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &buffer_desc(bytes, D3D12_RESOURCE_FLAG_NONE),
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut to,
            )?
        };
        let to = to.ok_or_else(windows::core::Error::empty)?;
        let mut best = f64::MAX;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            // SAFETY: two live buffers of `bytes` bytes or more.
            self.run(|list| unsafe { list.CopyBufferRegion(&to, 0, from, 0, bytes) })?;
            best = best.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(best)
    }
}

fn fill(at: *mut u8, len: usize, seed: u8) {
    for i in 0..len {
        // SAFETY: the caller's own committed memory of `len` bytes.
        unsafe {
            at.add(i)
                .write((i as u8).wrapping_mul(31).wrapping_add(seed))
        };
    }
}

fn same(at: *const u8, bytes: &[u8]) -> bool {
    // SAFETY: the caller's own committed memory of that length.
    unsafe { std::slice::from_raw_parts(at, bytes.len()) == bytes }
}

fn working_set_mb() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe {
        K32GetProcessMemoryInfo(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            &mut counters,
            counters.cb,
        )
    };
    counters.WorkingSetSize as u64 >> 20
}

#[derive(Default)]
pub(crate) struct Probe {
    pub(crate) lines: Vec<String>,
    pub(crate) part_reads: bool,
    pub(crate) part_writes: bool,
    pub(crate) section_reads: bool,
    pub(crate) section_writes: bool,
    pub(crate) section_outlives_views: bool,
    pub(crate) reserved_section_refused: bool,
}

pub(crate) fn probe(
    device: &ID3D12Device,
    memory: &dyn Fn() -> Option<[u64; 4]>,
    mb: usize,
) -> Probe {
    let mut p = Probe::default();
    let mut rig = match Rig::new(device) {
        Ok(rig) => rig,
        Err(e) => {
            p.lines.push(format!("no queue of its own: {e}"));
            return p;
        }
    };
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    // SAFETY: a plain query that fills our structure.
    let _ = unsafe {
        device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS,
            (&raw mut options).cast(),
            size_of_val(&options) as u32,
        )
    };
    p.lines.push(format!(
        "the device: ID3D12Device3 {}, ID3D12Device13 {} (parts of an allocation); cross-adapter row-major textures {}; heap tier {}",
        device.cast::<ID3D12Device3>().is_ok(),
        device.cast::<ID3D12Device13>().is_ok(),
        options.CrossAdapterRowMajorTextureSupported.as_bool(),
        options.ResourceHeapTier.0
    ));
    // SAFETY: memory of the probe's own, made and released here; every
    // address used lies inside it.
    unsafe {
        private(&mut p, &mut rig);
        section(&mut p, &mut rig);
        if mb > 0 {
            cost(&mut p, &mut rig, memory, mb);
        }
    }
    p.lines.push(format!(
        "VERDICT: private memory in place: reads {}, writes {}; the page pool's section in place: reads {}, writes {}, whatever the views do {}; \
         a reserved section is refused {} (the pool must be made committed to be opened)",
        p.part_reads, p.part_writes, p.section_reads, p.section_writes, p.section_outlives_views, p.reserved_section_refused
    ));
    p
}

fn describe(heap: &ID3D12Heap) -> String {
    // SAFETY: a plain query.
    let d = unsafe { heap.GetDesc() };
    format!(
        "heap of {} KiB: type {} (4 = custom), CPU pages {} (3 = write-back), pool {} (1 = the PC's memory), alignment {}, flags {:#x} (0x1 shared, 0x20 cross-adapter, 0x200 write watch, 0x400 shader atomics)",
        d.SizeInBytes >> 10,
        d.Properties.Type.0,
        d.Properties.CPUPageProperty.0,
        d.Properties.MemoryPoolPreference.0,
        d.Alignment,
        d.Flags.0
    )
}

unsafe fn private(p: &mut Probe, rig: &mut Rig) {
    const SIZE: usize = 64 * K64;
    unsafe {
        let base = mem::VirtualAlloc(
            std::ptr::null(),
            SIZE,
            mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH,
            mem::PAGE_READWRITE,
        )
        .cast::<u8>();
        if base.is_null() {
            p.lines.push("private memory: none to try with".to_owned());
            return;
        }
        let (at, bytes) = (base.add(16 * K64), 16 * K64);
        let whole = open(&rig.device, From::Region(base as usize));
        p.lines.push(format!(
            "private memory, the whole allocation (OpenExistingHeapFromAddress): {}",
            hr(&whole)
        ));
        drop(whole);
        let opened = open(&rig.device, From::Part(at as usize, bytes));
        p.lines.push(format!(
            "private memory, a part of it (OpenExistingHeapFromAddress1): {}",
            hr(&opened)
        ));
        if let Ok(heap) = &opened {
            p.lines.push(format!("  {}", describe(heap)));
            let placed = place(&rig.device, heap, K64 as u64, 4 * K64 as u64);
            p.lines.push(format!(
                "  a buffer placed in it (cross-adapter, unordered access): {}",
                hr(&placed)
            ));
            if let Ok(buffer) = &placed {
                let memory = at.add(K64);
                fill(memory, 4 * K64, 1);
                let first = rig
                    .fetch(buffer, 0, 4 * K64)
                    .is_ok_and(|got| same(memory, &got));
                fill(memory, 4 * K64, 77);
                let second = rig
                    .fetch(buffer, 0, 4 * K64)
                    .is_ok_and(|got| same(memory, &got));
                p.part_reads = first && second;
                p.lines.push(format!("  the GPU reads the memory's bytes: {first}; and those the CPU wrote afterwards, with no call in between: {second}"));
                mem::ResetWriteWatch(base.cast(), SIZE);
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x5a).collect();
                let copied = rig.store(buffer, 4096, &new).is_ok() && same(memory.add(4096), &new);
                let (mut found, mut count, mut granularity) = ([0usize; 64], 64usize, 0u32);
                let watched = mem::GetWriteWatch(
                    0,
                    base.cast(),
                    SIZE,
                    found.as_mut_ptr().cast(),
                    &mut count,
                    &mut granularity,
                ) == 0;
                memory.cast::<[u32; 3]>().write([40, 2, 0]);
                let shaded = rig.shade(buffer.GetGPUVirtualAddress());
                let shader = shaded.is_ok() && memory.cast::<[u32; 3]>().read()[2] == 42;
                p.part_writes = copied && shader;
                p.lines.push(format!(
                    "  the CPU reads at the memory's address what the GPU wrote: by a copy {copied}; by a shader through unordered access {shader} ({}); \
                     write watching saw {} of the copy's 16 pages{}",
                    hr(&shaded),
                    if watched { count } else { 0 },
                    if watched { "" } else { " (the asking failed)" }
                ));
            }
            let mut old = 0;
            let protect_in =
                mem::VirtualProtect(at.add(8 * K64).cast(), K64, mem::PAGE_READONLY, &mut old) != 0;
            if protect_in {
                mem::VirtualProtect(at.add(8 * K64).cast(), K64, mem::PAGE_READWRITE, &mut old);
            }
            let decommit_in = mem::VirtualFree(at.add(9 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            let decommit_out =
                mem::VirtualFree(base.add(40 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            p.lines.push(format!(
                "  with the heap standing, the title's own calls: read-only inside the part {protect_in}, decommit inside {decommit_in}, decommit outside {decommit_out} \
                 (false inside means the heap must go before the title frees or guards those pages)"
            ));
            drop(placed);
        }
        drop(opened);
        let freed = mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE) != 0;
        p.lines
            .push(format!("  released after the heap went: {freed}"));
    }
}

unsafe fn section(p: &mut Probe, rig: &mut Rig) {
    const SIZE: usize = 32 * K64;
    unsafe {
        let process = windows_sys::Win32::System::Threading::GetCurrentProcess();
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let reserved = mem::CreateFileMappingW(
            invalid,
            std::ptr::null(),
            mem::PAGE_READWRITE | mem::SEC_RESERVE,
            0,
            SIZE as u32,
            std::ptr::null(),
        );
        if !reserved.is_null() {
            let opened = open(&rig.device, From::Section(reserved as usize));
            p.reserved_section_refused = opened.is_err();
            p.lines.push(format!("a section made reserved, as the page pool is today (OpenExistingHeapFromFileMapping): {}", hr(&opened)));
            drop(opened);
            windows_sys::Win32::Foundation::CloseHandle(reserved);
        }
        let handle = mem::CreateFileMappingW(
            invalid,
            std::ptr::null(),
            mem::PAGE_READWRITE | mem::SEC_COMMIT,
            0,
            SIZE as u32,
            std::ptr::null(),
        );
        let hole = mem::VirtualAlloc2(
            process,
            std::ptr::null(),
            SIZE,
            mem::MEM_RESERVE | mem::MEM_RESERVE_PLACEHOLDER,
            mem::PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        )
        .cast::<u8>();
        if handle.is_null() || hole.is_null() {
            p.lines
                .push("a committed section: none to try with".to_owned());
            return;
        }
        mem::VirtualFree(
            hole.cast(),
            8 * K64,
            mem::MEM_RELEASE | mem::MEM_PRESERVE_PLACEHOLDER,
        );
        let view = mem::MapViewOfFile3(
            handle,
            process,
            hole.cast(),
            (4 * K64) as u64,
            8 * K64,
            mem::MEM_REPLACE_PLACEHOLDER,
            mem::PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        );
        let opened = open(&rig.device, From::Section(handle as usize));
        p.lines.push(format!(
            "a section made committed (OpenExistingHeapFromFileMapping): {}",
            hr(&opened)
        ));
        let mut unmapped = false;
        if let (Ok(heap), false) = (&opened, view.Value.is_null()) {
            p.lines.push(format!("  {}", describe(heap)));
            let placed = place(&rig.device, heap, 5 * K64 as u64, 2 * K64 as u64);
            p.lines.push(format!(
                "  a buffer placed at the section's offset of a view's pages: {}",
                hr(&placed)
            ));
            if let Ok(buffer) = &placed {
                let memory = hole.add(K64);
                fill(memory, 2 * K64, 3);
                p.section_reads = rig
                    .fetch(buffer, 0, 2 * K64)
                    .is_ok_and(|got| same(memory, &got));
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x33).collect();
                p.section_writes = rig.store(buffer, 0, &new).is_ok() && same(memory, &new);
                let mut old = 0;
                let guarded =
                    mem::VirtualProtect(memory.cast(), K64, mem::PAGE_READONLY, &mut old) != 0;
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x44).collect();
                let through = rig.store(buffer, 0, &new).is_ok() && same(memory, &new);
                unmapped = mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER) != 0;
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x55).collect();
                let stored = rig.store(buffer, 0, &new).is_ok();
                let other =
                    mem::MapViewOfFile(handle, mem::FILE_MAP_WRITE, 0, (5 * K64) as u32, K64);
                p.section_outlives_views =
                    unmapped && stored && !other.Value.is_null() && same(other.Value.cast(), &new);
                p.lines.push(format!(
                    "  the GPU reads the view's bytes {}; the CPU reads through the view what the GPU wrote {}; the view made read-only under the heap {guarded}, \
                     and the GPU writes it all the same {through}; the view unmapped under the heap {unmapped}, the GPU's write then shows in a view mapped afterwards {}",
                    p.section_reads, p.section_writes, p.section_outlives_views
                ));
                if !other.Value.is_null() {
                    mem::UnmapViewOfFile(other);
                }
            }
        }
        drop(opened);
        if !unmapped && !view.Value.is_null() {
            mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER);
        }
        windows_sys::Win32::Foundation::CloseHandle(handle);
        mem::VirtualFree(hole.cast(), 0, mem::MEM_RELEASE);
        mem::VirtualFree(hole.add(8 * K64).cast(), 0, mem::MEM_RELEASE);
    }
}

unsafe fn cost(p: &mut Probe, rig: &mut Rig, memory: &dyn Fn() -> Option<[u64; 4]>, mb: usize) {
    let bytes = mb << 20;
    let shared = || memory().map_or(0, |m| m[3] >> 20);
    let ms = |speed: Option<f64>| speed.map_or("?".to_owned(), |ms| format!("{ms:.1}"));
    unsafe {
        let base = mem::VirtualAlloc(
            std::ptr::null(),
            bytes,
            mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH,
            mem::PAGE_READWRITE,
        )
        .cast::<u8>();
        if !base.is_null() {
            let (set, account, started) = (working_set_mb(), shared(), std::time::Instant::now());
            let opened = open(&rig.device, From::Part(base as usize, bytes));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let placed = opened
                .as_ref()
                .ok()
                .and_then(|heap| place(&rig.device, heap, 0, bytes as u64).ok());
            let speed = placed
                .as_ref()
                .and_then(|buffer| rig.copy_ms(buffer, bytes as u64).ok());
            p.lines.push(format!(
                "cost, {mb} MB of private memory never touched: {}; opened in {took:.1} ms; working set {set} -> {} MB; shared with the card {account} -> {} MB; \
                 the card copies it out in {} ms",
                hr(&opened),
                working_set_mb(),
                shared(),
                ms(speed)
            ));
            drop((placed, opened));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let handle = mem::CreateFileMappingW(
            invalid,
            std::ptr::null(),
            mem::PAGE_READWRITE | mem::SEC_COMMIT,
            (bytes as u64 >> 32) as u32,
            bytes as u32,
            std::ptr::null(),
        );
        if !handle.is_null() {
            let (set, account, started) = (working_set_mb(), shared(), std::time::Instant::now());
            let opened = open(&rig.device, From::Section(handle as usize));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let (set_opened, account_opened) = (working_set_mb(), shared());
            let placed = opened
                .as_ref()
                .ok()
                .and_then(|heap| place(&rig.device, heap, 0, bytes as u64).ok());
            let speed = placed
                .as_ref()
                .and_then(|buffer| rig.copy_ms(buffer, bytes as u64).ok());
            p.lines.push(format!(
                "cost, {mb} MB of a committed section never touched: {}; opened in {took:.1} ms; working set {set} -> {set_opened} -> after the card read all of it {} MB; \
                 shared with the card {account} -> {account_opened} -> {} MB; the card copies it out in {} ms",
                hr(&opened),
                working_set_mb(),
                shared(),
                ms(speed)
            ));
            drop((placed, opened));
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let made = rig.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(bytes as u64, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        );
        if let (Ok(()), Some(buffer)) = (made, buffer) {
            let speed = rig.copy_ms(&buffer, bytes as u64).ok();
            p.lines.push(format!("cost, for comparison: the card copies {mb} MB out of a shared buffer of today's kind in {} ms", ms(speed)));
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Tally([(u64, u64); 6]);

const KINDS: [&str; 6] = [
    "private, committed whole (a part can be opened)",
    "view memory, one run of a section (one placed buffer)",
    "view memory, scattered pages",
    "view memory with unmapped pages",
    "private but not committed whole, or guarded",
    "other",
];

fn kind_of(at: usize, len: usize, known: &mut Vec<(usize, usize)>) -> usize {
    match cover(at, len) {
        Cover::Run(..) => return 1,
        Cover::Scattered => return 2,
        Cover::Holes => return 3,
        Cover::None => {}
    }
    let end = at + len;
    if known.iter().any(|&(from, to)| from <= at && end <= to) {
        return 0;
    }
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    if n == 0 {
        return 5;
    }
    let reach = (info.BaseAddress as usize).saturating_add(info.RegionSize);
    if info.Type == mem::MEM_PRIVATE
        && info.State == mem::MEM_COMMIT
        && info.Protect == mem::PAGE_READWRITE
    {
        known.push((info.BaseAddress as usize, reach));
        return if reach >= end { 0 } else { 4 };
    }
    match info.Type {
        mem::MEM_PRIVATE => 4,
        mem::MEM_MAPPED => 3,
        _ => 5,
    }
}

fn census_of(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let mut known = Vec::new();
    let (mut buffers, mut windows) = (Tally::default(), Tally::default());
    for &(start, len, window) in spans {
        let kind = kind_of(start as usize, len as usize, &mut known);
        let tally = if window { &mut windows } else { &mut buffers };
        tally.0[kind].0 += 1;
        tally.0[kind].1 += len;
    }
    let mut lines = Vec::new();
    for (name, tally) in [
        ("title buffers", buffers),
        ("windows outside buffers", windows),
    ] {
        let all = tally.0.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        lines.push(format!("census, {name}: {} of {} MB", all.0, all.1 >> 20));
        for (kind, &(n, bytes)) in KINDS.iter().zip(&tally.0) {
            if n > 0 {
                lines.push(format!("census,   {kind}: {n} of {} MB", bytes >> 20));
            }
        }
    }
    let v = views_read();
    let (count, bytes) = (
        v.views.len(),
        v.views.values().map(|view| view.0 as u64).sum::<u64>(),
    );
    let pages = v.views.values().filter(|view| view.0 == K64).count();
    let big = v.views.values().map(|view| view.0).max().unwrap_or(0);
    lines.push(format!(
        "census, the title's views of physical pages: {count} of {} MB; {pages} of them one 64 KiB page; the longest {} MB",
        bytes >> 20,
        big >> 20
    ));
    for &(bytes, committed) in v.sections.values() {
        lines.push(format!(
            "census, a section of {} MB, {}",
            bytes >> 20,
            if committed {
                "committed: it can be opened as one heap"
            } else {
                "reserved: refused as a heap until it is made with SEC_COMMIT"
            }
        ));
    }
    lines
}

fn real_part(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let Some(host) = crate::host::host() else {
        return Vec::new();
    };
    let mut known = Vec::new();
    let found = spans
        .iter()
        .filter(|s| !s.2 && s.1 >= 2 * K64 as u64)
        .filter(|s| kind_of(s.0 as usize, s.1 as usize, &mut known) == 0)
        .max_by_key(|s| s.1);
    let Some(&(start, len, _)) = found else {
        return vec![
            "the title's memory: no title buffer in private, committed memory to try".to_owned(),
        ];
    };
    let at = (start as usize).next_multiple_of(K64);
    let bytes = ((start + len) as usize).saturating_sub(at).min(4 << 20) & !(K64 - 1);
    if bytes == 0 || !private_whole(at, bytes) {
        return vec!["the title's memory: the buffer found holds no whole 64 KiB page that is still committed".to_owned()];
    }
    let mut lines = Vec::new();
    let (set, started) = (working_set_mb(), std::time::Instant::now());
    let opened = open(&host.device, From::Part(at, bytes));
    lines.push(format!(
        "the title's memory, {} KiB at {at:#x} (of the title buffer {start:#x}+{len:#x}): {}; in {:.1} ms; working set {set} -> {} MB",
        bytes >> 10,
        hr(&opened),
        started.elapsed().as_secs_f64() * 1000.0,
        working_set_mb()
    ));
    if let Ok(heap) = &opened {
        let placed = place(&host.device, heap, 0, bytes as u64);
        lines.push(format!("  a buffer placed over it: {}", hr(&placed)));
        if let (Ok(buffer), Ok(mut rig)) = (&placed, Rig::new(&host.device)) {
            let mut before = vec![0u8; K64];
            crate::host::copy_mapped(before.as_mut_ptr(), at as *const u8, K64, false);
            let got = rig.fetch(buffer, 0, K64);
            let mut after = vec![0u8; K64];
            crate::host::copy_mapped(after.as_mut_ptr(), at as *const u8, K64, false);
            match got {
                Ok(got) => {
                    let equal = (0..K64)
                        .filter(|&i| got[i] == before[i] || got[i] == after[i])
                        .count();
                    let zeros = got.iter().filter(|&&b| b == 0).count();
                    lines.push(format!(
                        "  the GPU read 64 KiB of the title's own bytes: {equal} of {K64} equal to what the CPU read just before or just after ({zeros} of them zero)"
                    ));
                }
                Err(e) => lines.push(format!("  the GPU's read failed: {e}")),
            }
        }
    }
    lines
}

pub(crate) fn tick() {
    if level() > 0 {
        say();
    }
    if probe_level() == 0 {
        return;
    }
    static STARTED: AtomicU64 = AtomicU64::new(0);
    static DONE: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms().max(1);
    let started = match STARTED.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => {
            let Some(host) = crate::host::host() else {
                return;
            };
            let mb = std::env::var("OATHKEEP_INPLACE_PROBE_MB")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(64usize);
            let took = std::time::Instant::now();
            let probe = probe(&host.device, &|| host.video_memory(), mb.min(1024));
            for line in &probe.lines {
                tracing::info!("in place probe: {line}");
            }
            tracing::info!(
                "in place probe: took {:.0} ms",
                took.elapsed().as_secs_f64() * 1000.0
            );
            now
        }
        Err(started) => started,
    };
    let due = match DONE.load(Ordering::Relaxed) {
        0 => now >= started + 30_000,
        1 => now >= started + 150_000,
        _ => false,
    };
    if due {
        let which = DONE.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let spans = crate::shadow::spans();
            for line in census_of(&spans) {
                tracing::info!("in place {line}");
            }
            if which == 0 && probe_level() >= 2 {
                for line in real_part(&spans) {
                    tracing::info!("in place probe: {line}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_say_what_one_placed_buffer_could_show() {
        let mut views = BTreeMap::new();
        let at = 0x7_0000_0000usize;
        views.insert(at, (4 * K64, 9, 16 * K64 as u64));
        views.insert(at + 4 * K64, (2 * K64, 9, 20 * K64 as u64));
        views.insert(at + 6 * K64, (K64, 9, 3 * K64 as u64));
        views.insert(at + 8 * K64, (K64, 9, 4 * K64 as u64));
        assert_eq!(
            cover_in(&views, at + K64, 2 * K64),
            Cover::Run(9, 17 * K64 as u64)
        );
        assert_eq!(
            cover_in(&views, at + K64, 5 * K64),
            Cover::Run(9, 17 * K64 as u64)
        );
        assert_eq!(cover_in(&views, at, 7 * K64), Cover::Scattered);
        assert_eq!(cover_in(&views, at + 6 * K64, 3 * K64), Cover::Holes);
        assert_eq!(cover_in(&views, at + 7 * K64, K64), Cover::None);
        assert_eq!(cover_in(&views, at - K64, 2 * K64), Cover::None);
    }

    #[test]
    fn remapped_memory_forgets_its_views() {
        let at = 0x7_4000_0000usize;
        title_memory_view(at, 4 * K64, 77, 0);
        title_memory_view(at + 4 * K64, K64, 77, 4 * K64 as u64);
        assert_eq!(cover(at, 5 * K64), Cover::Run(77, 0));
        views_gone(at + K64, K64);
        assert_eq!(cover(at, K64), Cover::None);
        assert_eq!(cover(at + 4 * K64, K64), Cover::Run(77, 4 * K64 as u64));
        views_gone(at + 4 * K64, K64);
        assert_eq!(cover(at + 4 * K64, K64), Cover::None);
    }

    #[test]
    fn only_private_memory_committed_whole_is_a_part() {
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(
                std::ptr::null(),
                8 * K64,
                mem::MEM_RESERVE,
                mem::PAGE_NOACCESS,
            )
            .cast::<u8>();
            assert!(!base.is_null());
            assert!(!private_whole(base as usize, K64), "reserved only");
            mem::VirtualAlloc(base.cast(), 4 * K64, mem::MEM_COMMIT, mem::PAGE_READWRITE);
            assert!(private_whole(base as usize, 4 * K64));
            assert!(private_whole(base as usize + K64, 2 * K64));
            assert!(
                !private_whole(base as usize, 5 * K64),
                "reaches into the reserved half"
            );
            let mut old = 0;
            mem::VirtualProtect(base.cast(), K64, mem::PAGE_READONLY, &mut old);
            assert!(!private_whole(base as usize, 2 * K64), "a guarded page");
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    pub(crate) fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::*;
        // SAFETY: plain DXGI queries and device creation on the software
        // adapter; a card is never opened.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                return None;
            }
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    #[test]
    fn warp_probe_opens_title_like_memory_and_the_gpu_sees_its_bytes() {
        let Some(device) = warp() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("no ID3D12Device13 here: parts of an allocation cannot be opened");
            return;
        }
        let probe = probe(&device, &|| None, 4);
        for line in &probe.lines {
            eprintln!("in place probe: {line}");
        }
        assert!(probe.part_reads, "the GPU reads private memory in place");
        assert!(probe.part_writes, "GPU writes land in private memory");
        assert!(
            probe.section_reads && probe.section_writes,
            "a committed section in place"
        );
        assert!(
            probe.section_outlives_views,
            "the section's heap does not depend on views"
        );
        assert!(
            probe.reserved_section_refused,
            "a reserved section is refused"
        );
    }

    #[test]
    fn warp_a_title_buffer_in_place_shows_its_bytes_at_its_offset() {
        let Some(device) = warp() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            return;
        }
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(
                std::ptr::null(),
                8 * K64,
                mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH,
                mem::PAGE_READWRITE,
            )
            .cast::<u8>();
            let (start, len) = (base as u64 + K64 as u64 + 0x1230, 2 * K64 as u64 + 5);
            let placed =
                place_part(&device, start, len).unwrap_or_else(|e| panic!("not placed: {e:?}"));
            assert_eq!(placed.bias, 0x1230);
            assert_eq!(placed.gpu, placed.buffer.GetGPUVirtualAddress() + 0x1230);
            assert_eq!(
                placed.buffer.GetDesc().Width,
                3 * K64 as u64,
                "the pages that hold the buffer"
            );
            let mut rig = Rig::new(&device).unwrap();
            fill(start as *mut u8, len as usize, 7);
            let got = rig
                .fetch(&placed.buffer, placed.bias, len as usize)
                .unwrap();
            assert!(
                same(start as *const u8, &got),
                "the GPU reads the title buffer's bytes"
            );
            rig.store(&placed.buffer, placed.bias + 16, &[0xa5; 64])
                .unwrap();
            assert!(
                same((start + 16) as *const u8, &[0xa5; 64]),
                "the GPU's write is at the title's address"
            );
            drop(placed);
            assert_ne!(
                mem::VirtualFree(base.add(2 * K64).cast(), K64, mem::MEM_DECOMMIT),
                0
            );
            assert!(matches!(place_part(&device, start, len), Err(None)));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    #[test]
    fn census_tells_private_memory_from_views() {
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(
                std::ptr::null(),
                8 * K64,
                mem::MEM_RESERVE | mem::MEM_COMMIT,
                mem::PAGE_READWRITE,
            ) as u64;
            let view = 0x7_8000_0000u64;
            title_memory_view(view as usize, 2 * K64, 5, 0);
            title_memory_view(view as usize + 2 * K64, K64, 5, 9 * K64 as u64);
            let lines = census_of(&[
                (base, 2 * K64 as u64, false),
                (base + 4 * K64 as u64, K64 as u64, true),
                (view, K64 as u64, true),
                (view, 3 * K64 as u64, false),
            ]);
            let text = lines.join("\n");
            assert!(text.contains("census, title buffers: 2 of"), "{text}");
            assert!(
                text.contains("census,   private, committed whole (a part can be opened): 1 of"),
                "{text}"
            );
            assert!(
                text.contains("census,   view memory, scattered pages: 1 of"),
                "{text}"
            );
            assert!(
                text.contains(
                    "census,   view memory, one run of a section (one placed buffer): 1 of"
                ),
                "{text}"
            );
            views_gone(view as usize, 3 * K64);
            mem::VirtualFree(base as *mut c_void, 0, mem::MEM_RELEASE);
        }
    }

    #[test]
    fn budget_reservation_enforces_both_target_and_total_caps() {
        let sections = AtomicU64::new(0);
        let parts = AtomicU64::new(0);
        let sec_cap = 2048 << 20;
        let total_cap = 3072 << 20;
        assert!(try_reserve_budget(
            &sections,
            &parts,
            1024 << 20,
            sec_cap,
            total_cap
        ));
        assert_eq!(sections.load(Ordering::Relaxed), 1024 << 20);
        assert!(try_reserve_budget(
            &sections,
            &parts,
            1024 << 20,
            sec_cap,
            total_cap
        ));
        assert!(
            !try_reserve_budget(&sections, &parts, 64 << 20, sec_cap, total_cap),
            "exceeds section-specific cap"
        );
        assert!(try_reserve_budget(
            &parts,
            &sections,
            1024 << 20,
            total_cap,
            total_cap
        ));
        assert!(
            !try_reserve_budget(&parts, &sections, 64 << 20, total_cap, total_cap),
            "exceeds combined total cap"
        );
        sections.fetch_sub(1024 << 20, Ordering::AcqRel);
        assert!(try_reserve_budget(
            &parts,
            &sections,
            1024 << 20,
            total_cap,
            total_cap
        ));
        assert_eq!(parts.load(Ordering::Relaxed), 2048 << 20);
    }

    #[test]
    fn section_registry_updates_and_clears_refused_handle_reuse() {
        let handle = 0xfeed_faceusize;
        title_memory_section(handle, 4 * K64 as u64, false);
        assert_eq!(section_info(handle), Some((4 * K64 as u64, false)));
        {
            let mut heaps = SECTION_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
            heaps
                .get_or_insert_with(Default::default)
                .insert(handle, SectionEntry::Refused);
        }
        // Re-registering the same handle value clears a stale Refused entry.
        title_memory_section(handle, 8 * K64 as u64, true);
        assert_eq!(section_info(handle), Some((8 * K64 as u64, true)));
        {
            let heaps = SECTION_HEAPS.lock().unwrap_or_else(|e| e.into_inner());
            assert!(heaps.as_ref().is_none_or(|m| !m.contains_key(&handle)));
        }
        title_memory_section_closing(handle);
        assert_eq!(section_info(handle), None);
    }

    #[test]
    fn overwriting_a_view_cleans_stale_page_entries() {
        let at = 0x7_6000_0000usize;
        let sec_a = 0xdead_0001usize;
        let sec_b = 0xdead_0002usize;
        title_memory_view(at, 2 * K64, sec_a, 0);
        assert_eq!(views_of_section(sec_a, 0, 2 * K64), [(at, 2 * K64)]);
        // Overwrite the same base with a smaller view into a different section.
        title_memory_view(at, K64, sec_b, 0);
        assert!(
            views_of_section(sec_a, 0, 2 * K64).is_empty(),
            "overwritten view must not leak stale page entries"
        );
        assert_eq!(views_of_section(sec_b, 0, K64), [(at, K64)]);
        views_gone(at, K64);
        assert!(views_of_section(sec_b, 0, K64).is_empty());
    }
}
