use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::host;

const TILE: u64 = 64 << 10;

const HEAP_TILES: u32 = 1024;

pub(crate) static STATS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

pub(crate) fn wanted() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if std::env::var_os("OATHKEEP_LEVELS_ON_DEMAND").is_some_and(|v| v == "0") {
            return false;
        }
        let Some(host) = host() else {
            return false;
        };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: a plain query into a struct of its own size.
        let asked = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        };
        let tier = options.TiledResourcesTier.0;
        let on = asked.is_ok() && tier >= 2;
        tracing::info!(
            tier,
            on,
            floor_kb = floor() >> 10,
            "host copies of title textures take the card's memory level by level (OATHKEEP_LEVELS_ON_DEMAND=0: off)"
        );
        on
    })
}

pub(crate) fn floor() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        std::env::var("OATHKEEP_LEVELS_MIN_KB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(128 << 10, |kb| kb.max(64) << 10)
    })
}

pub(crate) fn given_total() -> u64 {
    STATS[2].load(Ordering::Relaxed)
}

struct Heap {
    heap: ID3D12Heap,
    free: Vec<(u32, u32)>,
}

// SAFETY: a heap of the device, named only in queue and device calls, which
// any thread may make; its free runs are plain numbers behind `POOL`'s lock.
unsafe impl Send for Heap {}

impl Heap {
    fn free_tiles(&self) -> u32 {
        self.free.iter().map(|run| run.1).sum()
    }
}

static POOL: Mutex<Vec<Option<Heap>>> = Mutex::new(Vec::new());

fn take(device: &ID3D12Device, count: u32) -> Option<(ID3D12Heap, usize, Vec<(u32, u32)>)> {
    if count == 0 || count > HEAP_TILES {
        return None;
    }
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let found = pool
        .iter()
        .position(|h| h.as_ref().is_some_and(|h| h.free_tiles() >= count));
    let index = match found {
        Some(index) => index,
        None => {
            let desc = D3D12_HEAP_DESC {
                SizeInBytes: u64::from(HEAP_TILES) * TILE,
                Properties: D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
            };
            let mut made: Option<ID3D12Heap> = None;
            // SAFETY: plain creation; the out pointer is ours.
            if let Err(e) =
                unsafe { device.CreateHeap::<ID3D12Heap>(&raw const desc, &raw mut made) }
            {
                tracing::warn!(error = %e, "no heap for texture levels given on demand: those levels stay empty");
                return None;
            }
            let fresh = Heap {
                heap: made?,
                free: vec![(0, HEAP_TILES)],
            };
            STATS[3].fetch_add(u64::from(HEAP_TILES) * TILE, Ordering::Relaxed);
            match pool.iter().position(Option::is_none) {
                Some(index) => {
                    pool[index] = Some(fresh);
                    index
                }
                None => {
                    pool.push(Some(fresh));
                    pool.len() - 1
                }
            }
        }
    };
    let h = pool[index].as_mut()?;
    let mut runs = Vec::new();
    let mut left = count;
    while left > 0 {
        let &(start, tiles) = h.free.first()?;
        let part = tiles.min(left);
        runs.push((start, part));
        if part == tiles {
            h.free.remove(0);
        } else {
            h.free[0] = (start + part, tiles - part);
        }
        left -= part;
    }
    Some((h.heap.clone(), index, runs))
}

fn give_back(index: usize, runs: &[(u32, u32)]) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(h)) = pool.get_mut(index) else {
        return;
    };
    h.free.extend_from_slice(runs);
    h.free.sort_unstable();
    let mut joined: Vec<(u32, u32)> = Vec::with_capacity(h.free.len());
    for &(from, tiles) in &h.free {
        match joined.last_mut() {
            Some(last) if last.0 + last.1 == from => last.1 += tiles,
            _ => joined.push((from, tiles)),
        }
    }
    h.free = joined;
    let all_free = h.free_tiles() == HEAP_TILES;
    if all_free && pool.iter().flatten().count() > 1 {
        pool[index] = None;
        STATS[3].fetch_sub(u64::from(HEAP_TILES) * TILE, Ordering::Relaxed);
        STATS[8].fetch_add(1, Ordering::Relaxed);
    }
}

struct Given {
    levels: Vec<bool>,
    packed: bool,
    runs: Vec<(usize, u32, u32)>,
}

pub(crate) struct Tiled {
    standard: Vec<u32>,
    packed: u32,
    levels: u32,
    pub(crate) full: u64,
    given: Mutex<Given>,
    shown: AtomicU32,
    held: AtomicU32,
    card: AtomicU32,
    claimed: AtomicU32,
}

impl Tiled {
    pub(crate) fn make(
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        levels: u32,
    ) -> Option<Tiled> {
        if levels == 0 || levels > 16 {
            return None;
        }
        let mut total = 0u32;
        let mut packed = D3D12_PACKED_MIP_INFO::default();
        let mut shape = D3D12_TILE_SHAPE::default();
        let mut count = levels;
        let mut tilings = vec![D3D12_SUBRESOURCE_TILING::default(); levels as usize];
        // SAFETY: outputs of our own, `count` tilings of them.
        unsafe {
            device.GetResourceTiling(
                texture,
                Some(&raw mut total),
                Some(&raw mut packed),
                Some(&raw mut shape),
                Some(&raw mut count),
                0,
                tilings.as_mut_ptr(),
            );
        }
        let standard_levels = u32::from(packed.NumStandardMips);
        if total == 0
            || standard_levels + u32::from(packed.NumPackedMips) != levels
            || standard_levels > count
        {
            return None;
        }
        let mut standard = Vec::with_capacity(standard_levels as usize);
        for tiling in &tilings[..standard_levels as usize] {
            let tiles = tiling.WidthInTiles
                * u32::from(tiling.HeightInTiles)
                * u32::from(tiling.DepthInTiles);
            if tiles == 0
                || tiles > HEAP_TILES
                || tiling.StartTileIndexInOverallResource == D3D12_PACKED_TILE
            {
                return None;
            }
            standard.push(tiles);
        }
        let packed_tiles = if packed.NumPackedMips > 0 {
            packed.NumTilesForPackedMips
        } else {
            0
        };
        if packed_tiles > HEAP_TILES {
            return None;
        }
        let full = u64::from(total) * TILE;
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(full, Ordering::Relaxed);
        STATS[4].fetch_add(1, Ordering::Relaxed);
        Some(Tiled {
            given: Mutex::new(Given {
                levels: vec![false; standard.len()],
                packed: false,
                runs: Vec::new(),
            }),
            standard,
            packed: packed_tiles,
            levels,
            full,
            shown: AtomicU32::new(u32::MAX),
            held: AtomicU32::new(0),
            card: AtomicU32::new(0),
            claimed: AtomicU32::new(0),
        })
    }

    pub(crate) fn uploaded(&self, texture: &ID3D12Resource, held: u32, room: bool) -> (bool, bool) {
        match host() {
            Some(host) => self.uploaded_on(host.queue(), &host.device, texture, held, room),
            None => (false, false),
        }
    }

    pub(crate) fn uploaded_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        held: u32,
        room: bool,
    ) -> (bool, bool) {
        self.held.store(held, Ordering::Relaxed);
        let card = self.card.load(Ordering::Relaxed);
        let named = held | card;
        let changed = self.give_on(
            queue,
            device,
            texture,
            named,
            if room { named } else { card },
        );
        (changed, self.without(named) != 0)
    }

    pub(crate) fn card_wrote(&self, texture: &ID3D12Resource, levels: u32) -> bool {
        match host() {
            Some(host) => self.card_wrote_on(host.queue(), &host.device, texture, levels),
            None => false,
        }
    }

    pub(crate) fn card_wrote_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        levels: u32,
    ) -> bool {
        let card = self.card.fetch_or(levels, Ordering::Relaxed) | levels;
        let changed = self.give_on(
            queue,
            device,
            texture,
            card | self.held.load(Ordering::Relaxed),
            levels,
        );
        let without = self.without(levels);
        if without != 0 {
            self.unclaim(without);
        }
        changed
    }

    pub(crate) fn with_level(&self, level: u32) -> u32 {
        let first_packed = self.standard.len() as u32;
        if level < first_packed {
            1 << level
        } else if level < self.levels {
            self.every_level() & !((1u32 << first_packed) - 1)
        } else {
            0
        }
    }

    pub(crate) fn claim(&self, levels: u32) -> u32 {
        let fresh = self.without(levels);
        fresh & !self.claimed.fetch_or(fresh, Ordering::Relaxed)
    }

    pub(crate) fn unclaim(&self, levels: u32) {
        self.claimed.fetch_and(!levels, Ordering::Relaxed);
    }

    pub(crate) fn without(&self, levels: u32) -> u32 {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        let first_packed = self.standard.len() as u32;
        let mut without = 0;
        for level in 0..self.levels.min(32) {
            let has = if level < first_packed {
                given.levels[level as usize]
            } else {
                given.packed
            };
            if !has {
                without |= 1 << level;
            }
        }
        levels & without
    }

    pub(crate) fn give_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        named: u32,
        may: u32,
    ) -> bool {
        let has = |level: u32| level < 32 && named & (1 << level) != 0;
        let now = |level: u32| level < 32 && named & may & (1 << level) != 0;
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        let first_packed = self.standard.len() as u32;
        let mut wanted: Vec<(u32, u32, Option<usize>)> = Vec::new();
        for level in 0..first_packed {
            if !given.levels[level as usize] && now(level) {
                wanted.push((level, self.standard[level as usize], Some(level as usize)));
            }
        }
        if self.packed > 0 && !given.packed && (first_packed..self.levels).any(now) {
            wanted.push((first_packed, self.packed, None));
        }
        for (subresource, tiles, level) in wanted {
            let Some((heap, index, runs)) = take(device, tiles) else {
                continue;
            };
            let at = D3D12_TILED_RESOURCE_COORDINATE {
                X: 0,
                Y: 0,
                Z: 0,
                Subresource: subresource,
            };
            let size = D3D12_TILE_REGION_SIZE {
                NumTiles: tiles,
                ..Default::default()
            };
            let flags = vec![D3D12_TILE_RANGE_FLAG_NONE; runs.len()];
            let starts: Vec<u32> = runs.iter().map(|run| run.0).collect();
            let counts: Vec<u32> = runs.iter().map(|run| run.1).collect();
            let began = std::time::Instant::now();
            // SAFETY: a live reserved texture and a live heap of ours; one
            // region of `tiles` tiles from the subresource's first tile;
            // `runs.len()` ranges whose counts add up to `tiles`, in three
            // arrays of that length that live through the call.
            unsafe {
                queue.UpdateTileMappings(
                    texture,
                    1,
                    Some(&raw const at),
                    Some(&raw const size),
                    &heap,
                    runs.len() as u32,
                    Some(flags.as_ptr()),
                    Some(starts.as_ptr()),
                    Some(counts.as_ptr()),
                    D3D12_TILE_MAPPING_FLAG_NONE,
                );
            }
            STATS[6].fetch_add(1, Ordering::Relaxed);
            STATS[7].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
            match level {
                Some(level) => given.levels[level] = true,
                None => given.packed = true,
            }
            for &(start, count) in &runs {
                given.runs.push((index, start, count));
            }
            STATS[2].fetch_add(u64::from(tiles) * TILE, Ordering::Relaxed);
            STATS[5].fetch_add(1, Ordering::Relaxed);
        }
        let with_tiles = |level: u32| {
            if level < first_packed {
                given.levels[level as usize]
            } else {
                given.packed
            }
        };
        let shown = (0..self.levels)
            .find(|&level| has(level) && with_tiles(level))
            .unwrap_or(u32::MAX);
        self.shown.swap(shown, Ordering::Relaxed) != shown
    }

    pub(crate) fn first_shown(&self) -> Option<u32> {
        let shown = self.shown.load(Ordering::Relaxed);
        (shown != u32::MAX).then_some(shown)
    }

    #[cfg(test)]
    fn has_tiles(&self, level: u32) -> bool {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        match given.levels.get(level as usize) {
            Some(&has) => has,
            None => level < self.levels && given.packed,
        }
    }

    #[cfg(test)]
    fn whole(&self) -> bool {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        given.levels.iter().all(|&has| has) && (self.packed == 0 || given.packed)
    }

    pub(crate) fn every_level(&self) -> u32 {
        if self.levels >= 32 {
            u32::MAX
        } else {
            (1 << self.levels) - 1
        }
    }

    pub(crate) fn given_bytes(&self) -> u64 {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        given.runs.iter().map(|run| u64::from(run.2) * TILE).sum()
    }
}

impl Drop for Tiled {
    fn drop(&mut self) {
        let given = self.given.get_mut().unwrap_or_else(|e| e.into_inner());
        let mut bytes = 0;
        let mut by_heap: std::collections::BTreeMap<usize, Vec<(u32, u32)>> =
            std::collections::BTreeMap::new();
        for &(index, start, tiles) in &given.runs {
            by_heap.entry(index).or_default().push((start, tiles));
            bytes += u64::from(tiles) * TILE;
        }
        for (index, runs) in by_heap {
            give_back(index, &runs);
        }
        STATS[0].fetch_sub(1, Ordering::Relaxed);
        STATS[1].fetch_sub(self.full, Ordering::Relaxed);
        STATS[2].fetch_sub(bytes, Ordering::Relaxed);
    }
}

pub(crate) fn show_from(desc: &mut D3D12_SHADER_RESOURCE_VIEW_DESC, first: u32, levels: u32) {
    if desc.ViewDimension != D3D12_SRV_DIMENSION_TEXTURE2D || first >= levels {
        return;
    }
    // SAFETY: a 2D view's member of the union.
    let view = unsafe { &mut desc.Anonymous.Texture2D };
    if first <= view.MostDetailedMip {
        return;
    }
    let fewer = first - view.MostDetailedMip;
    view.MostDetailedMip = first;
    if view.MipLevels != u32::MAX {
        view.MipLevels = view.MipLevels.saturating_sub(fewer).max(1);
    }
}

pub(crate) fn status() {
    static BEFORE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    if s[4] == 0 {
        return;
    }
    let calls = s[6] - BEFORE[0].swap(s[6], Ordering::Relaxed);
    let micros = s[7] - BEFORE[1].swap(s[7], Ordering::Relaxed);
    tracing::info!(
        "textures with levels on demand: {} alive ({} made so far), {} MB of the card given to {} levels in all, \
         {} MB of the card not allocated for levels the title does not hold ({} MB if every level had memory); \
         the pool's heaps {} MB ({} given back to the card so far); since the line before: {} calls of \
         UpdateTileMappings in {} microseconds; in all {} calls, {} microseconds; {} uploads left a held \
         level without memory (no room on the card, or no heap), {} uploads were recorded again for one",
        s[0],
        s[4],
        s[2] >> 20,
        s[5],
        s[1].saturating_sub(s[2]) >> 20,
        s[1] >> 20,
        s[3] >> 20,
        s[8],
        calls,
        micros,
        s[6],
        s[7],
        s[9],
        s[10],
    );
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::{Interface, s};

    use super::*;
    use crate::host::{compile, transition, warp_adapter};

    const SIDE: u32 = 1024;
    const LEVELS: u16 = 11;

    const LOOK: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors = 2)), UAV(u0), StaticSampler(s0, filter = FILTER_MIN_MAG_MIP_POINT)"
Texture2D<float4> clamped : register(t0);
Texture2D<float4> whole : register(t1);
SamplerState nearest : register(s0);
RWByteAddressBuffer words : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void look() {
    float4 a = clamped.SampleLevel(nearest, float2(0.3, 0.3), 0);
    float4 b = whole.SampleLevel(nearest, float2(0.3, 0.3), 0);
    words.Store(0, (uint)(a.x * 255.0 + 0.5));
    words.Store(4, (uint)(b.x * 255.0 + 0.5));
}
"#;

    struct Laid {
        footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
        rows: Vec<u32>,
        wide: Vec<u64>,
        total: u64,
    }

    struct Rig {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        event: HANDLE,
        submitted: u64,
        messages: Option<ID3D12InfoQueue>,
        step: &'static str,
        card: bool,
    }

    impl Rig {
        fn new() -> Option<Rig> {
            // SAFETY: plain object creation on a device of our own.
            unsafe {
                let card = std::env::var_os("OATHKEEP_TILES_TEST_CARD").is_some_and(|v| v == "1");
                let mut layer: Option<ID3D12Debug> = None;
                let layered = D3D12GetDebugInterface(&mut layer).is_ok() && layer.is_some();
                layer.iter().for_each(|l| l.EnableDebugLayer());
                if card && !layered {
                    eprintln!(
                        "no D3D12 debug layer (Windows optional feature \"Graphics Tools\"): not run on the card without it"
                    );
                    return None;
                }
                let mut device: Option<ID3D12Device> = None;
                if card {
                    D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                } else {
                    let adapter = warp_adapter().ok()?;
                    D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                }
                let device = device?;
                let messages = device.cast::<ID3D12InfoQueue>().ok();
                let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
                device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .ok()?;
                eprintln!(
                    "levels on demand, {}: tiled resources tier {}, debug layer {}",
                    if card {
                        "ON THE CARD"
                    } else {
                        "on the software adapter"
                    },
                    options.TiledResourcesTier.0,
                    if messages.is_some() {
                        "on"
                    } else {
                        "not installed"
                    }
                );
                if options.TiledResourcesTier.0 < 2 {
                    return None;
                }
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .ok()?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                Some(Rig {
                    device,
                    queue,
                    allocator,
                    list,
                    fence,
                    event,
                    submitted: 0,
                    messages,
                    step: "making the device",
                    card,
                })
            }
        }

        fn said(&self) -> Option<String> {
            let queue = self.messages.as_ref()?;
            let mut first_error = None;
            // SAFETY: each message is read into a buffer of the size the
            // queue reports, then the queue is emptied.
            unsafe {
                for i in 0..queue.GetNumStoredMessages() {
                    let mut size = 0;
                    if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                        continue;
                    }
                    let mut buffer = vec![0u64; size.div_ceil(8)];
                    let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                    if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                        let m = &*message;
                        let text =
                            std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                        let text = String::from_utf8_lossy(text);
                        let text = text.trim_end_matches('\0');
                        eprintln!(
                            "D3D12 says (severity {}, id {}): {text}",
                            m.Severity.0, m.ID.0
                        );
                        if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                            first_error = Some(text.to_owned());
                        }
                    }
                }
                queue.ClearStoredMessages();
            }
            first_error
        }

        fn check(&self, what: &str) {
            let error = self.said();
            // SAFETY: a plain query.
            let removed = unsafe { self.device.GetDeviceRemovedReason() };
            assert!(
                removed.is_ok(),
                "{}, {what}: the device is lost: {removed:?}",
                self.step
            );
            assert!(
                error.is_none(),
                "{}, {what}: the debug layer reports an error: {}",
                self.step,
                error.unwrap_or_default()
            );
        }

        fn give(&self, tiled: &Tiled, texture: &ID3D12Resource, levels: u32) {
            tiled.give_on(&self.queue, &self.device, texture, levels, levels);
            self.check("tiles given");
        }

        fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
            record(&self.list);
            self.submitted += 1;
            // SAFETY: our list, queue and fence; the allocator is reset
            // only after the card finished everything recorded from it.
            unsafe {
                self.list.Close().unwrap();
                let run: ID3D12CommandList = self.list.cast().unwrap();
                self.queue.ExecuteCommandLists(&[Some(run)]);
                self.queue.Signal(&self.fence, self.submitted).unwrap();
                self.fence
                    .SetEventOnCompletion(self.submitted, self.event)
                    .unwrap();
                let limit = if self.card { 1500 } else { 20_000 };
                let waited = WaitForSingleObject(self.event, limit);
                self.check("a submission");
                assert!(
                    waited == WAIT_OBJECT_0,
                    "{}: a submission did not finish within {limit} ms; stopped before another is sent",
                    self.step
                );
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }

        fn buffer(
            &self,
            kind: D3D12_HEAP_TYPE,
            flags: D3D12_RESOURCE_FLAGS,
            state: D3D12_RESOURCE_STATES,
            bytes: u64,
        ) -> ID3D12Resource {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes,
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
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: plain buffer creation.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut out,
                    )
                    .unwrap();
            }
            out.unwrap()
        }

        fn desc(
            format: DXGI_FORMAT,
            side: u32,
            levels: u16,
            reserved: bool,
        ) -> D3D12_RESOURCE_DESC {
            D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(side),
                Height: side,
                DepthOrArraySize: 1,
                MipLevels: levels,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: if reserved {
                    D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE
                } else {
                    D3D12_TEXTURE_LAYOUT_UNKNOWN
                },
                ..Default::default()
            }
        }

        fn reserved(&self, desc: &D3D12_RESOURCE_DESC) -> (ID3D12Resource, Tiled) {
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation; a texture that is no target has no clear value.
            let asked = unsafe {
                self.device.CreateReservedResource(
                    desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut made,
                )
            };
            self.check("a texture without memory made");
            asked.expect("a reserved texture");
            let texture = made.expect("a reserved texture");
            let tiled = Tiled::make(&self.device, &texture, u32::from(desc.MipLevels))
                .expect("the card says how it cuts the texture");
            (texture, tiled)
        }

        fn footprints(&self, desc: &D3D12_RESOURCE_DESC) -> Laid {
            let levels = usize::from(desc.MipLevels);
            let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); levels];
            let mut rows = vec![0u32; levels];
            let mut wide = vec![0u64; levels];
            let mut total = 0u64;
            // SAFETY: outputs for `levels` subresources.
            unsafe {
                self.device.GetCopyableFootprints(
                    desc,
                    0,
                    levels as u32,
                    0,
                    Some(footprints.as_mut_ptr()),
                    Some(rows.as_mut_ptr()),
                    Some(wide.as_mut_ptr()),
                    Some(&mut total),
                );
            }
            Laid {
                footprints,
                rows,
                wide,
                total,
            }
        }

        fn filled(&self, layout: &Laid, byte: impl Fn(usize) -> u8) -> ID3D12Resource {
            let (footprints, rows) = (&layout.footprints, &layout.rows);
            let upload = self.buffer(
                D3D12_HEAP_TYPE_UPLOAD,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                layout.total,
            );
            // SAFETY: the buffer is `total` bytes; each level's rows lie inside it.
            unsafe {
                let mut data = std::ptr::null_mut();
                upload.Map(0, None, Some(&mut data)).unwrap();
                for (level, f) in footprints.iter().enumerate() {
                    let len = rows[level] as usize * f.Footprint.RowPitch as usize;
                    std::slice::from_raw_parts_mut(data.cast::<u8>().add(f.Offset as usize), len)
                        .fill(byte(level));
                }
                upload.Unmap(0, None);
            }
            upload
        }

        fn round_trip(
            &mut self,
            texture: &ID3D12Resource,
            layout: &Laid,
            upload: Option<&ID3D12Resource>,
        ) -> Vec<[Vec<u8>; 2]> {
            let (footprints, rows) = (&layout.footprints, &layout.rows);
            let back = self.buffer(
                D3D12_HEAP_TYPE_READBACK,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
                layout.total,
            );
            self.run(|list| {
                // SAFETY: an open list; the texture and buffers outlive the run.
                unsafe {
                    if let Some(upload) = upload {
                        for (level, f) in footprints.iter().enumerate() {
                            list.CopyTextureRegion(
                                &sub(texture, level as u32),
                                0,
                                0,
                                0,
                                &placed(upload, *f),
                                None,
                            );
                        }
                    }
                    list.ResourceBarrier(&[transition(
                        texture,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                    )]);
                    for (level, f) in footprints.iter().enumerate() {
                        list.CopyTextureRegion(
                            &placed(&back, *f),
                            0,
                            0,
                            0,
                            &sub(texture, level as u32),
                            None,
                        );
                    }
                    list.ResourceBarrier(&[transition(
                        texture,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    )]);
                }
            });
            let mut read = Vec::new();
            // SAFETY: a read-back buffer of `total` bytes the finished list copied into.
            unsafe {
                let mut data = std::ptr::null_mut();
                back.Map(0, None, Some(&mut data)).unwrap();
                for (level, f) in footprints.iter().enumerate() {
                    let pitch = f.Footprint.RowPitch as usize;
                    let len = rows[level] as usize * pitch;
                    let bytes = std::slice::from_raw_parts(
                        data.cast::<u8>().cast_const().add(f.Offset as usize),
                        len,
                    );
                    let own = layout.wide[level] as usize;
                    let wide = own.min(len).min(16);
                    let first = bytes[..wide].to_vec();
                    let middle = (rows[level] as usize / 2) * pitch + ((own / 2) & !15);
                    let far = if rows[level] >= 2 && own >= 32 && middle + wide <= len {
                        bytes[middle..middle + wide].to_vec()
                    } else {
                        first.clone()
                    };
                    read.push([first, far]);
                }
                back.Unmap(0, None);
            }
            read
        }
    }

    fn sub(texture: &ID3D12Resource, index: u32) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: index,
            },
        }
    }

    fn placed(
        buffer: &ID3D12Resource,
        footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    ) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        }
    }

    fn all(bytes: &[Vec<u8>; 2], want: u8) -> bool {
        bytes.iter().all(|part| part.iter().all(|&b| b == want))
    }

    fn from_level(first: u32) -> u32 {
        (u32::MAX << first) & ((1 << u32::from(LEVELS)) - 1)
    }

    fn levels_arrive(rig: &mut Rig, format: DXGI_FORMAT) {
        rig.step = if format == DXGI_FORMAT_BC7_UNORM {
            "step 1, levels arrive (BC7)"
        } else {
            "step 1, levels arrive (BC3)"
        };
        let desc = Rig::desc(format, SIDE, LEVELS, true);
        let (texture, tiled) = rig.reserved(&desc);
        eprintln!(
            "format {}: {} levels with tiles of their own ({:?} tiles), {} tiles for the packed ones",
            format.0,
            tiled.standard.len(),
            tiled.standard,
            tiled.packed
        );
        assert!(
            !rig.card || tiled.packed > 0,
            "on the card a 1024 texture has levels smaller than a tile"
        );
        let layout = rig.footprints(&desc);
        let upload = rig.filled(&layout, |level| level as u8 + 1);
        for first in [3u32, 1, 0] {
            rig.give(&tiled, &texture, from_level(first));
            let read = rig.round_trip(&texture, &layout, Some(&upload));
            for (level, bytes) in read.iter().enumerate() {
                let want = if tiled.has_tiles(level as u32) {
                    level as u8 + 1
                } else {
                    0
                };
                assert!(
                    all(bytes, want),
                    "format {} from level {first}: level {level} reads {bytes:?}, wanted {want}",
                    format.0
                );
            }
        }
        assert!(tiled.whole());
        assert_eq!(tiled.first_shown(), Some(0));
    }

    fn a_card_copy_lands_after_the_give(rig: &mut Rig) {
        rig.step = "step 2, a copy on the card into a level without tiles";
        let format = DXGI_FORMAT_BC7_UNORM;
        let desc = Rig::desc(format, SIDE, LEVELS, true);
        let (texture, tiled) = rig.reserved(&desc);
        let layout = rig.footprints(&desc);
        let level = tiled.standard.len() as u32 - 1;
        let source_desc = Rig::desc(format, SIDE >> level, 1, false);
        let source_layout = rig.footprints(&source_desc);
        let source_bytes = rig.filled(&source_layout, |_| 0x5a);
        let mut made: Option<ID3D12Resource> = None;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        // SAFETY: plain texture creation.
        unsafe {
            rig.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &source_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut made,
                )
                .unwrap();
        }
        let source = made.unwrap();
        rig.run(|list| {
            // SAFETY: an open list and live resources.
            unsafe {
                list.CopyTextureRegion(
                    &sub(&source, 0),
                    0,
                    0,
                    0,
                    &placed(&source_bytes, source_layout.footprints[0]),
                    None,
                );
                list.ResourceBarrier(&[transition(
                    &source,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.CopyTextureRegion(&sub(&texture, level), 0, 0, 0, &sub(&source, 0), None);
            }
        });
        let before = rig.round_trip(&texture, &layout, None);
        assert!(
            all(&before[level as usize], 0),
            "a copy into a level without tiles left {:?}",
            before[level as usize]
        );
        assert_ne!(tiled.claim(tiled.with_level(level)), 0, "the level is new");
        rig.give(&tiled, &texture, tiled.with_level(level));
        rig.run(|list| {
            // SAFETY: as above.
            unsafe {
                list.CopyTextureRegion(&sub(&texture, level), 0, 0, 0, &sub(&source, 0), None)
            };
        });
        let after = rig.round_trip(&texture, &layout, None);
        assert!(
            all(&after[level as usize], 0x5a),
            "the copy after the give left {:?}",
            after[level as usize]
        );
        assert_eq!(tiled.claim(tiled.with_level(level)), 0, "claimed once");
    }

    fn a_reused_tile_reads_as_written(rig: &mut Rig) {
        rig.step = "step 3, a reused tile";
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let layout = rig.footprints(&desc);
        let (first, first_tiles) = rig.reserved(&desc);
        let level = first_tiles.standard.len() as u32 - 1;
        rig.give(&first_tiles, &first, 1 << level);
        let upload = rig.filled(&layout, |_| 0xa5);
        let read = rig.round_trip(&first, &layout, Some(&upload));
        assert!(all(&read[level as usize], 0xa5));
        let runs = first_tiles.given.lock().unwrap().runs.clone();
        drop(first_tiles);
        drop(first);
        let (second, second_tiles) = rig.reserved(&desc);
        rig.give(&second_tiles, &second, 1 << level);
        let reused = second_tiles.given.lock().unwrap().runs == runs;
        let unwritten = rig.round_trip(&second, &layout, None);
        eprintln!(
            "a level given tiles and not written reads {:?} (its tiles are the first texture's: {reused})",
            &unwritten[level as usize][0][..4]
        );
        let upload = rig.filled(&layout, |_| 0x3c);
        let read = rig.round_trip(&second, &layout, Some(&upload));
        assert!(
            all(&read[level as usize], 0x3c),
            "the reused level reads {:?}",
            read[level as usize]
        );
        assert!(
            all(&read[0], 0),
            "level 0 has no tiles and reads {:?}",
            read[0]
        );
    }

    fn a_level_on_several_ranges(rig: &mut Rig) {
        rig.step = "step 4, a level on several ranges of the heap";
        let singles: Vec<_> = (0..3)
            .map(|_| take(&rig.device, 1).expect("a tile"))
            .collect();
        give_back(singles[0].1, &singles[0].2);
        give_back(singles[2].1, &singles[2].2);
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let layout = rig.footprints(&desc);
        let (texture, tiled) = rig.reserved(&desc);
        assert!(tiled.standard.len() > 1 && tiled.standard[1] >= 2);
        rig.give(&tiled, &texture, 1 << 1);
        let runs = tiled.given.lock().unwrap().runs.len();
        eprintln!(
            "level 1 ({} tiles) was mapped on {runs} ranges",
            tiled.standard[1]
        );
        assert!(runs >= 2, "the single free tiles were not used first");
        let upload = rig.filled(&layout, |_| 0x77);
        let read = rig.round_trip(&texture, &layout, Some(&upload));
        assert!(all(&read[1], 0x77), "level 1 reads {:?}", read[1]);
        give_back(singles[1].1, &singles[1].2);
    }

    fn the_view_starts_at_the_first_level_with_tiles(rig: &mut Rig) -> Option<()> {
        rig.step = "step 5, the view from the first level with tiles";
        let cs = compile(LOOK, s!("look"), s!("cs_5_1")).ok()?;
        let format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let desc = Rig::desc(format, SIDE, LEVELS, true);
        let layout = rig.footprints(&desc);
        let (texture, tiled) = rig.reserved(&desc);
        rig.give(&tiled, &texture, from_level(1));
        assert_eq!(tiled.first_shown(), Some(1));
        let upload = rig.filled(&layout, |level| (level as u8 + 1) * 16);
        rig.round_trip(&texture, &layout, Some(&upload));
        // SAFETY: plain object creation; the views are made in a heap
        // that lives to the end of the function, of a live texture.
        let (root, pipeline, views, words, back) = unsafe {
            let root: ID3D12RootSignature = rig.device.CreateRootSignature(0, &cs[..]).ok()?;
            let pipeline: ID3D12PipelineState = rig
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: cs.as_ptr().cast(),
                        BytecodeLength: cs.len(),
                    },
                    ..Default::default()
                })
                .ok()?;
            let views: ID3D12DescriptorHeap = rig
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            let as_asked = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
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
            let mut shown = as_asked;
            show_from(&mut shown, tiled.first_shown()?, u32::from(LEVELS));
            assert_eq!(shown.Anonymous.Texture2D.MostDetailedMip, 1);
            let step = rig
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let mut at = views.GetCPUDescriptorHandleForHeapStart();
            rig.device
                .CreateShaderResourceView(&texture, Some(&raw const shown), at);
            at.ptr += step;
            rig.device
                .CreateShaderResourceView(&texture, Some(&raw const as_asked), at);
            let words = rig.buffer(
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                256,
            );
            let back = rig.buffer(
                D3D12_HEAP_TYPE_READBACK,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
                256,
            );
            (root, pipeline, views, words, back)
        };
        rig.check("the pipeline and the views made");
        rig.run(|list| {
            // SAFETY: an open list and live objects.
            unsafe {
                list.ResourceBarrier(&[transition(
                    &texture,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                )]);
                list.SetDescriptorHeaps(&[Some(views.clone())]);
                list.SetComputeRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetComputeRootDescriptorTable(0, views.GetGPUDescriptorHandleForHeapStart());
                list.SetComputeRootUnorderedAccessView(1, words.GetGPUVirtualAddress());
                list.Dispatch(1, 1, 1);
                list.ResourceBarrier(&[transition(
                    &words,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.CopyBufferRegion(&back, 0, &words, 0, 16);
            }
        });
        // SAFETY: a read-back buffer the finished list copied sixteen bytes into.
        let read = unsafe {
            let mut data = std::ptr::null_mut();
            let range = D3D12_RANGE { Begin: 0, End: 16 };
            back.Map(0, Some(&range), Some(&mut data)).unwrap();
            let read = std::slice::from_raw_parts(data.cast::<u32>(), 2).to_vec();
            back.Unmap(0, None);
            read
        };
        eprintln!(
            "a sample at level 0: {} through the view from level 1, {} through the view as asked",
            read[0], read[1]
        );
        assert_eq!(
            read[0], 32,
            "the view from the first level with tiles reads level 1"
        );
        assert_eq!(
            read[1], 0,
            "the view from level 0 reads a level without tiles"
        );
        Some(())
    }

    fn a_full_card_keeps_new_levels_waiting(rig: &mut Rig) {
        rig.step =
            "step 6, no room on the card: held levels wait, and get their tiles when asked again";
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let (texture, tiled) = rig.reserved(&desc);
        let layout = rig.footprints(&desc);
        let upload = rig.filled(&layout, |level| level as u8 + 1);
        let read_as_given = |rig: &mut Rig, what: &str| {
            let read = rig.round_trip(&texture, &layout, Some(&upload));
            for (level, bytes) in read.iter().enumerate() {
                let want = if tiled.has_tiles(level as u32) {
                    level as u8 + 1
                } else {
                    0
                };
                assert!(
                    all(bytes, want),
                    "{what}: level {level} reads {bytes:?}, wanted {want}"
                );
            }
        };
        let (changed, waiting) =
            tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(3), false);
        rig.check("an upload without room");
        assert!(
            !changed && waiting,
            "without room: changed {changed}, waiting {waiting}"
        );
        assert_eq!(tiled.given_bytes(), 0, "without room no tile is given");
        assert_eq!(tiled.first_shown(), None);
        read_as_given(rig, "without room");
        let (changed, waiting) =
            tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(3), true);
        rig.check("the upload again, with room");
        assert!(
            changed && !waiting,
            "with room: changed {changed}, waiting {waiting}"
        );
        assert!(tiled.has_tiles(3) && !tiled.has_tiles(2));
        assert_eq!(tiled.first_shown(), Some(3));
        read_as_given(rig, "with room");
        let before = tiled.given_bytes();
        let (changed, waiting) =
            tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(1), false);
        rig.check("more levels held, without room");
        assert!(
            !changed && waiting,
            "more levels without room: changed {changed}, waiting {waiting}"
        );
        assert_eq!(tiled.given_bytes(), before);
        assert_eq!(tiled.first_shown(), Some(3));
        read_as_given(rig, "more levels without room");
        assert_eq!(tiled.claim(1 << 0), 1 << 0);
        let changed = tiled.card_wrote_on(&rig.queue, &rig.device, &texture, 1 << 0);
        rig.check("the card's own write, without room");
        assert!(changed && tiled.has_tiles(0) && !tiled.has_tiles(1) && !tiled.has_tiles(2));
        assert_eq!(tiled.first_shown(), Some(0));
        assert_eq!(
            tiled.claim(1 << 0),
            0,
            "a level with tiles is not claimed again"
        );
        let (_, waiting) =
            tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(1), true);
        rig.check("the last upload, with room");
        assert!(!waiting && tiled.whole());
        read_as_given(rig, "everything given");
    }

    fn a_claim_given_up_is_made_again(rig: &mut Rig) {
        rig.step = "step 7, a claim whose zeros never landed is given up";
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let (_texture, tiled) = rig.reserved(&desc);
        let level = tiled.with_level(2);
        assert_eq!(tiled.claim(level), level, "the level is new");
        assert_eq!(tiled.claim(level), 0, "claimed once");
        tiled.unclaim(level);
        assert_eq!(
            tiled.claim(level),
            level,
            "claimed again after it was given up"
        );
        let small = tiled.with_level(u32::from(LEVELS) - 1);
        assert!(
            !rig.card || small.count_ones() > 1,
            "on the card a 1024 texture has several levels in one tile"
        );
        assert_eq!(
            tiled.claim(1 << (u32::from(LEVELS) - 1)),
            1 << (u32::from(LEVELS) - 1)
        );
        tiled.unclaim(small);
        assert_eq!(tiled.claim(small), small);
    }

    #[test]
    #[ignore = "on the card: run it by name with --ignored --nocapture"]
    fn levels_on_demand_on_the_card() {
        let Some(mut rig) = Rig::new() else {
            eprintln!(
                "no adapter with tiled resources of tier 2, or no debug layer for a run on the card; skipped"
            );
            return;
        };
        rig.check("the device made");
        levels_arrive(&mut rig, DXGI_FORMAT_BC7_UNORM);
        levels_arrive(&mut rig, DXGI_FORMAT_BC3_UNORM);
        a_card_copy_lands_after_the_give(&mut rig);
        a_reused_tile_reads_as_written(&mut rig);
        a_level_on_several_ranges(&mut rig);
        a_full_card_keeps_new_levels_waiting(&mut rig);
        a_claim_given_up_is_made_again(&mut rig);
        if the_view_starts_at_the_first_level_with_tiles(&mut rig).is_none() {
            eprintln!("the view test's shader or objects could not be made; that part was skipped");
        }
        rig.step = "the end";
        rig.check("every step done");
        eprintln!(
            "all steps passed {}",
            if rig.card {
                "ON THE CARD"
            } else {
                "on the software adapter; OATHKEEP_TILES_TEST_CARD=1 is the run on the card"
            }
        );
        let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
        eprintln!(
            "{} calls of UpdateTileMappings took {} microseconds in all ({} a call); the pool's heaps {} MB, {} given back",
            s[6],
            s[7],
            s[7] / s[6].max(1),
            s[3] >> 20,
            s[8]
        );
    }
}
