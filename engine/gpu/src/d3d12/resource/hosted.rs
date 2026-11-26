use std::ffi::c_void;

use super::format::{element, footprint, target_format, texture_layout};
use super::host_tex::{host_texture, host_texture_as};
use super::placed::placed_bytes;
use super::twin::{Twin, twin_texture};
use super::{BUFFER, ResourceDesc, resource};
use crate::d3d12::object::This;

/// A title texture's host copy and the state it is in.
pub(crate) struct HostTexture {
    pub(crate) texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    pub(crate) state: std::sync::atomic::AtomicU32,
    pub(super) bytes: u64,
    pub(super) filled: bool,
    pub(crate) tiled: Option<crate::memory::tiles::Tiled>,
    pub(crate) base: u32,
    pub(crate) serial: u64,
    pub(crate) grown: std::sync::atomic::AtomicBool,
    pub(crate) holds: std::sync::Arc<Holds>,
    pub(crate) ahead: std::sync::atomic::AtomicU8,
    pub(super) flags: u32,
}

/// What a host copy was filled with last: the first level of it and the
/// hash of the title memory it was read from.
pub(crate) type Holds = std::sync::Mutex<Option<(u32, crate::textures::texture::Digest)>>;

pub(super) static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(super) type Made = (std::sync::Arc<HostTexture>, Option<std::sync::Arc<Twin>>);

/// A title texture's host side.
pub(super) struct Hosted {
    pub(super) made: std::sync::Mutex<Option<Made>>,
    pub(super) state: u32,
    clear: Option<[u32; 5]>,
    pub(super) used: std::sync::atomic::AtomicU64,
    refused: std::sync::atomic::AtomicBool,
    pub(super) gone: std::sync::atomic::AtomicBool,
    pub(super) placed: std::sync::atomic::AtomicBool,
    pub(super) wanted: std::sync::atomic::AtomicU32,
    pub(super) grown: std::sync::atomic::AtomicBool,
}

impl Hosted {
    pub(super) fn new(state: u32, clear: *const c_void) -> Hosted {
        Hosted {
            made: std::sync::Mutex::new(None),
            state,
            // SAFETY: the title passes a whole clear value (5 words) or null.
            clear: (!clear.is_null()).then(|| unsafe { clear.cast::<[u32; 5]>().read_unaligned() }),
            used: std::sync::atomic::AtomicU64::new(0),
            refused: std::sync::atomic::AtomicBool::new(false),
            gone: std::sync::atomic::AtomicBool::new(false),
            placed: std::sync::atomic::AtomicBool::new(false),
            wanted: std::sync::atomic::AtomicU32::new(u32::MAX),
            grown: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// Whether a texture the title places in memory is read, kept and uploaded
/// from the first level its views show on.
pub(crate) fn held_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("OATHKEEP_HELD_LEVELS").is_none_or(|v| v != "0") && !crate::memory::tiles::wanted()
    })
}

/// The first level a placed texture's view shows.
pub(crate) fn first_mip(view: &[u32]) -> u32 {
    const TEXTURE_2D_VIEW: u32 = 4;
    match *view {
        [_, TEXTURE_2D_VIEW, _, _, first, ..] => first.min(15),
        _ => 0,
    }
}

/// The level from which on a texture of layout `layout` is read for a view
/// that starts at `first`.
pub(crate) fn read_from(layout: &oathkeep_formats::texture::Layout, first: u32) -> u32 {
    if layout.slices != 1 || layout.mips.is_empty() {
        return 0;
    }
    let first = first.min(layout.first_tail_mip).min(layout.mips.len() as u32 - 1);
    if layout.bytes_from(first) < layout.size { first } else { 0 }
}

fn base_level(desc: &ResourceDesc, first: u32) -> u32 {
    const TEXTURE_2D: u32 = 3;
    if first == 0
        || !fills(desc)
        || desc.dimension != TEXTURE_2D
        || desc.depth_or_array_size != 1
        || desc.mip_levels <= 1
        || desc.sample_count > 1
    {
        return 0;
    }
    let Some(t) = texture_layout(desc).filter(|t| t.planes.len() == 1) else {
        return 0;
    };
    let layout = &t.planes[0].layout;
    if layout.desc.swizzle.xor() {
        return 0;
    }
    let block = u64::from(layout.desc.block.max(1));
    let mut base = read_from(layout, first).min(u32::from(desc.mip_levels) - 1);
    let whole = |side: u64| side != 0 && side.is_multiple_of(block);
    while base > 0 && !(whole(desc.width >> base) && whole(u64::from(desc.height) >> base)) {
        base -= 1;
    }
    base
}

/// The bytes of title memory a placed texture is read from for a view that starts at level `first`.
pub(crate) fn placed_bytes_from(desc: &[u32; 14], first: u32) -> Option<u64> {
    if first == 0 || !held_only() {
        return placed_bytes(desc);
    }
    type Known = std::collections::HashMap<([u32; 14], u32), Option<u64>>;
    thread_local! {
        static KNOWN: std::cell::RefCell<Known> = std::cell::RefCell::default();
    }
    KNOWN.with(|known| {
        *known.borrow_mut().entry((*desc, first)).or_insert_with(|| {
            let whole = placed_bytes(desc)?;
            // SAFETY: 56 bytes in the description's layout.
            let d = unsafe { desc.as_ptr().cast::<ResourceDesc>().read_unaligned() };
            let Some(t) = texture_layout(&d).filter(|t| t.planes.len() == 1) else {
                return Some(whole);
            };
            let layout = &t.planes[0].layout;
            Some(layout.bytes_from(read_from(layout, first)).min(whole))
        })
    })
}

pub(super) static REPLACED: std::sync::Mutex<Vec<(u64, Made)>> = std::sync::Mutex::new(Vec::new());
pub(crate) static HELD: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

pub(crate) fn want_level(this: This, first: u32) {
    use std::sync::atomic::Ordering::Relaxed;
    if this.is_null() || !held_only() {
        return;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER || !r.host.placed.load(Relaxed) {
        return;
    }
    let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
    let Some((host, _)) = made.as_ref() else {
        r.host.wanted.fetch_min(first, Relaxed);
        return;
    };
    if host.base == 0 || host.base <= base_level(&r.desc, first) {
        return;
    }
    r.host.wanted.store(first, Relaxed);
    crate::textures::texture::unstage(this);
    r.host.grown.store(true, Relaxed);
    HELD[2].fetch_add(1, Relaxed);
    if let Some(old) = made.take() {
        REPLACED.lock().unwrap_or_else(|e| e.into_inner()).push((crate::backend::moment(), old));
    }
}

pub(crate) static KINDS: [std::sync::atomic::AtomicU64; 7] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 7];

static TEXTURES: std::sync::Mutex<std::collections::BTreeSet<usize>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

pub(super) fn textures() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<usize>> {
    TEXTURES.lock().unwrap_or_else(|e| e.into_inner())
}

static TARGETS_AT: std::sync::Mutex<std::collections::BTreeMap<u64, usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

pub(super) fn register_texture(at: usize, desc: &ResourceDesc, data: *mut u8) {
    textures().insert(at);
    crate::textures::readback::replaced(data as usize);
    if !fills(desc) {
        TARGETS_AT.lock().unwrap_or_else(|e| e.into_inner()).insert(data as u64, at);
    }
    if !lazy() {
        let _ = hosted(at as This, true);
    }
}

pub(super) fn unregister_texture(address: usize, desc: &ResourceDesc, data: *mut u8) {
    crate::textures::readback::released(address);
    textures().remove(&address);
    if !fills(desc) {
        let mut targets = TARGETS_AT.lock().unwrap_or_else(|e| e.into_inner());
        let at = data as u64;
        if targets.get(&at) == Some(&address) {
            targets.remove(&at);
        }
    }
}

/// For `this`, a texture made for a view the title placed in memory:
/// the title texture the host writes that lies in the same memory.
pub(crate) fn alias_of(this: This) -> Option<This> {
    use std::sync::atomic::Ordering::Relaxed;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if this.is_null()
        || !*ON.get_or_init(|| std::env::var_os("OATHKEEP_ALIAS_COPY").is_none_or(|v| v != "0"))
    {
        return None;
    }
    static WHOLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let whole = *WHOLE.get_or_init(|| std::env::var_os("OATHKEEP_ALIAS_COPY").is_none_or(|v| v != "1"));
    let r = resource(this);
    let one = |d: &ResourceDesc| {
        d.dimension != BUFFER
            && d.mip_levels >= 1
            && d.depth_or_array_size >= 1
            && (whole || (d.mip_levels == 1 && d.depth_or_array_size == 1))
            && d.sample_count <= 1
    };
    if !r.host.placed.load(Relaxed) || !fills(&r.desc) || !one(&r.desc) {
        return None;
    }
    let targets = TARGETS_AT.lock().unwrap_or_else(|e| e.into_inner());
    let other = *targets.get(&(r.data() as u64))? as This;
    let o = resource(other);
    const DEPTH: u32 = 0x2;
    let same = one(&o.desc)
        && o.desc.dimension == r.desc.dimension
        && o.desc.width == r.desc.width
        && o.desc.height == r.desc.height
        && o.desc.mip_levels == r.desc.mip_levels
        && o.desc.depth_or_array_size == r.desc.depth_or_array_size
        && o.desc.flags & DEPTH == 0
        && target_format(o.desc.format) == o.desc.format
        && element(o.desc.format).is_some_and(|e| e.1 == 1 && Some(e) == element(r.desc.format));
    if !same && r.desc.sample_count <= 1 {
        static SAID: std::sync::Mutex<std::collections::BTreeSet<[u32; 6]>> =
            std::sync::Mutex::new(std::collections::BTreeSet::new());
        let pair = [
            r.desc.width as u32, r.desc.height, r.desc.format,
            o.desc.width as u32, o.desc.height, o.desc.format,
        ];
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if said.len() < 64 && said.insert(pair) {
            tracing::warn!(
                view = ?this,
                texture = ?other,
                "a placed texture lies where a texture the host writes does, and is not like it (width, height, format of each): {pair:?}, levels {} and {}, layers {} and {}",
                r.desc.mip_levels, o.desc.mip_levels, r.desc.depth_or_array_size, o.desc.depth_or_array_size
            );
        }
    }
    same.then_some(other)
}

fn lazy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LAZY_TEXTURES").is_none_or(|v| v != "0"))
}

pub(super) fn fills(desc: &ResourceDesc) -> bool {
    const TARGET_DEPTH_UNORDERED: u32 = 0x7;
    desc.dimension != BUFFER && desc.flags & TARGET_DEPTH_UNORDERED == 0
}

pub(crate) fn title_fills(this: This) -> bool {
    !this.is_null() && fills(&resource(this).desc)
}

pub(crate) fn host_waits(this: This) -> bool {
    if this.is_null() || !lazy() {
        return false;
    }
    let r = resource(this);
    r.desc.dimension != BUFFER
        && !r.host.refused.load(std::sync::atomic::Ordering::Relaxed)
        && r.host.made.lock().unwrap_or_else(|e| e.into_inner()).is_none()
}

pub(super) fn hosted(this: This, make: bool) -> Option<Made> {
    use std::sync::atomic::Ordering::Relaxed;
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
    if made.is_none() {
        if !make || r.host.refused.load(Relaxed) {
            return None;
        }
        let clear = r.host.clear.as_ref().map_or(std::ptr::null(), |c| std::ptr::from_ref(c).cast::<c_void>());
        const TEXTURE_2D: u32 = 3;
        let mut tiled = None;
        let mut texture = None;
        let shape_fits = fills(&r.desc)
            && r.desc.dimension == TEXTURE_2D
            && r.desc.depth_or_array_size == 1
            && r.desc.mip_levels > 1
            && r.desc.sample_count <= 1;
        if shape_fits && r.host.placed.load(Relaxed) && crate::memory::tiles::wanted() {
            let bytes = footprint(&r.desc).map_or(0, |f| f.size);
            let start = r.data() as u64;
            let readable = texture_layout(&r.desc).is_some_and(|t| !t.planes[0].layout.desc.swizzle.xor());
            let partly = readable
                && bytes >= crate::memory::tiles::floor()
                && (!crate::textures::texture::committed(r.data().cast_const(), bytes as usize)
                    || crate::descriptors::host_heap::next_start(start, start.saturating_add(bytes)).is_some()
                    || crate::descriptors::host_heap::first_viewed(start).is_some_and(|first| first > 0));
            let reserved = if partly { host_texture_as(&r.desc, r.host.state, clear, true) } else { None };
            let cut = reserved
                .as_ref()
                .zip(crate::backend::host())
                .and_then(|(t, host)| crate::memory::tiles::Tiled::make(&host.device, t, u32::from(r.desc.mip_levels)));
            if cut.is_some() {
                tiled = cut;
                texture = reserved;
            }
        }
        let mut base = 0;
        if shape_fits && r.host.placed.load(Relaxed) && held_only() && alias_of(this).is_none() {
            let wanted = r.host.wanted.load(Relaxed);
            let viewed = crate::descriptors::host_heap::first_viewed(r.data() as u64).unwrap_or(u32::MAX);
            let first = wanted.min(viewed);
            if first != u32::MAX {
                base = base_level(&r.desc, first);
            }
        }
        let texture = match texture {
            Some(texture) => Some(texture),
            None => host_texture(&r.desc.from_level(base), r.host.state, clear),
        };
        let Some(texture) = texture else {
            if !crate::memory::pool::no_grow() {
                r.host.refused.store(true, Relaxed);
            }
            return None;
        };
        let twin = twin_texture(&r.desc, &texture);
        if r.desc.flags & 6 == 6 {
            tracing::info!(resource = ?this, desc = ?r.desc, twin = twin.is_some(), "depth texture with unordered access");
        }
        let state = crate::backend::legal_state(&texture, crate::backend::texture_state(r.host.state));
        let mut host = match tiled {
            Some(tiled) => HostTexture::new_tiled(texture, state, tiled),
            None => HostTexture::new(texture, state),
        };
        host.base = base;
        host.grown.store(r.host.grown.swap(false, Relaxed), Relaxed);
        if base > 0 {
            HELD[0].fetch_add(1, Relaxed);
            HELD[1].fetch_add(host.bytes, Relaxed);
            HELD[3].fetch_add(footprint(&r.desc).map_or(0, |f| f.size), Relaxed);
        }
        host.filled = fills(&r.desc);
        if host.filled {
            KINDS[0].fetch_add(1, Relaxed);
            KINDS[1].fetch_add(host.bytes, Relaxed);
        }
        if r.host.gone.load(Relaxed) {
            KINDS[4].fetch_add(1, Relaxed);
            KINDS[5].fetch_add(host.bytes, Relaxed);
        }
        KINDS[6].fetch_add(1, Relaxed);
        *made = Some((std::sync::Arc::new(host), twin));
        crate::descriptors::texture_made(this as usize);
    }
    let now = crate::backend::moment();
    if r.host.used.load(Relaxed) != now {
        r.host.used.store(now, Relaxed);
    }
    made.clone()
}

#[cfg(feature = "diagnostics")]
pub(crate) fn host_made<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (host, _) = hosted(this, false)?;
    // SAFETY: as in `host_resource`.
    Some(unsafe { &*std::ptr::from_ref(&host.texture) })
}
