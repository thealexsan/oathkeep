use windows::Win32::Graphics::Direct3D12::*;

use super::lru::{note_draw_upload, note_picked_upload};
use super::refresh::{fill_unlocked, no_wait};
use super::staging::{Staging, staging};
use super::{
    BAKING, HOST_WRITTEN, LAST_DRAW_UPLOAD_MS, PICKING, ahead, mapped_bytes, once, staged_from,
    with_read,
};
use crate::host::{HostList, host};
use crate::object::This;

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, _texture: &ID3D12Resource) {
    upload_from(list, resource, 0);
}

/// [`upload`] of the levels from `first` on, the ones a view shows: of a
/// texture the title placed there may be no memory of the larger ones, and
/// what lies there is another texture's. (After victory-amd 4436f8f1.)
pub(crate) fn upload_from(list: &HostList, resource: This, first: u32) {
    let first = if crate::resource::held_only() { first } else { 0 };
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written {
        return;
    }
    // Once a list for each first level; not again for one that the list's
    // upload of larger ones held too.
    if !list.first_use(1 << 63 | (resource as usize) << 4 | first as usize & 15) {
        return;
    }
    if !list.first_use(resource as usize) && staged_from(resource) <= first {
        return;
    }
    let Some(copy) = crate::resource::host_copy(resource) else {
        return;
    };
    let texture = &copy.texture;
    ahead::listed(&copy);
    let Some(staging) = staging(resource, &copy, first) else {
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    let baking = BAKING.get();
    if baking {
        staging.exact.store(true, Relaxed);
    }
    let picking = PICKING.get();
    if picking.is_none() || baking {
        LAST_DRAW_UPLOAD_MS.store(crate::host::coarse_ms().max(1), Relaxed);
        note_draw_upload(resource, staging.memory as u64, staging.bytes.max(staging.read as u64));
    } else {
        note_picked_upload(resource, &copy, &staging);
    }
    if picking.is_none() {
        // A draw samples it through a table: kept and checked from here on.
        staging.once.store(false, Relaxed);
    }
    staging.used.store(crate::host::moment(), Relaxed);
    // Levels on demand: a view of the texture now starts at another
    // level, or another texture now starts inside its span, with no byte
    // of its memory changed. The levels are judged anew: detiled again at
    // this list's submission, and uploaded by this list.
    if staging.on_demand && staging.judgement() != staging.judged() {
        staging.forget();
        staging.dirty.store(true, Relaxed);
    }
    // Levels on demand: the last upload left a held level without tiles.
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
    // The video decoder cycles CPU-written linear plane buffers. Keep a
    // dedicated per-list upload buffer through the list's GPU fence so
    // concurrent frames never overwrite each other or stall on staging hashes.
    if staging.movie_plane() {
        let (at, len) = staging.memory();
        let footprint = staging.footprints[0].Footprint;
        let pitch = footprint.RowPitch as usize;
        let bytes = pitch * staging.rows[0] as usize;
        let plane = staging.clone();
        let Some(source) = list.plane_upload(bytes as u64, (at, len), move |dst| {
            fill_plane(&plane.layout, at, len, dst, bytes, pitch);
        }) else {
            once!("no upload buffer for a CPU-written movie plane");
            return;
        };
        record_upload(list, resource, texture, &staging, &source, 0);
        staging.uploaded.store(true, Relaxed);
        return;
    }
    // The host texture has the staging buffer's contents: only the check
    // at submission is left.
    if !baking && staging.uploaded.load(Relaxed) && !staging.dirty.load(Relaxed) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexSkipped, 1);
        list.defer_texture(staging);
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::TexUploads, 1);
    if no_wait() && fill_unlocked() {
        let there = match picking {
            Some(there) if there != u64::MAX => there,
            _ => mapped_bytes(staging.memory, staging.read),
        };
        staging.expected.store(there, Relaxed);
    }
    record_upload(list, resource, texture, &staging, &staging.buffer, staging.base);
    {
        let staging = staging.clone();
        list.defer_host(move || {
            staging.uploaded.store(true, Relaxed);
            staging.dirty.store(false, Relaxed);
        });
    }
    // A host copy that took the place of one with fewer levels: the slots
    // that show that one are made again once this upload has run.
    if staging.first == staging.level0 && copy.grown.swap(false, Relaxed) {
        let start = staging.memory as u64;
        list.defer_host(move || crate::descriptor::host::levels_changed_after(start));
    }
    let noted = staging.clone();
    let on_demand = if staging.on_demand { crate::resource::host_tiled(resource) } else { None };
    let Some(on_demand) = on_demand else {
        list.defer_texture(staging);
        list.defer_host(move || noted.note_held());
        return;
    };
    list.defer_texture(staging.clone());
    list.defer_host(move || {
        let waiting = on_demand.uploaded(staging.detiled.load(Relaxed), staging.memory as u64)
            || staging.clamped_mips.load(Relaxed);
        staging.waiting.store(if waiting { crate::host::moment().max(1) } else { 0 }, Relaxed);
        if waiting {
            crate::descriptor::host::levels_waiting(staging.memory as u64);
        }
    });
}

fn record_upload(
    list: &HostList,
    resource: This,
    texture: &ID3D12Resource,
    staging: &Staging,
    source: &ID3D12Resource,
    base: u64,
) {
    use std::sync::atomic::Ordering::Relaxed;
    const COPY_DEST: u32 = 0x400;
    // A texture cut from a block may lie where another did: what the card
    // holds of that one is done with before the first upload into this one.
    if crate::pool::on() && !staging.uploaded.load(Relaxed) {
        let aliasing = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: std::mem::ManuallyDrop::new(None),
                    // SAFETY: a borrowed pointer the barrier never releases.
                    pResourceAfter: unsafe { std::mem::transmute_copy(texture) },
                }),
            },
        };
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[aliasing]) };
    }
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The staging/plane upload buffer lives until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (source.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        base + footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + base + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: staging.first - staging.level0 + i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(source) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: base + footprint.Offset,
                    ..*footprint
                },
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

fn fill_plane(
    layout: &oathkeep_formats::texture::Layout,
    at: usize,
    len: usize,
    dst: usize,
    bytes: usize,
    pitch: usize,
) {
    let _pin = crate::watch::title_memory_reading();
    // SAFETY: the list owns this full upload allocation until its GPU work completes.
    let out = unsafe { std::slice::from_raw_parts_mut(dst as *mut u8, bytes) };
    let mip = &layout.mips[0];
    if mip.offset == 0
        && mip.pitch as usize * layout.desc.element_bytes as usize == pitch
        && bytes <= len
        && crate::host::read_into(at as *const _, out)
    {
        return;
    }
    with_read(len, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), at as *const u8, len, false);
        if let Err(error) = layout.detile(read, 0, 0, out, pitch, bytes) {
            out.fill(0);
            once!("could not copy the linear plane into the list's upload buffer: {error:?}");
        }
    });
}

/// Levels on demand: the levels a list claimed for its zeros
/// (`crate::tiles::Tiled::claim`), held by the list's submission-time
/// step. A list that is reset without having been submitted gives the
/// claim up, or the next list that writes those levels would send no zeros
/// and what its write does not cover would be what the tiles' last owner
/// left.
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

/// Records copies of zeros into every subresource of `texture`, which is in
/// the copy destination state.
pub(crate) fn zeros(list: &HostList, host: &crate::host::Host, texture: &ID3D12Resource) {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(d.DepthOrArraySize) };
    let count = u32::from(d.MipLevels) * layers;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut bytes = 0u64;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device
            .GetCopyableFootprints(&d, 0, count, 0, Some(footprints.as_mut_ptr()), None, None, Some(&mut bytes));
    }
    if bytes == 0 || bytes == u64::MAX {
        return;
    }
    let Some((buffer, data)) = list.upload(host, bytes) else {
        return;
    };
    // SAFETY: a mapped buffer of `bytes` bytes, just made.
    unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above; the list holds the buffer until it is reset.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

/// Levels on demand (`crate::tiles`): the card itself is about to write
/// title texture `resource` in `list`.
pub(crate) fn card_writes(list: &HostList, resource: This, texture: &ID3D12Resource, level: Option<u32>) {
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
    // Of them, the ones whose zeros could not be recorded.
    let mut unzeroed = if host().is_none() { fresh } else { 0 };
    if fresh != 0
        && let Some(host) = host()
    {
        const COPY_DEST: u32 = 0x400;
        crate::list::ensure_state(list, resource, COPY_DEST);
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        for level in (0..u32::from(d.MipLevels).min(32)).filter(|&level| fresh & (1u32 << level) != 0) {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut bytes = 0u64;
            // SAFETY: outputs for one subresource.
            unsafe {
                host.device
                    .GetCopyableFootprints(&d, level, 1, 0, Some(&mut footprint), None, None, Some(&mut bytes));
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
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: level },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above; the list holds the buffer until it is reset.
                pResource: unsafe { std::mem::transmute_copy(&buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
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
