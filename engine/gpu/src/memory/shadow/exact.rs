use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use crate::backend::host;

use super::loose::seed_mapped_bytes;
use super::Shadow;

/// Copies of single buffer views' own bytes ([`exact`]), by (start,
/// bytes), each with the asking tick that last took it.
struct ExactEntry {
    shadow: Arc<Shadow>,
    used: AtomicU64,
}

static EXACT_TICK: AtomicU64 = AtomicU64::new(0);
static EXACT: RwLock<BTreeMap<(u64, u64), ExactEntry>> = RwLock::new(BTreeMap::new());

/// Recycled idle `shared_buffer`s from evicted [`exact`] shadows, keyed by
/// rounded byte size, to avoid `CreateCommittedResource` churn when ring-buffer
/// views move across misaligned offsets every frame.
const EXACT_POOL_MAX: usize = 64;
const EXACT_POOL_MAX_BYTES: u64 = 1 << 20;
#[allow(clippy::type_complexity)]
static EXACT_POOL: Mutex<(usize, BTreeMap<u64, Vec<(ID3D12Resource, usize, u64)>>)> =
    Mutex::new((0, BTreeMap::new()));

/// Since the last frame line: askings for such copies, and copies made.
static EXACT_STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

fn recycle_exact_buffer(shadow: &Shadow) {
    let alloc_size = shadow.width();
    if alloc_size == 0 || alloc_size > EXACT_POOL_MAX_BYTES || shadow.card.is_some() || shadow.place.is_some() {
        return;
    }
    let mut pool = EXACT_POOL.lock().unwrap_or_else(|e| e.into_inner());
    if pool.0 >= EXACT_POOL_MAX {
        return;
    }
    pool.0 += 1;
    pool.1
        .entry(alloc_size)
        .or_default()
        .push((shadow.buffer.clone(), shadow.data as usize, shadow.gpu));
}

fn take_exact_buffer(aligned: u64) -> Option<(ID3D12Resource, *mut u8, u64)> {
    if aligned <= EXACT_POOL_MAX_BYTES {
        let mut pool = EXACT_POOL.lock().unwrap_or_else(|e| e.into_inner());
        let (total, buckets) = &mut *pool;
        if let Some(list) = buckets.get_mut(&aligned) {
            while let Some((buf, data, gpu)) = list.pop() {
                *total = total.saturating_sub(1);
                if crate::backend::references(&buf) == 1 {
                    return Some((buf, data as *mut u8, gpu));
                }
            }
        }
    }
    let (buffer, data) = host()?.shared_buffer(aligned)?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    Some((buffer, data, gpu))
}

/// Pure validity check for an exact aligned copy request `[address, address + len)`.
/// Allows arbitrary byte alignment (e.g. 2-byte aligned `offset=6 element=4` views)
/// because `Shadow` copies and lands byte ranges cleanly while the host buffer
/// itself starts at offset 0 on a 64 KiB boundary.
pub(super) fn exact_range_valid(address: u64, len: u64) -> bool {
    const MOST: u64 = 64 << 20;
    address != 0 && len > 0 && len <= MOST && address.checked_add(len).is_some()
}

/// A copy of the title memory `[address, address + len)` that starts at
/// `address` itself: for a buffer view whose bytes do not start on an
/// element (or 16-byte raw boundary) of the copy its buffer has
/// (`descriptor/host.rs` `view_home`).
/// A host view counts its first element from its buffer's start; a console
/// view starts at any address (including 2-byte aligned offsets such as
/// `offset=6` for 4-byte elements), and the title places views at ring
/// positions inside its big GPU heap.
///
/// The copy is brought up to date and written back like any other (a list
/// refreshes the copy its view's slot shows, `buffer_copy`). Kept by
/// (start, bytes) for as long as a list or a slot holds it; of the others,
/// those not asked for lately go once there are many (ring positions
/// change every frame) and their backing buffers are recycled.
pub(crate) fn exact(address: u64, len: u64) -> Option<Arc<Shadow>> {
    /// Copies kept before the idle ones are looked for, and the askings an
    /// idle one stays for.
    const MANY: usize = 1024;
    const LATELY: u64 = 1024;
    if !exact_range_valid(address, len) {
        return None;
    }
    EXACT_STATS[0].fetch_add(1, Ordering::Relaxed);
    let now = EXACT_TICK.fetch_add(1, Ordering::Relaxed) + 1;
    {
        let map = EXACT.read().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get(&(address, len)) {
            entry.used.store(now, Ordering::Relaxed);
            entry.shadow.used.store(crate::backend::moment(), Ordering::Relaxed);
            return Some(entry.shadow.clone());
        }
    }
    let aligned = len.checked_next_multiple_of(256)?;
    let mut map = EXACT.write().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = map.get(&(address, len)) {
        entry.used.store(now, Ordering::Relaxed);
        entry.shadow.used.store(crate::backend::moment(), Ordering::Relaxed);
        return Some(entry.shadow.clone());
    }
    if map.len() >= MANY {
        // Held by the map alone (no list), its buffer by the copy alone (no
        // slot shows it), and not asked for lately.
        let mut stale = Vec::new();
        for (&key, entry) in map.iter() {
            if Arc::strong_count(&entry.shadow) == 1
                && crate::backend::references(&entry.shadow.buffer) == 1
                && entry.used.load(Ordering::Relaxed) + LATELY <= now
            {
                stale.push(key);
            }
        }
        for key in stale {
            if let Some(entry) = map.remove(&key) {
                recycle_exact_buffer(&entry.shadow);
            }
        }
    }
    let (buffer, data, gpu) = take_exact_buffer(aligned)?;
    crate::backend::name(&buffer, || format!("title view {address:#x}+{len:#x}"));
    seed_mapped_bytes(data, aligned, address, len);
    let shadow = Shadow::new(buffer, data, gpu, address, len);
    let _ = shadow.width.set(aligned);
    let shadow = Arc::new(shadow);
    map.insert(
        (address, len),
        ExactEntry {
            shadow: shadow.clone(),
            used: AtomicU64::new(now),
        },
    );
    EXACT_STATS[1].fetch_add(1, Ordering::Relaxed);
    OWN_MADE.fetch_add(1, Ordering::Relaxed);
    Some(shadow)
}

/// Copies of single views' bytes made since the start.
pub(crate) static OWN_MADE: AtomicU64 = AtomicU64::new(0);

/// With the frame line: the buffer views that got a copy of their own
/// bytes ([`exact`]), per frame over `frames`, and the copies kept now.
/// Nothing while there are none.
pub(crate) fn exact_line(frames: f64) {
    let [asked, made] = [0, 1].map(|i| EXACT_STATS[i].swap(0, Ordering::Relaxed));
    if asked == 0 {
        return;
    }
    let kept = EXACT.read().unwrap_or_else(|e| e.into_inner()).len();
    tracing::info!(
        "buffer views off their copy's elements (they were refused and read zeros; OATHKEEP_VIEW_BYTES=0 refuses them again): \
         {:.1} asked for per frame, {:.1} copies of their own bytes made per frame, {kept} kept",
        asked as f64 / frames,
        made as f64 / frames
    );
}
