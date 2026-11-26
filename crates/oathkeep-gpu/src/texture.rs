use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

#[derive(Default)]
struct UsizeHasher(u64);

impl Hasher for UsizeHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.wrapping_mul(0x517c_c1b7_2722_0a95) ^ u64::from(b);
        }
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        let x = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.0 = x ^ (x >> 32);
    }
}

#[derive(Clone, Copy, Default)]
struct UsizeBuildHasher;

impl BuildHasher for UsizeBuildHasher {
    type Hasher = UsizeHasher;

    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        UsizeHasher(0)
    }
}

type UsizeMap<V> = HashMap<usize, V, UsizeBuildHasher>;
type UsizeSet = HashSet<usize, UsizeBuildHasher>;

/// Fast 4-lane unrolled 128-bit streaming hash for same-texture change detection
/// and slot fingerprinting. Covers 100% of every input byte with identical output
/// across direct-read and copy-fallback paths.
struct FastHasher {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    total: u64,
}

impl FastHasher {
    const K0: u64 = 0x9e37_79b9_7f4a_7c15;
    const K1: u64 = 0xbf58_476d_1ce4_e5b9;
    const K2: u64 = 0x94d0_49bb_1331_11eb;
    const K3: u64 = 0xd6e8_feb8_6659_fd93;

    #[inline]
    fn new() -> Self {
        Self {
            v0: Self::K0,
            v1: Self::K1,
            v2: Self::K2,
            v3: Self::K3,
            total: 0,
        }
    }

    #[inline]
    fn mix64(mut x: u64) -> u64 {
        x ^= x >> 30;
        x = x.wrapping_mul(Self::K1);
        x ^= x >> 27;
        x = x.wrapping_mul(Self::K2);
        x ^ (x >> 31)
    }

    #[inline]
    fn update(&mut self, bytes: &[u8]) {
        self.total = self.total.wrapping_add(bytes.len() as u64);
        let (mut v0, mut v1, mut v2, mut v3) = (self.v0, self.v1, self.v2, self.v3);
        let chunks = bytes.chunks_exact(32);
        let rem = chunks.remainder();
        for chunk in chunks {
            let a = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
            let b = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
            let c = u64::from_le_bytes(chunk[16..24].try_into().unwrap());
            let d = u64::from_le_bytes(chunk[24..32].try_into().unwrap());
            v0 = (v0 ^ a.wrapping_mul(Self::K1))
                .rotate_left(27)
                .wrapping_mul(Self::K0);
            v1 = (v1 ^ b.wrapping_mul(Self::K2))
                .rotate_left(31)
                .wrapping_mul(Self::K1);
            v2 = (v2 ^ c.wrapping_mul(Self::K3))
                .rotate_left(29)
                .wrapping_mul(Self::K2);
            v3 = (v3 ^ d.wrapping_mul(Self::K0))
                .rotate_left(33)
                .wrapping_mul(Self::K3);
        }
        let words = rem.chunks_exact(8);
        let tail = words.remainder();
        for (i, w) in words.enumerate() {
            let x = u64::from_le_bytes(w.try_into().unwrap());
            match i {
                0 => {
                    v0 = (v0 ^ x.wrapping_mul(Self::K1))
                        .rotate_left(27)
                        .wrapping_mul(Self::K0)
                }
                1 => {
                    v1 = (v1 ^ x.wrapping_mul(Self::K2))
                        .rotate_left(31)
                        .wrapping_mul(Self::K1)
                }
                _ => {
                    v2 = (v2 ^ x.wrapping_mul(Self::K3))
                        .rotate_left(29)
                        .wrapping_mul(Self::K2)
                }
            }
        }
        if !tail.is_empty() {
            let mut last = [0u8; 8];
            last[..tail.len()].copy_from_slice(tail);
            let x = u64::from_le_bytes(last);
            v3 = (v3 ^ x.wrapping_mul(Self::K0))
                .rotate_left(33)
                .wrapping_mul(Self::K3);
        }
        // Domain-separate each chunk by its byte length.
        v1 = (v1 ^ (bytes.len() as u64).wrapping_mul(Self::K3))
            .rotate_left(19)
            .wrapping_mul(Self::K0);
        (self.v0, self.v1, self.v2, self.v3) = (v0, v1, v2, v3);
    }

    #[inline]
    fn finalize128(self) -> [u64; 2] {
        let t = self.total.wrapping_mul(Self::K0);
        let h0 = Self::mix64(
            self.v0
                ^ self.v2.rotate_left(23)
                ^ t
                ^ self.v1.wrapping_mul(Self::K3),
        );
        let h1 = Self::mix64(
            self.v1
                ^ self.v3.rotate_left(41)
                ^ t.rotate_left(17)
                ^ self.v0.wrapping_mul(Self::K2),
        );
        [h0, h1]
    }

    #[inline]
    fn finalize64(self) -> u64 {
        let [h0, h1] = self.finalize128();
        (h0 ^ h1.rotate_left(17)) | 1
    }

    #[inline]
    fn hash128(bytes: &[u8]) -> [u64; 2] {
        let mut h = Self::new();
        h.update(bytes);
        h.finalize128()
    }
}

#[inline]
fn is_all_zero(bytes: &[u8]) -> bool {
    let chunks = bytes.chunks_exact(32);
    let rem = chunks.remainder();
    for chunk in chunks {
        let a = u64::from_ne_bytes(chunk[0..8].try_into().unwrap());
        let b = u64::from_ne_bytes(chunk[8..16].try_into().unwrap());
        let c = u64::from_ne_bytes(chunk[16..24].try_into().unwrap());
        let d = u64::from_ne_bytes(chunk[24..32].try_into().unwrap());
        if (a | b | c | d) != 0 {
            return false;
        }
    }
    let words = rem.chunks_exact(8);
    let tail = words.remainder();
    for w in words {
        if u64::from_ne_bytes(w.try_into().unwrap()) != 0 {
            return false;
        }
    }
    tail.iter().all(|&b| b == 0)
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

pub(crate) struct Staging {
    buffer: Option<ID3D12Resource>,
    capacity: u64,
    #[cfg(feature = "diagnostics")]
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: oathkeep_formats::texture::Layout,
    mips: u32,
    bytes: u64,
    memory: *const u8,
    last: Mutex<Last>,
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    used: std::sync::atomic::AtomicU64,
    on_demand: bool,
    detiled: std::sync::atomic::AtomicU32,
    zeroed_mips: std::sync::atomic::AtomicU32,
    watch_mode: std::sync::atomic::AtomicU8,
    judged: [std::sync::atomic::AtomicU64; 2],
    waiting: std::sync::atomic::AtomicU64,
    exact: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Last {
    hash: Option<[u64; 2]>,
    asked: u64,
    partial: Option<u64>,
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: RwLock<Option<UsizeMap<Arc<Staging>>>> = RwLock::new(None);
static STAGING_COUNT: AtomicUsize = AtomicUsize::new(0);

struct Through {
    buffer: ID3D12Resource,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}

const ALIAS_PARTS: u32 = 54;

// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

static THROUGH: RwLock<Option<UsizeMap<Arc<Through>>>> = RwLock::new(None);
static THROUGH_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    let (Some(to), Some(from)) = (
        crate::resource::host_resource(placed),
        crate::resource::host_resource(source),
    ) else {
        return false;
    };
    let Some(host) = host() else { return false };
    let placed_key = placed as usize;
    let existing = if THROUGH_COUNT.load(Ordering::Relaxed) != 0 {
        THROUGH
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|map| map.get(&placed_key).cloned())
    } else {
        None
    };
    let through = match existing {
        Some(through) => through,
        None => {
            // Build outside the global lock so D3D12 queries and buffer creation
            // never stall other recording threads.
            // SAFETY: live textures.
            let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
            let count = |d: &D3D12_RESOURCE_DESC| {
                let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                    1
                } else {
                    u32::from(d.DepthOrArraySize)
                };
                u32::from(d.MipLevels) * layers
            };
            let parts = count(&of_from);
            if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                return false;
            }
            let mut rows = [(); 2]
                .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
            let mut bytes = [0u64; 2];
            // SAFETY: plain queries; the out pointers are ours and
            // hold `parts` layouts each.
            unsafe {
                host.device.GetCopyableFootprints(
                    &of_from,
                    0,
                    parts,
                    0,
                    Some(rows[0].as_mut_ptr()),
                    None,
                    None,
                    Some(&mut bytes[0]),
                );
                host.device.GetCopyableFootprints(
                    &of_to,
                    0,
                    parts,
                    0,
                    Some(rows[1].as_mut_ptr()),
                    None,
                    None,
                    Some(&mut bytes[1]),
                );
            }
            let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                let (fa, fb) = (a.Footprint, b.Footprint);
                a.Offset == b.Offset
                    && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                        == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
            });
            if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                return false;
            }
            let Some(buffer) = host.uav_buffer(bytes[0]) else {
                return false;
            };
            crate::host::name(&buffer, || {
                format!(
                    "alias copy {}x{} f{} to f{}",
                    of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                )
            });
            tracing::info!(
                view = ?placed,
                texture = ?source,
                width = of_from.Width,
                height = of_from.Height,
                parts,
                written_as = of_from.Format.0,
                read_as = of_to.Format.0,
                "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
            );
            let [of_written, of_read] = rows;
            let built = Arc::new(Through {
                buffer,
                parts: of_written.into_iter().zip(of_read).collect(),
            });
            let (chosen, surplus) = {
                let mut map = THROUGH.write().unwrap_or_else(|e| e.into_inner());
                let map = map.get_or_insert_with(UsizeMap::default);
                match map.entry(placed_key) {
                    std::collections::hash_map::Entry::Occupied(e) => {
                        (e.get().clone(), Some(built))
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(built.clone());
                        THROUGH_COUNT.store(map.len(), Ordering::Relaxed);
                        (built, None)
                    }
                }
            };
            drop(surplus);
            chosen
        }
    };
    let first = list.first_use(placed as usize);
    let before = list
        .transition(source, COPY_SOURCE)
        .map(|(texture, before)| {
            let state = crate::host::legal_state(texture, COPY_SOURCE);
            let barrier = crate::host::transition(
                texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(state as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { list.list.ResourceBarrier(&[barrier]) };
            before
        });
    if !first && before.is_none() {
        return true;
    }
    card_writes(list, placed, to, None);
    crate::list::ensure_state(list, placed, COPY_DEST);
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&through.buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, read, written) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
    // SAFETY: an open list and live resources; the buffer is as big as
    // the footprints and rests in the common state between copies.
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, common, written)]);
        for (index, (of_written, _)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, written, read)]);
        for (index, (_, of_read)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, read, common)]);
    }
    list.keep(&through.buffer);
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state(list, source, before);
    }
    true
}

fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_NO_WAIT").is_none_or(|v| v != "0"))
}

pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    REFRESH_WAITS[slot].fetch_add(
        started.elapsed().as_micros() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    out
}

thread_local! {
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < size {
            read.resize(size, 0);
        }
        let out = f(&mut read[..size]);
        if read.capacity() > (16 << 20) {
            read.clear();
            read.shrink_to(4 << 20);
        }
        out
    })
}

fn through_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_COPY").is_some_and(|v| v == "1"))
}

fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
        f(read)
    })
}

static STAGED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

const STAGING_POOL_MAX_BUF: u64 = 16 << 20;

struct PooledStaging {
    buffer: ID3D12Resource,
    data: usize,
    capacity: u64,
}

// SAFETY: D3D12 upload buffers and their persistent mappings are free-threaded
// once no command list references them.
unsafe impl Send for PooledStaging {}

struct StagingPool {
    kept: Vec<PooledStaging>,
    bytes: u64,
}

static STAGING_POOL: Mutex<StagingPool> = Mutex::new(StagingPool {
    kept: Vec::new(),
    bytes: 0,
});

fn staging_pool_max_bytes() -> u64 {
    static MAX: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var("OATHKEEP_STAGING_POOL_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(64)
            << 20
    })
}

fn staging_bucket(size: u64) -> u64 {
    if size <= STAGING_POOL_MAX_BUF {
        size.max(64 << 10).next_power_of_two().min(STAGING_POOL_MAX_BUF)
    } else {
        size
    }
}

fn take_staging_buffer(
    host: &crate::host::Host,
    total: u64,
) -> Option<(ID3D12Resource, *mut u8, u64, bool)> {
    let bucket = staging_bucket(total);
    if bucket <= STAGING_POOL_MAX_BUF && staging_pool_max_bytes() > 0 {
        let mut pool = STAGING_POOL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = pool.kept.iter().rposition(|b| b.capacity == bucket) {
            let item = pool.kept.swap_remove(at);
            pool.bytes -= item.capacity;
            return Some((item.buffer, item.data as *mut u8, item.capacity, false));
        }
    }
    let (buffer, data) = host.upload_buffer(bucket)?;
    Some((buffer, data, bucket, true))
}

fn give_staging_buffer(buffer: ID3D12Resource, data: usize, capacity: u64) {
    let max_bytes = staging_pool_max_bytes();
    if capacity <= STAGING_POOL_MAX_BUF && max_bytes > 0 {
        let mut pool = STAGING_POOL.lock().unwrap_or_else(|e| e.into_inner());
        if pool.bytes + capacity <= max_bytes {
            pool.bytes += capacity;
            pool.kept.push(PooledStaging {
                buffer,
                data,
                capacity,
            });
            return;
        }
    }
    crate::host::defer_drop(crate::host::SendDrop(buffer));
}

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
        if let Some(buffer) = self.buffer.take() {
            give_staging_buffer(buffer, self.data as usize, self.capacity);
        }
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 3] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING_COUNT.load(Relaxed) as u64;
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
}

static LET_GO: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

pub(crate) fn let_go_idle() {
    let_go_idle_with(false);
}

pub(crate) fn let_go_idle_urgent() {
    let_go_idle_with(true);
}

fn let_go_idle_with(urgent: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    let avail_phys = crate::host::available_physical_ram();
    let avail_commit = crate::host::available_commit();
    if urgent
        || avail_phys.is_some_and(|b| b < (2 << 30))
        || avail_commit.is_some_and(|c| c < (2560u64 << 20))
    {
        let drained = if urgent {
            STAGING_POOL.try_lock().ok().map(|mut pool| {
                pool.bytes = 0;
                std::mem::take(&mut pool.kept)
            })
        } else {
            let mut pool = STAGING_POOL.lock().unwrap_or_else(|e| e.into_inner());
            pool.bytes = 0;
            Some(std::mem::take(&mut pool.kept))
        };
        if let Some(drained) = drained
            && !drained.is_empty()
        {
            if urgent {
                drop(drained);
            } else {
                crate::host::defer_drop(crate::host::SendDrop(drained));
            }
        }
    }
    if STAGING_COUNT.load(Relaxed) == 0 {
        return;
    }
    let (idle_ms, idle_frames) = if urgent {
        (0, 1)
    } else {
        (
            crate::host::ram_idle_ms(avail_phys).min(crate::host::commit_idle_ms(avail_commit)),
            crate::host::commit_idle_frames(avail_commit).min(IDLE_FRAMES),
        )
    };
    let mut evicted: Vec<Arc<Staging>> = Vec::new();
    let (mut went, mut bytes) = (0u64, 0u64);
    {
        let mut map = if urgent {
            let Ok(guard) = STAGING.try_write() else {
                return;
            };
            guard
        } else {
            STAGING.write().unwrap_or_else(|e| e.into_inner())
        };
        let Some(map) = map.as_mut() else {
            return;
        };
        if map.is_empty() {
            return;
        }
        map.retain(|_, s| {
            let in_flight = s
                .buffer
                .as_ref()
                .is_some_and(|b| crate::host::references(b) > 1);
            let keep = Arc::strong_count(s) > 1
                || in_flight
                || !crate::host::long_ago(s.used.load(Relaxed), idle_ms, idle_frames);
            if !keep {
                LET_GO[0].fetch_add(1, Relaxed);
                LET_GO[1].fetch_add(s.bytes, Relaxed);
                went += 1;
                bytes += s.bytes;
                evicted.push(s.clone());
            }
            keep
        });
        STAGING_COUNT.store(map.len(), Relaxed);
    }
    // Drop evicted staging entries after releasing `STAGING` so `Staging::drop`
    // does not nest `STAGING_POOL` under `STAGING`.
    drop(evicted);
    if went > 0 {
        crate::host::trail(|| {
            format!(
                "sweep: {went} staging buffers of textures let go ({} KiB), held by no list",
                bytes >> 10
            )
        });
    }
}

static HOST_WRITTEN: RwLock<Option<UsizeSet>> = RwLock::new(None);
static HOST_WRITTEN_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn host_written(resource: This) {
    let key = resource as usize;
    if HOST_WRITTEN_COUNT.load(Ordering::Relaxed) != 0
        && HOST_WRITTEN
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|set| set.contains(&key))
    {
        return;
    }
    let new = {
        let mut set = HOST_WRITTEN.write().unwrap_or_else(|e| e.into_inner());
        let set = set.get_or_insert_with(UsizeSet::default);
        let inserted = set.insert(key);
        if inserted {
            HOST_WRITTEN_COUNT.store(set.len(), Ordering::Relaxed);
        }
        inserted
    };
    if new && let Some((at, desc)) = crate::resource::texture_memory(resource) {
        // SAFETY: 56 bytes in the description's layout.
        let words = unsafe {
            std::ptr::from_ref(&desc)
                .cast::<[u32; 14]>()
                .read_unaligned()
        };
        let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
        if !at.is_null() && bytes > 0 {
            let mut map = HOST_WRITTEN_AT.write().unwrap_or_else(|e| e.into_inner());
            map.insert(
                at as u64,
                (bytes, words[4], words[6], words[7], words[8], key),
            );
            HOST_WRITTEN_AT_COUNT.store(map.len(), Ordering::Relaxed);
        }
    }
}

static HOST_WRITTEN_AT: RwLock<std::collections::BTreeMap<u64, (u64, u32, u32, u32, u32, usize)>> =
    RwLock::new(std::collections::BTreeMap::new());
static HOST_WRITTEN_AT_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn host_written_match(address: u64, desc: &[u32; 14]) -> Option<This> {
    if HOST_WRITTEN_AT_COUNT.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let map = HOST_WRITTEN_AT.read().unwrap_or_else(|e| e.into_inner());
    let &(_, width, height, layers_levels, format, key) = map.get(&address)?;
    if width == desc[4]
        && height == desc[6]
        && layers_levels == desc[7]
        && crate::resource::family(format) == crate::resource::family(desc[8])
    {
        Some(key as This)
    } else {
        None
    }
}

pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    if HOST_WRITTEN_AT_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let hit = {
        let map = HOST_WRITTEN_AT.read().unwrap_or_else(|e| e.into_inner());
        map.range(..=address)
            .next_back()
            .filter(|(at, d)| address < **at + d.0)
            .map(|(at, d)| (*at, *d))
    };
    if let Some((at, (bytes, width, height, layers_levels, format, _))) = hit {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                view_address = format_args!("{address:#x}"),
                view_offset = address - at,
                view_width = desc[4],
                view_height = desc[6],
                view_layers_levels = format_args!("{:#x}", desc[7]),
                view_format = desc[8],
                copied_address = format_args!("{at:#x}"),
                copied_bytes = bytes,
                copied_width = width,
                copied_height = height,
                copied_layers_levels = format_args!("{layers_levels:#x}"),
                copied_format = format,
                "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
            );
        }
    }
}

pub(crate) fn host_wrote(resource: usize) -> bool {
    if HOST_WRITTEN_COUNT.load(Ordering::Relaxed) == 0 {
        return false;
    }
    HOST_WRITTEN
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

pub(crate) fn forget(resource: usize) {
    if STAGING_COUNT.load(Ordering::Relaxed) != 0 {
        let present = STAGING
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|map| map.contains_key(&resource));
        if present {
            let removed = {
                let mut map = STAGING.write().unwrap_or_else(|e| e.into_inner());
                map.as_mut().and_then(|map| {
                    let v = map.remove(&resource);
                    STAGING_COUNT.store(map.len(), Ordering::Relaxed);
                    v
                })
            };
            drop(removed);
        }
    }
    let mut was_host_written = false;
    if HOST_WRITTEN_COUNT.load(Ordering::Relaxed) != 0 {
        let present = HOST_WRITTEN
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|set| set.contains(&resource));
        if present {
            let mut set = HOST_WRITTEN.write().unwrap_or_else(|e| e.into_inner());
            if let Some(set) = set.as_mut() {
                was_host_written = set.remove(&resource);
                HOST_WRITTEN_COUNT.store(set.len(), Ordering::Relaxed);
            }
        }
    }
    if was_host_written && HOST_WRITTEN_AT_COUNT.load(Ordering::Relaxed) != 0 {
        let mut map = HOST_WRITTEN_AT.write().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, d| d.5 != resource);
        HOST_WRITTEN_AT_COUNT.store(map.len(), Ordering::Relaxed);
    }
    if THROUGH_COUNT.load(Ordering::Relaxed) != 0 {
        let present = THROUGH
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|through| through.contains_key(&resource));
        if present {
            let removed = {
                let mut through = THROUGH.write().unwrap_or_else(|e| e.into_inner());
                through.as_mut().and_then(|through| {
                    let v = through.remove(&resource);
                    THROUGH_COUNT.store(through.len(), Ordering::Relaxed);
                    v
                })
            };
            drop(removed);
        }
    }
}

pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

thread_local! {
    pub(crate) static BAKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    if host_wrote(resource as usize) || !list.first_use(resource as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    let baking = BAKING.get();
    if baking {
        staging.exact.store(true, Relaxed);
    }
    staging.used.store(crate::host::moment(), Relaxed);
    if staging.on_demand && staging.judgement() != staging.judged() {
        staging.forget();
        staging.dirty.store(true, Relaxed);
    }
    if staging.on_demand {
        let since = staging.waiting.load(Relaxed);
        if since != 0 && crate::host::long_ago(since, 1000, 2) {
            if crate::resource::room_lately() {
                staging.waiting.store(0, Relaxed);
                staging.dirty.store(true, Relaxed);
                crate::tiles::STATS[10].fetch_add(1, Relaxed);
            } else {
                staging.waiting.store(crate::host::moment().max(1), Relaxed);
            }
        }
    }
    if !baking && staging.uploaded.load(Relaxed) && !staging.dirty.load(Relaxed) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexSkipped, 1);
        list.defer_texture(staging);
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::TexUploads, 1);
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    let Some(staging_buffer) = staging.buffer.as_ref() else {
        return;
    };
    #[cfg(feature = "diagnostics")]
    // SAFETY: live staging buffer.
    let staging_gpu = unsafe { staging_buffer.GetGPUVirtualAddress() };
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    // SAFETY: live texture.
                    let d = unsafe { texture.GetDesc() };
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {staging_gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (staging_gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(staging_buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    let on_demand = if staging.on_demand {
        crate::resource::host_tiled(resource)
    } else {
        None
    };
    let after = staging.clone();
    list.defer_texture(staging);
    match on_demand {
        None => {
            list.defer_after_textures(move || {
                after.uploaded.store(true, Relaxed);
                after.dirty.store(false, Relaxed);
            });
        }
        Some(on_demand) => {
            list.defer_after_textures(move || {
                after.uploaded.store(true, Relaxed);
                after.dirty.store(false, Relaxed);
                let waiting = on_demand.uploaded(after.detiled.load(Relaxed), after.memory as u64);
                after.waiting.store(
                    if waiting {
                        crate::host::moment().max(1)
                    } else {
                        0
                    },
                    Relaxed,
                );
                if waiting {
                    crate::descriptor::host::levels_waiting(after.memory as u64);
                }
            });
        }
    }
}

struct Claimed {
    on_demand: crate::resource::OnDemand,
    fresh: u32,
    ran: std::sync::atomic::AtomicBool,
}

impl Drop for Claimed {
    fn drop(&mut self) {
        if self.fresh != 0
            && !*self.ran.get_mut()
            && let Some(tiled) = self.on_demand.tiled()
        {
            tiled.unclaim(self.fresh);
        }
    }
}

pub(crate) fn card_writes(
    list: &HostList,
    resource: This,
    texture: &ID3D12Resource,
    level: Option<u32>,
) {
    let Some(on_demand) = crate::resource::host_tiled(resource) else {
        return;
    };
    let Some(tiled) = on_demand.tiled() else {
        return;
    };
    let levels = match level {
        Some(level) => tiled.with_level(level),
        None => tiled.every_level(),
    };
    if levels == 0 {
        return;
    }
    let fresh = tiled.claim(levels);
    let mut unzeroed = if host().is_none() { fresh } else { 0 };
    if fresh != 0
        && let Some(host) = host()
    {
        const COPY_DEST: u32 = 0x400;
        crate::list::ensure_state(list, resource, COPY_DEST);
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        for level in
            (0..u32::from(d.MipLevels).min(32)).filter(|&level| fresh & (1u32 << level) != 0)
        {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut bytes = 0u64;
            // SAFETY: outputs for one subresource.
            unsafe {
                host.device.GetCopyableFootprints(
                    &d,
                    level,
                    1,
                    0,
                    Some(&mut footprint),
                    None,
                    None,
                    Some(&mut bytes),
                );
            }
            let Some((buffer, data)) = list.upload(host, bytes) else {
                unzeroed |= tiled.with_level(level);
                continue;
            };
            // SAFETY: a mapped buffer of `bytes` bytes, just made.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: level,
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above; the list holds the buffer until it is reset.
                pResource: unsafe { std::mem::transmute_copy(&buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprint,
                },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
    unzeroed &= fresh;
    if unzeroed != 0 {
        tiled.unclaim(unzeroed);
    }
    let levels = levels & !unzeroed;
    if levels == 0 {
        return;
    }
    let memory = crate::resource::texture_memory(resource).map_or(0, |(memory, _)| memory as u64);
    let step = Claimed {
        on_demand,
        fresh: fresh & !unzeroed,
        ran: std::sync::atomic::AtomicBool::new(false),
    };
    list.defer_host(move || {
        step.ran.store(true, std::sync::atomic::Ordering::Relaxed);
        step.on_demand.card_wrote(levels, memory);
    });
}

impl Staging {
    fn judgement(&self) -> [u64; 2] {
        let start = self.memory as u64;
        let first = crate::descriptor::host::first_viewed(start).map_or(0, u64::from);
        let limit =
            crate::descriptor::host::next_start(start, start.saturating_add(self.layout.size))
                .map_or(u64::MAX, |at| at - start);
        [first, limit]
    }

    fn judged(&self) -> [u64; 2] {
        use std::sync::atomic::Ordering::Relaxed;
        [self.judged[0].load(Relaxed), self.judged[1].load(Relaxed)]
    }

    fn held_levels(&self, [first, limit]: [u64; 2]) -> u32 {
        let mips = &self.layout.mips;
        if self.layout.slices != 1 || mips.is_empty() || mips.len() > 32 {
            return u32::MAX;
        }
        let tail = (self.layout.first_tail_mip as usize).min(mips.len());
        let use_query = by_query();
        let mut last_span: Option<(usize, usize, bool)> = None;
        let mut there = |from: u64, to: u64| {
            if to <= from || to > limit {
                return false;
            }
            let addr = (self.memory as usize).wrapping_add(from as usize);
            let len = (to - from) as usize;
            if !use_query {
                return committed(addr as *const u8, len);
            }
            let end = addr.saturating_add(len);
            if let Some((base, span_end, is_commit)) = last_span
                && addr >= base
                && end <= span_end
            {
                return is_commit;
            }
            let mut p = addr;
            while p < end {
                let Some((base, span_end, is_commit)) = query_region_live(p) else {
                    return false;
                };
                last_span = Some((base, span_end, is_commit));
                if !is_commit {
                    return false;
                }
                p = span_end;
            }
            true
        };
        let mut held = 0u32;
        for (level, m) in mips.iter().enumerate().take(tail) {
            let end = if level == 0 {
                self.layout.slice_size
            } else {
                mips[level - 1].offset
            };
            if level as u64 >= first && there(m.offset, end) {
                held |= 1 << level;
            }
        }
        if tail < mips.len() && there(0, self.layout.alignment.min(self.layout.slice_size)) {
            for level in tail..mips.len() {
                if level as u64 >= first {
                    held |= 1 << level;
                }
            }
        }
        held
    }

    fn forget(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Last::default();
    }

    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.layout.size as usize)
    }

    pub(crate) fn watch_range(&self) -> Option<(usize, usize)> {
        let mode = self.watch_mode.load(std::sync::atomic::Ordering::Relaxed);
        if mode <= 1 && self.layout.size > 0 {
            let start = self.memory as usize;
            Some((start, start.saturating_add(self.layout.size as usize)))
        } else {
            None
        }
    }

    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed();
        REFRESH_WAITS[3].fetch_add(
            whole.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        read
    }

    fn refresh_timed(&self) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        use std::sync::atomic::Ordering::Relaxed;
        let exact = self.exact.swap(false, Relaxed);
        let mode = self.watch_mode.load(Relaxed);
        if mode == 0 || mode == 1 {
            if let Some((now, written)) =
                waited(1, || crate::watch::writes(start, start + size, false))
            {
                if mode == 0 {
                    self.watch_mode.store(1, Relaxed);
                }
                if !exact && last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                    return 0;
                }
                last.asked = now;
                with_read(size, |read| {
                    crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                    self.detile(read);
                });
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
                return size;
            }
            if mode == 1 {
                self.watch_mode.store(0, Relaxed);
            }
        }
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if (mode == 0 || mode == 2)
            && *GUARD
                .get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_GUARD").is_none_or(|v| v != "0"))
        {
            if let Some((now, written)) = waited(1, || {
                crate::watch::writes_guarded_since(start, start + size, last.asked)
            }) {
                if mode == 0 {
                    self.watch_mode.store(2, Relaxed);
                }
                static HASH_EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
                let hash_every = *HASH_EVERY.get_or_init(|| {
                    std::env::var("OATHKEEP_TEXTURE_HASH_EVERY")
                        .ok()
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map_or(32, |n| n.max(1))
                });
                let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
                last.asked = now;
                let turn = crate::frame::current()
                    .wrapping_add((start >> 16) as u64)
                    .is_multiple_of(hash_every);
                if !exact && unwritten && !turn {
                    return 0;
                }
            } else if mode == 0 {
                self.watch_mode.store(3, Relaxed);
            }
        }
        let _mapped = if !exact && no_wait() {
            let Some(mapped) = crate::watch::title_memory_reading_now() else {
                return self.refresh_unlocked(last, size);
            };
            mapped
        } else {
            waited(2, crate::watch::title_memory_reading)
        };
        if !committed(self.memory, size) {
            let frame = crate::frame::current();
            if !exact && last.partial.is_some_and(|at| frame < at + 30) {
                return 0;
            }
            last.partial = Some(frame);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            let changed = with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                let hash = FastHasher::hash128(read);
                if last.hash == Some(hash) {
                    return false;
                }
                last.hash = Some(hash);
                if is_all_zero(read) {
                    return false;
                }
                self.detile(read);
                true
            });
            #[cfg(feature = "diagnostics")]
            if changed {
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            }
            let _ = changed;
            return size;
        }
        if through_copy() {
            return with_title(self.memory, size, |tiled| {
                let hash = FastHasher::hash128(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash == Some(hash) {
                    return size;
                }
                last.hash = Some(hash);
                self.detile(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                size
            });
        }
        // SAFETY: committed memory of the texture's layout size, and it
        // stays committed while `_mapped` is held.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = FastHasher::hash128(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        self.detile(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let read = with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            let hash = FastHasher::hash128(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                self.detile(read);
            }
            true
        });
        if read {
            UNLOCKED[0].fetch_add(1, Relaxed);
            size
        } else {
            UNLOCKED[1].fetch_add(1, Relaxed);
            last.asked = 0;
            0
        }
    }

    fn detile(&self, tiled: &[u8]) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        let held = if self.on_demand {
            use std::sync::atomic::Ordering::Relaxed;
            let judged = self.judgement();
            self.judged[0].store(judged[0], Relaxed);
            self.judged[1].store(judged[1], Relaxed);
            let held = self.held_levels(judged);
            self.detiled.store(held, Relaxed);
            held
        } else {
            u32::MAX
        };
        #[cfg(feature = "diagnostics")]
        if tracing::enabled!(tracing::Level::DEBUG) && is_all_zero(tiled) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        thread_local! {
            static DETILE_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        use std::sync::atomic::Ordering::Relaxed;
        DETILE_BUF.with(|buf_cell| {
            let mut buf = buf_cell.borrow_mut();
            for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
                let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
                let f = footprint.Footprint;
                let pitch = f.RowPitch as usize;
                let len = pitch * rows as usize * f.Depth as usize;
                let dst_ptr = unsafe { self.data.add(footprint.Offset as usize) };
                if mip < 32 && held & (1 << mip) == 0 {
                    if self.zeroed_mips.load(Relaxed) & (1 << mip) == 0 {
                        // SAFETY: the subresource's part of the mapped staging buffer.
                        unsafe { std::ptr::write_bytes(dst_ptr, 0, len) };
                        self.zeroed_mips.fetch_or(1 << mip, Relaxed);
                    }
                    continue;
                }
                if mip < 32 {
                    self.zeroed_mips.fetch_and(!(1 << mip), Relaxed);
                }
                if buf.len() < len {
                    buf.resize(len, 0);
                }
                if let Err(e) = self.layout.detile(
                    tiled,
                    mip,
                    slice,
                    &mut buf[..len],
                    pitch,
                    pitch * rows as usize,
                ) {
                    tracing::warn!(?e, mip, slice, "texture not detiled");
                    return;
                }
                // SAFETY: `buf[..len]` is valid cached RAM and `dst_ptr` points to
                // `len` bytes inside the persistently mapped WC upload buffer.
                unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), dst_ptr, len) };
            }
            if buf.capacity() > (16 << 20) {
                buf.clear();
                buf.shrink_to(4 << 20);
            }
        });
    }
}

pub(crate) fn unstage(resource: This) {
    if STAGING_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let key = resource as usize;
    let present = STAGING
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|map| map.contains_key(&key));
    if !present {
        return;
    }
    let removed = {
        let mut map = STAGING.write().unwrap_or_else(|e| e.into_inner());
        map.as_mut().and_then(|map| {
            let v = map.remove(&key);
            STAGING_COUNT.store(map.len(), Ordering::Relaxed);
            v
        })
    };
    drop(removed);
}

pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
    if STAGING_COUNT.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let map = STAGING.read().unwrap_or_else(|e| e.into_inner());
    let (at, len) = map.as_ref()?.get(&(resource as usize))?.memory();
    Some((at as u64, len as u64))
}

const FINGERPRINT_WINDOW: usize = 16 << 10;

fn fingerprint_windows(len: usize) -> ([(usize, usize); 3], usize) {
    if len <= 3 * FINGERPRINT_WINDOW {
        ([(0, len), (0, 0), (0, 0)], 1)
    } else {
        (
            [
                (0, FINGERPRINT_WINDOW),
                ((len / 2) & !0xfff, FINGERPRINT_WINDOW),
                (len - FINGERPRINT_WINDOW, FINGERPRINT_WINDOW),
            ],
            3,
        )
    }
}

fn readable_committed_live(at: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    if len == 0 {
        return true;
    }
    let mut p = at;
    let end = p.saturating_add(len);
    while p < end {
        let mut info = std::mem::MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
        // SAFETY: valid out pointer of exact size.
        let n = unsafe { VirtualQuery(p as *const _, info.as_mut_ptr(), size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        // SAFETY: VirtualQuery succeeded and initialized `info`.
        let info = unsafe { info.assume_init() };
        if info.State != MEM_COMMIT || (info.Protect & (PAGE_NOACCESS | PAGE_GUARD)) != 0 {
            return false;
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= p {
            return false;
        }
        p = next;
    }
    true
}

fn fingerprint_via_copy(at: usize, windows: &[(usize, usize)]) -> u64 {
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let mut hasher = FastHasher::new();
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < 3 * FINGERPRINT_WINDOW {
            read.resize(3 * FINGERPRINT_WINDOW, 0);
        }
        for &(from, bytes) in windows {
            // `copy_mapped` overwrites every byte (zeroing any unmapped pages),
            // so no pre-zero fill is needed.
            crate::host::copy_mapped(
                read.as_mut_ptr(),
                at.wrapping_add(from) as *const u8,
                bytes,
                false,
            );
            hasher.update(&read[..bytes]);
        }
    });
    hasher.finalize64()
}

pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
    let at = at as usize;
    let len = len as usize;
    let (windows, count) = fingerprint_windows(len);
    let windows = &windows[..count];
    if !through_copy()
        && no_wait()
        && let Some(_mapped) = crate::watch::title_memory_reading_now()
    {
        let mut all_readable = true;
        let mut last_span: Option<(usize, usize)> = None;
        for &(from, bytes) in windows {
            let start = at.wrapping_add(from);
            let end = start.saturating_add(bytes);
            if let Some((b, e)) = last_span
                && start >= b
                && end <= e
            {
                continue;
            }
            if !readable_committed_live(start, bytes) {
                all_readable = false;
                break;
            }
            last_span = Some((start, end));
        }
        if all_readable {
            let mut hasher = FastHasher::new();
            for &(from, bytes) in windows {
                // SAFETY: verified committed and readable above while `_mapped`
                // prevents title memory unmapping.
                let slice =
                    unsafe { std::slice::from_raw_parts(at.wrapping_add(from) as *const u8, bytes) };
                hasher.update(slice);
            }
            return hasher.finalize64();
        }
    }
    fingerprint_via_copy(at, windows)
}

#[cfg(feature = "diagnostics")]
pub(crate) fn staged_state(resource: This) -> Option<(bool, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    if STAGING_COUNT.load(Relaxed) == 0 {
        return None;
    }
    let map = STAGING.read().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    Some((staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed)))
}

pub(crate) fn is_staged(resource: This) -> bool {
    if STAGING_COUNT.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let map = STAGING.read().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .is_some_and(|map| map.contains_key(&(resource as usize)))
}

pub(crate) fn staged(resource: This) -> Option<(u64, bool)> {
    if STAGING_COUNT.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let (memory, size) = {
        let map = STAGING.read().unwrap_or_else(|e| e.into_inner());
        let s = map.as_ref()?.get(&(resource as usize))?;
        (s.memory as usize, s.layout.size as usize)
    };
    Some((size as u64, committed_advisory(memory as *const u8, size)))
}

#[inline]
fn query_region_live(p: usize) -> Option<(usize, usize, bool)> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut info = std::mem::MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(p as *const _, info.as_mut_ptr(), size_of_val(&info)) };
    if n == 0 {
        return None;
    }
    // SAFETY: VirtualQuery succeeded and initialized `info`.
    let info = unsafe { info.assume_init() };
    let base = info.BaseAddress as usize;
    let end = base.saturating_add(info.RegionSize);
    if end <= p {
        return None;
    }
    Some((base, end, info.State == MEM_COMMIT))
}

const QUERY_CACHE_SLOTS: usize = 16;

struct QueryCache {
    seq: u64,
    frame: u64,
    ms: u64,
    hits: u32,
    next: usize,
    len: usize,
    entries: [(usize, usize, bool); QUERY_CACHE_SLOTS],
}

impl QueryCache {
    const fn new() -> Self {
        Self {
            seq: u64::MAX,
            frame: u64::MAX,
            ms: 0,
            hits: 0,
            next: 0,
            len: 0,
            entries: [(0, 0, false); QUERY_CACHE_SLOTS],
        }
    }

    #[inline]
    fn clear(&mut self, seq: u64, frame: u64, ms: u64) {
        self.seq = seq;
        self.frame = frame;
        self.ms = ms;
        self.hits = 0;
        self.next = 0;
        self.len = 0;
    }

    #[inline]
    fn lookup_or_query(&mut self, p: usize) -> Option<(usize, usize, bool)> {
        let seq = crate::watch::title_memory_remapping_seq();
        let frame = crate::frame::current();
        if self.seq != seq || self.frame != frame {
            self.clear(seq, frame, crate::host::now_ms());
        } else if self.hits >= 64 {
            let now = crate::host::now_ms();
            self.hits = 0;
            if now != self.ms {
                self.clear(seq, frame, now);
            }
        }
        for &(base, end, is_commit) in &self.entries[..self.len] {
            if p >= base && p < end {
                self.hits += 1;
                return Some((base, end, is_commit));
            }
        }
        let region = query_region_live(p)?;
        self.entries[self.next] = region;
        self.next = (self.next + 1) % QUERY_CACHE_SLOTS;
        if self.len < QUERY_CACHE_SLOTS {
            self.len += 1;
        }
        Some(region)
    }
}

thread_local! {
    static QUERY_CACHE: std::cell::RefCell<QueryCache> = const { std::cell::RefCell::new(QueryCache::new()) };
}

fn query_cache_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_QUERY_CACHE").is_none_or(|v| v != "0"))
}

#[inline]
fn query_region_cached(p: usize) -> Option<(usize, usize, bool)> {
    if !query_cache_on() {
        return query_region_live(p);
    }
    QUERY_CACHE.with(|c| c.borrow_mut().lookup_or_query(p))
}

#[cfg(test)]
fn clear_query_cache() {
    QUERY_CACHE.with(|c| c.borrow_mut().clear(u64::MAX, u64::MAX, 0));
}

/// Advisory commitment check used by `staged()` (never gates unsafe derefs).
fn committed_advisory(at: *const u8, len: usize) -> bool {
    if !by_query() {
        if len == 0 {
            return true;
        }
        let (there, of) = readable(at as usize, len, true);
        return there == of;
    }
    let mut p = at as usize;
    let end = p.saturating_add(len);
    while p < end {
        let Some((_, next, is_commit)) = query_region_cached(p) else {
            return false;
        };
        if !is_commit {
            return false;
        }
        p = next;
    }
    true
}

pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if by_query() {
        return committed_by_query(at, len);
    }
    if len == 0 {
        return true;
    }
    let (there, of) = readable(at as usize, len, true);
    there == of
}

fn by_query() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_COMMITTED_QUERY").is_none_or(|v| v != "0"))
}

fn readable(at: usize, len: usize, all: bool) -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
    let of = last - first + 1;
    let mut there = 0;
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut page = first;
    while page <= last {
        let n = (last - page + 1).min(info.len());
        for (i, entry) in info[..n].iter_mut().enumerate() {
            entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        for (i, entry) in info[..n].iter().enumerate() {
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let mut byte = [0u8];
            if valid || crate::host::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte) {
                there += 1;
            } else if all {
                return (there, of);
            }
        }
        page += n;
    }
    (there, of)
}

fn committed_by_query(at: *const u8, len: usize) -> bool {
    let mut p = at as usize;
    let end = p.saturating_add(len);
    while p < end {
        let Some((_, next, is_commit)) = query_region_live(p) else {
            return false;
        };
        if !is_commit {
            return false;
        }
        p = next;
    }
    true
}

pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let start = at as usize;
    let end = start.saturating_add(len);
    if !by_query() {
        if !committed(at, 1) && !committed((end - 1) as *const u8, 1) {
            return 0;
        }
        static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *BY_PAGE
            .get_or_init(|| std::env::var_os("OATHKEEP_MAPPED_BY_PAGE").is_some_and(|v| v == "1"))
        {
            let (there, _) = readable(start, len, false);
            return (there as u64 * 4096).min(len as u64);
        }
        const BLOCK: usize = 64 << 10;
        let mut there = 0u64;
        let mut block = start & !(BLOCK - 1);
        while block < end {
            let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
            if readable(lo, 1, true).0 == 1 {
                there += (hi - lo) as u64;
            }
            block = block.saturating_add(BLOCK);
        }
        return there;
    }
    let Some((_, next0, commit0)) = query_region_cached(start) else {
        return 0;
    };
    let next0 = next0.min(end);
    if commit0 {
        if next0 >= end {
            return len as u64;
        }
        let mut there = (next0 - start) as u64;
        let mut p = next0;
        while p < end {
            let Some((_, next, is_commit)) = query_region_cached(p) else {
                break;
            };
            let next = next.min(end);
            if next <= p {
                break;
            }
            if is_commit {
                there += (next - p) as u64;
            }
            p = next;
        }
        return there;
    }
    // `start` is uncommitted. If that single uncommitted region already covers
    // `[start, end)`, return 0 without a second query.
    if next0 >= end {
        return 0;
    }
    // Preserve fast-out when both endpoints are uncommitted.
    let Some((tail_base, _, commit_end)) = query_region_cached(end - 1) else {
        return 0;
    };
    if !commit_end {
        return 0;
    }
    let tail_start = tail_base.max(next0);
    let mut there = (end - tail_start) as u64;
    let mut p = next0;
    while p < tail_start {
        let Some((_, next, is_commit)) = query_region_cached(p) else {
            break;
        };
        let next = next.min(tail_start);
        if next <= p {
            break;
        }
        if is_commit {
            there += (next - p) as u64;
        }
        p = next;
    }
    there
}

fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let key = resource as usize;
    if STAGING_COUNT.load(Ordering::Relaxed) != 0
        && let Some(s) = STAGING
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|map| map.get(&key).cloned())
    {
        return Some(s);
    }
    // Build the staging object outside the global `STAGING` lock so D3D12
    // layout queries and upload-buffer creation never block other threads.
    let on_demand = crate::resource::host_tiled(resource).is_some();
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, data, capacity, fresh) = take_staging_buffer(host, total)?;
    if fresh {
        crate::host::name(&buffer, || {
            format!(
                "staging of {resource:?} {}x{}x{} f{}",
                d.Width, d.Height, d.DepthOrArraySize, d.Format.0
            )
        });
    }
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let built = Arc::new(Staging {
        buffer: Some(buffer),
        capacity,
        #[cfg(feature = "diagnostics")]
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        bytes: total,
        memory,
        last: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(true),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        on_demand,
        detiled: std::sync::atomic::AtomicU32::new(0),
        zeroed_mips: std::sync::atomic::AtomicU32::new(if fresh { u32::MAX } else { 0 }),
        watch_mode: std::sync::atomic::AtomicU8::new(0),
        judged: [const { std::sync::atomic::AtomicU64::new(0) }; 2],
        waiting: std::sync::atomic::AtomicU64::new(0),
        exact: std::sync::atomic::AtomicBool::new(BAKING.get()),
    });
    let (chosen, surplus) = {
        let mut map = STAGING.write().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(UsizeMap::default);
        match map.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => (e.get().clone(), Some(built)),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(built.clone());
                STAGING_COUNT.store(map.len(), Ordering::Relaxed);
                (built, None)
            }
        }
    };
    drop(surplus);
    Some(chosen)
}

#[cfg(test)]
mod title_memory_tests {
    use super::with_title;

    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        // SAFETY: a private reservation of our own; only committed pages
        // are written in place, and nothing else reads them in place.
        unsafe {
            let other = vec![0x77u8; SIZE];
            with_title(other.as_ptr(), SIZE, |bytes| {
                assert!(bytes.iter().all(|&b| b == 0x77))
            });
            let base =
                VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(
                VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            base.write_bytes(0x5a, HEAD);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            assert_ne!(
                VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT),
                0
            );
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            let words: [u32; 14] = [
                3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0,
            ];
            // SAFETY: fourteen words are a resource description (56 bytes,
            // plain numbers).
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(
                size >= row * height as usize,
                "the layout holds every texel"
            );
            // SAFETY: our own reservation; the committed part is written
            // and read in place, by this thread alone.
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(
                    std::ptr::null(),
                    (pages + 1) * 4096,
                    MEM_RESERVE,
                    PAGE_NOACCESS,
                )
                .cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(
                    VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE),
                    base.cast()
                );
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                with_title(memory, size, |copied| assert_eq!(copied, in_place));
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(
                        direct[y * pitch..][..row].iter().any(|&b| b != 0xee),
                        "row {y} written"
                    );
                    assert!(
                        direct[y * pitch + row..(y + 1) * pitch]
                            .iter()
                            .all(|&b| b == 0xee),
                        "nothing past row {y}"
                    );
                }
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }

    #[test]
    fn fast_hasher_covers_every_byte_and_matches_across_paths() {
        use super::{FINGERPRINT_WINDOW, FastHasher, fingerprint, fingerprint_via_copy, fingerprint_windows, is_all_zero};
        assert!(is_all_zero(&[]));
        assert!(is_all_zero(&[0u8; 7]));
        assert!(is_all_zero(&[0u8; 65]));
        let mut nonzero = vec![0u8; 65];
        nonzero[64] = 1;
        assert!(!is_all_zero(&nonzero));
        nonzero[64] = 0;
        nonzero[31] = 0x80;
        assert!(!is_all_zero(&nonzero));

        // Single-bit sensitivity at arbitrary offsets including unaligned tail.
        let mut buf = vec![0x3cu8; 103];
        let base = FastHasher::hash128(&buf);
        for idx in [0, 7, 31, 32, 63, 64, 95, 96, 102] {
            buf[idx] ^= 1;
            assert_ne!(FastHasher::hash128(&buf), base, "bit flip at {idx}");
            buf[idx] ^= 1;
        }

        // Fingerprint direct-read fast path and copy-mapped fallback produce identical output,
        // and detect edits in head, middle, and tail windows.
        let len = 5 * FINGERPRINT_WINDOW + 123;
        let mut big = vec![0u8; len];
        for (i, b) in big.iter_mut().enumerate() {
            *b = (i.wrapping_mul(31) ^ (i >> 7)) as u8;
        }
        let (wins, count) = fingerprint_windows(len);
        let direct = fingerprint(big.as_ptr() as u64, len as u64);
        let copied = fingerprint_via_copy(big.as_ptr() as usize, &wins[..count]);
        assert_eq!(direct, copied, "direct and copy paths must match bit-for-bit");
        assert_ne!(direct & 1, 0, "fingerprint is always odd / non-zero");

        for &pos in &[0usize, FINGERPRINT_WINDOW - 1, (len / 2) & !0xfff, len - 1] {
            big[pos] ^= 0x01;
            let changed = fingerprint(big.as_ptr() as u64, len as u64);
            assert_ne!(changed, direct, "fingerprint must detect edit at {pos}");
            big[pos] ^= 0x01;
        }
    }

    #[test]
    fn mapped_bytes_and_query_cache_handle_committed_partial_and_uncommitted() {
        use super::{clear_query_cache, committed, committed_advisory, mapped_bytes};
        use windows_sys::Win32::System::Memory::*;
        const SIZE: usize = 0x10_0000;
        const HEAD: usize = 0x4_0000;
        const HOLE: usize = 0x2_0000;
        // SAFETY: private reservation owned by this test.
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());

            // Whole uncommitted reservation.
            clear_query_cache();
            assert!(!committed(base, SIZE));
            assert_eq!(mapped_bytes(base, SIZE), 0);
            assert!(!committed_advisory(base, SIZE));

            // Commit head only (partial mapping).
            assert_eq!(
                VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            clear_query_cache();
            assert!(committed(base, HEAD));
            assert!(!committed(base, SIZE));
            assert_eq!(mapped_bytes(base, HEAD), HEAD as u64);
            assert_eq!(mapped_bytes(base, SIZE), HEAD as u64);
            assert!(committed_advisory(base, HEAD));
            assert!(!committed_advisory(base, SIZE));

            // Commit the entire reservation, then punch a hole in the middle.
            assert_eq!(
                VirtualAlloc(base.cast(), SIZE, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            clear_query_cache();
            assert_eq!(mapped_bytes(base, SIZE), SIZE as u64);
            assert!(committed_advisory(base, SIZE));

            assert_ne!(
                VirtualFree(base.add(HEAD).cast(), HOLE, MEM_DECOMMIT),
                0
            );
            // Live `committed` sees the decommit immediately without clearing cache.
            assert!(!committed(base, SIZE));
            // Remapping guard bumps `title_memory_remapping_seq()`, invalidating the cache.
            {
                let _remap = crate::watch::title_memory_remapping();
            }
            assert_eq!(mapped_bytes(base, SIZE), (SIZE - HOLE) as u64);
            assert!(!committed_advisory(base, SIZE));

            // Head uncommitted, tail committed: `mapped_bytes` still counts tail.
            assert_ne!(VirtualFree(base.cast(), HEAD, MEM_DECOMMIT), 0);
            clear_query_cache();
            assert_eq!(mapped_bytes(base, SIZE), (SIZE - HEAD - HOLE) as u64);

            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }
}
