use super::create::create_at_address;
use super::format::{footprint, texture_layout};
#[cfg(feature = "diagnostics")]
use super::host_tex::made_copy;
use super::{BUFFER, RESOURCE_IIDS, resource};
use crate::d3d12::object::{S_OK, This};

#[cfg(feature = "diagnostics")]
pub(super) use super::ResourceDesc;

static PLACED_TEXTURES: std::sync::Mutex<std::collections::BTreeMap<(u64, [u32; 14]), usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

static PLACED_RAW: std::sync::Mutex<std::collections::BTreeSet<(u64, [u32; 14])>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

static PLACED_TWINS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn placed_textures_map() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<(u64, [u32; 14]), usize>> {
    PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(feature = "diagnostics")]
pub(crate) fn placed_textures() -> u64 {
    PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner()).len() as u64
}

pub(crate) fn placed_filler_twins() -> u64 {
    PLACED_TWINS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn texture_kind(this: This) -> Option<(u64, u32, u32)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    const TEXTURE_2D: u32 = 3;
    (r.desc.dimension == TEXTURE_2D && r.desc.sample_count <= 1)
        .then_some((r.desc.width, r.desc.height, r.desc.format))
}

pub(crate) fn title_texture(this: This) -> Option<(oathkeep_formats::texture::Layout, usize, usize)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let t = texture_layout(&r.desc)?;
    let [plane] = &t.planes[..] else {
        return None;
    };
    Some((plane.layout.clone(), r.data() as usize, t.size as usize))
}

pub(crate) fn placed_bytes(desc: &[u32; 14]) -> Option<u64> {
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe { desc.as_ptr().cast::<super::ResourceDesc>().read_unaligned() };
    if d.dimension == BUFFER {
        return None;
    }
    footprint(&d).map(|f| f.size)
}

#[cfg(feature = "diagnostics")]
pub(crate) fn texture_at_made(address: u64, desc: &[u32; 14]) -> Option<This> {
    let mut key = *desc;
    key[1] = 0;
    key[13] = 0;
    let map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let found = map.get(&(address, key)).or_else(|| map.get(&(address, *desc)))?;
    Some(*found as This)
}

#[cfg(feature = "diagnostics")]
pub(crate) fn texture_held(resource: This) -> Option<(u32, crate::textures::texture::Digest)> {
    *made_copy(resource)?.holds.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn texture_at(address: u64, desc: &[u32; 14]) -> Option<This> {
    static KEPT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let raw = *desc;
    let mut key = *desc;
    if !*KEPT.get_or_init(|| std::env::var_os("OATHKEEP_PLACED_FILLER").is_some_and(|v| v == "1")) {
        key[1] = 0;
        key[13] = 0;
    }
    let desc = &key;
    let mut map = PLACED_TEXTURES.lock().unwrap_or_else(|e| e.into_inner());
    if raw != key {
        let mut seen = PLACED_RAW.lock().unwrap_or_else(|e| e.into_inner());
        if seen.insert((address, raw)) {
            let others = seen.range((address, [0; 14])..=(address, [u32::MAX; 14])).filter(|(_, r)| {
                let mut blank = *r;
                blank[1] = 0;
                blank[13] = 0;
                blank == key
            });
            if others.count() > 1 {
                PLACED_TWINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    if let Some(&r) = map.get(&(address, *desc)) {
        return Some(r as This);
    }
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe { desc.as_ptr().cast::<super::ResourceDesc>().read_unaligned() };
    if d.dimension == BUFFER {
        return None;
    }
    let mut out = std::ptr::null();
    let made = create_at_address(
        std::ptr::null(),
        address,
        &d,
        0,
        std::ptr::null(),
        &RESOURCE_IIDS[0],
        &mut out,
    );
    if made != S_OK {
        return None;
    }
    map.insert((address, *desc), out as usize);
    crate::textures::texture::placed_in_host_written(address, desc);
    resource(out)
        .host
        .placed
        .store(true, std::sync::atomic::Ordering::Relaxed);
    Some(out)
}
