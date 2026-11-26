//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::memory::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

pub(crate) mod prefetch;
mod alias;
mod aliased;
mod committed;
mod lru;
mod refresh;
mod staging;
#[cfg(test)]
mod tests;
mod upload;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::d3d12::object::This;
pub(crate) use alias::alias;
pub(crate) use aliased::fingerprint;
pub(crate) use committed::{committed, mapped_bytes};
pub(crate) use lru::{let_go_idle, note_picked_upload};
pub(crate) use staging::Staging;
pub(crate) use upload::{card_writes, upload, upload_from, zeros};

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}
pub(super) use once;

pub(super) static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// Since the last frame-rate line, with `no_wait`: refreshes that read
/// through the kernel because title memory was being remapped, and those
/// left for the next refresh because a page was not there; staging buffers
/// filled from the pages there (`fill_unlocked`), and of them those that
/// got less than was mapped (their slots are looked at again).
pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Since the last frame-rate line, of texture refreshes on every thread, in
/// microseconds: waiting for the texture's own lock (another thread is
/// refreshing the same texture), asking the write watch and the guards,
/// waiting for title memory to hold still (the lock the runtime takes
/// alone while it remaps, `crate::memory::watch::title_memory_reading`), and the
/// refreshes in all.
pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// `f`'s result, its time added to [`REFRESH_WAITS`]' place `slot`.
pub(super) fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    REFRESH_WAITS[slot].fetch_add(started.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    out
}

thread_local! {
    /// Room to read a texture's memory into: one buffer per submitting
    /// thread. Each texture kept its own, a second copy in memory of every
    /// write-watched or partly mapped texture ever sampled, for the run.
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Calls `f` with `size` bytes of this thread's read buffer (their
/// contents are whatever the last texture left).
pub(super) fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < size {
            read.resize(size, 0);
        }
        f(&mut read[..size])
    })
}

/// Calls `f` with the `size` bytes of title memory at `memory`, read
/// through the kernel into this thread's read buffer; pages that are not
/// there read as zeros. The bytes are those a read in place would see.
#[cfg(test)]
pub(super) fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::backend::copy_mapped(read.as_mut_ptr(), memory, size, false);
        f(read)
    })
}

/// Staging buffers alive (also those only a list still holds) and their
/// bytes.
pub(super) static STAGED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

pub(crate) fn live_staging_bytes() -> u64 {
    STAGED[1].load(std::sync::atomic::Ordering::Relaxed)
}

/// For the memory line: staging buffers alive, their bytes, and how many
/// of them their texture still has (the others wait for a list's reset).
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 3] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |map| map.len() as u64);
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
}

/// Staging buffers let go as idle since the start, and their bytes.
pub(super) static LET_GO: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// For the memory line: staging buffers let go as idle, and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

/// Last time (`coarse_ms`) a draw-bound or bake texture uploaded, so
/// speculative background prefetch yields staging/VRAM bandwidth to visible
/// draws.
pub(super) static LAST_DRAW_UPLOAD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// True when a draw-bound or bake texture uploaded within the last 50 ms.
pub(crate) fn draw_active_recently() -> bool {
    let last = LAST_DRAW_UPLOAD_MS.load(std::sync::atomic::Ordering::Relaxed);
    last != 0 && crate::backend::coarse_ms().saturating_sub(last) <= 50
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
pub(super) static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let new = {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(Default::default).insert(resource as usize)
    };
    // Diagnostics (run 177): where its title memory is, for placed views
    // the title reads that memory through ([`placed_in_host_written`]).
    if new && let Some((at, desc)) = crate::d3d12::resource::texture_memory(resource) {
        // SAFETY: 56 bytes in the description's layout.
        let words = unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() };
        let bytes = crate::d3d12::resource::placed_bytes(&words).unwrap_or(0);
        if !at.is_null() && bytes > 0 {
            HOST_WRITTEN_AT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(at as u64, (bytes, words[4], words[6], words[7], words[8], resource as usize));
        }
    }
}

/// The title memory of textures the host copied into, by address: bytes
/// and description words 4, 6, 7 and 8 (width, height, layers and levels,
/// format).
static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<u64, (u64, u32, u32, u32, u32, usize)>> =
    Mutex::new(std::collections::BTreeMap::new());

/// A texture the title placed at `address` (description words `desc`) was
/// made: says so once if it lies in the memory of a texture the host
/// copied into ([`host_written`]). Its own host copy is filled from title
/// memory, which the copy never reached: the copied texels are on the
/// card in the other texture, where no shader that reads this view finds
/// them.
pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    let hit = {
        let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
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

/// Whether the host GPU wrote `resource` by a copy or resolve
/// ([`host_written`]): its host copy cannot be made again from title
/// memory.
pub(crate) fn host_wrote(resource: usize) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
    // Its memory may be given to other textures now (run 177: most of the
    // "HERO LEAD" lines were views in memory a released texture had held).
    HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, d| d.5 != resource);
    alias::forget(resource);
    lru::forget(resource);
    aliased::forget(resource);
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

thread_local! {
    /// The thread records uploads for a draw that bakes from them.
    pub(crate) static BAKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The thread records the upload of a texture picked by index, of which
    /// that many bytes were found mapped just now (`u64::MAX`: not asked).
    pub(crate) static PICKING: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// A 128-bit digest of `bytes`, to tell a texture's memory changed: four
/// lanes of folded multiplications over 64 bytes at a time. (Not a
/// cryptographic one, which took 0.27 ms a megabyte, four times the detiling
/// it was to spare; nobody makes up textures to collide.)
pub(crate) fn digest(bytes: &[u8]) -> Digest {
    const K: [u64; 8] = [
        0xa076_1d64_78bd_642f,
        0xe703_7ed1_a0b4_28db,
        0x8ebc_6af0_9c88_c6e3,
        0x5899_65cc_7537_4cc3,
        0x1d8e_4e27_c47d_124f,
        0xeb44_acca_b455_d165,
        0x9e37_79b9_7f4a_7c15,
        0xc2b2_ae3d_27d4_eb4f,
    ];
    let fold = |a: u64, b: u64| {
        let p = u128::from(a) * u128::from(b);
        p as u64 ^ (p >> 64) as u64
    };
    let word = |b: &[u8]| {
        let mut w = [0; 8];
        w[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    let mut lanes = [K[0], K[1], K[2], K[3]];
    let mut step = |block: &[u8; 64]| {
        for (i, lane) in lanes.iter_mut().enumerate() {
            let a = word(&block[16 * i..16 * i + 8]);
            let b = word(&block[16 * i + 8..16 * i + 16]);
            // (What went in is added again: a product with a factor of zero
            // forgets neither the lane nor the block.)
            *lane = fold(a ^ K[4 + i], b ^ *lane).wrapping_add(a.rotate_left(23) ^ lane.rotate_left(41)) ^ b;
        }
    };
    let mut blocks = bytes.chunks_exact(64);
    for block in &mut blocks {
        step(block.try_into().unwrap_or(&[0; 64]));
    }
    let rest = blocks.remainder();
    let mut last = [0u8; 64];
    last[..rest.len()].copy_from_slice(rest);
    step(&last);
    let len = bytes.len() as u64;
    let low = fold(lanes[0] ^ K[1], lanes[1] ^ len) ^ fold(lanes[2] ^ K[3], lanes[3] ^ K[5]);
    let high = fold(lanes[0].rotate_left(32) ^ K[6], lanes[2] ^ len) ^ fold(lanes[1].rotate_left(32) ^ K[7], lanes[3] ^ K[2]);
    Digest(u128::from(low) << 64 | u128::from(high))
}

/// See [`digest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Digest(pub(super) u128);

/// Lets go of a title texture's staging buffer (lists that recorded an
/// upload from it hold it until they have run); the next upload makes one
/// anew.
pub(crate) fn unstage(resource: This) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&(resource as usize));
    }
}

/// Where a staged title texture's memory is: address and bytes.
pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let (at, len) = map.as_ref()?.get(&(resource as usize))?.memory();
    Some((at as u64, len as u64))
}

/// Whether a title texture has a staging buffer: a list recorded its
/// upload, and the lists that sample it check it at submission.
/// For the frame listing: whether a list has uploaded `resource`'s staging
/// buffer, and whether it was detiled since. None: it has none.
#[cfg(feature = "diagnostics")]
pub(crate) fn staged_state(resource: This) -> Option<(bool, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    Some((staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed)))
}

/// The first level a title texture's staging buffer holds; `u32::MAX`
/// without one.
pub(super) fn staged_from(resource: This) -> u32 {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .and_then(|map| map.get(&(resource as usize)))
        .map_or(u32::MAX, |s| s.first)
}

/// Whether the levels from `first` on of `resource`'s host copy were filled
/// from the bytes title memory holds now: an upload would bring nothing new.
pub(crate) fn unchanged(resource: This, first: u32) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_SAME").is_none_or(|v| v != "0")) {
        return false;
    }
    let Some(copy) = crate::d3d12::resource::made_copy(resource) else {
        return false;
    };
    let Some((from, held)) = *copy.holds.lock().unwrap_or_else(|e| e.into_inner()) else {
        return false;
    };
    let Some((memory, desc)) = crate::d3d12::resource::texture_memory(resource) else {
        return false;
    };
    let Some(layout) = crate::d3d12::resource::texture_layout(&desc) else {
        return false;
    };
    let layout = &layout.planes[0].layout;
    if from > crate::d3d12::resource::read_from(layout, first).max(copy.base) {
        return false;
    }
    let size = layout.bytes_from(from) as usize;
    with_read(size, |read| {
        if crate::backend::read_there(memory, read) == 0 {
            return false;
        }
        digest(read) == held
    })
}

/// Where the levels from `first` on of a title texture are: address and
/// bytes.
pub(crate) fn title_span(resource: This, first: u32) -> Option<(u64, u64)> {
    let (memory, desc) = crate::d3d12::resource::texture_memory(resource)?;
    let layout = crate::d3d12::resource::texture_layout(&desc)?;
    let layout = &layout.planes[0].layout;
    Some((memory as u64, layout.bytes_from(crate::d3d12::resource::read_from(layout, first))))
}

pub(crate) fn is_staged(resource: This) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref().is_some_and(|map| map.contains_key(&(resource as usize)))
}

/// The bytes of a title texture's memory, and whether all of it is mapped,
/// if the texture has been staged for upload.
/// (Whether it is mapped is asked only if `ask`; else it counts as mapped.)
pub(crate) fn staged(resource: This, ask: bool) -> Option<(u64, bool)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    Some((s.read as u64, !ask || committed(s.memory, s.read)))
}
