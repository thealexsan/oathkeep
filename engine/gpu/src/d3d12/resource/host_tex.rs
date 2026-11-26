use std::ffi::c_void;

use super::budget::room;
use super::format::{family, pc_format, target_format, texture_layout};
use super::hosted::{HostTexture, KINDS, SERIAL, fills, hosted};
use super::{BUFFER, DEPTH_FORMATS, RESOURCE_FUNCTIONS, ResourceDesc, SRGB};
use crate::d3d12::object::This;

/// Host textures alive (the title's and the twins) and the bytes the host gives them.
pub(crate) static HOST_TEXTURES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

fn host_bytes(texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource) -> u64 {
    let Some(host) = crate::backend::host() else {
        return 0;
    };
    // SAFETY: plain queries on a live resource and device.
    unsafe {
        let desc = texture.GetDesc();
        host.device.GetResourceAllocationInfo(0, &[desc]).SizeInBytes
    }
}

impl HostTexture {
    pub(crate) fn flags(&self) -> u32 {
        self.flags
    }

    pub(crate) fn vram_bytes(&self) -> u64 {
        self.bytes
            .saturating_add(self.tiled.as_ref().map_or(0, crate::memory::tiles::Tiled::given_bytes))
    }

    pub(super) fn new(texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource, state: u32) -> HostTexture {
        use std::sync::atomic::Ordering::Relaxed;
        let bytes = host_bytes(&texture);
        // SAFETY: a live texture.
        let flags = unsafe { texture.GetDesc() }.Flags.0 as u32;
        HOST_TEXTURES[0].fetch_add(1, Relaxed);
        HOST_TEXTURES[1].fetch_add(bytes, Relaxed);
        HostTexture {
            texture,
            state: std::sync::atomic::AtomicU32::new(state),
            bytes,
            filled: false,
            tiled: None,
            base: 0,
            serial: SERIAL.fetch_add(1, Relaxed) + 1,
            grown: Default::default(),
            holds: Default::default(),
            ahead: crate::textures::texture::prefetch::fresh(),
            flags,
        }
    }

    pub(super) fn new_tiled(
        texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
        state: u32,
        tiled: crate::memory::tiles::Tiled,
    ) -> HostTexture {
        HOST_TEXTURES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: a live texture.
        let flags = unsafe { texture.GetDesc() }.Flags.0 as u32;
        HostTexture {
            texture,
            state: std::sync::atomic::AtomicU32::new(state),
            bytes: 0,
            filled: false,
            tiled: Some(tiled),
            base: 0,
            serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
            grown: Default::default(),
            holds: Default::default(),
            ahead: crate::textures::texture::prefetch::fresh(),
            flags,
        }
    }
}

impl Drop for HostTexture {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        HOST_TEXTURES[0].fetch_sub(1, Relaxed);
        HOST_TEXTURES[1].fetch_sub(self.bytes, Relaxed);
        if self.filled {
            KINDS[0].fetch_sub(1, Relaxed);
            KINDS[1].fetch_sub(self.bytes, Relaxed);
        }
    }
}

/// Moves the host texture to `wanted` (a host texture state).
pub(crate) fn host_transition(this: This, wanted: u32) -> Option<(*mut c_void, u32)> {
    use windows::core::Interface;
    let (host, _) = hosted(this, true)?;
    let before = host.state.swap(wanted, std::sync::atomic::Ordering::Relaxed);
    (before != wanted).then(|| (host.texture.as_raw(), before))
}

/// The host GPU's copy of a title resource, if it has one.
pub(crate) fn host_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (host, _) = hosted(this, true)?;
    // SAFETY: the resource holds the host copy until the title releases it or `let_go_idle`.
    Some(unsafe { &*std::ptr::from_ref(&host.texture) })
}

/// Whether `this` is a valid title resource created by this runtime.
pub(crate) fn is_resource(this: This) -> bool {
    let mut table = [0; size_of::<usize>()];
    crate::backend::read_into(this.cast(), &mut table)
        && usize::from_ne_bytes(table) == RESOURCE_FUNCTIONS.0.as_ptr() as usize
}

/// A title texture's host copy, shared, for a list to hold.
pub(crate) fn host_copy(this: This) -> Option<std::sync::Arc<HostTexture>> {
    if this.is_null() {
        return None;
    }
    hosted(this, true).map(|made| made.0)
}

/// A title texture's host copy if it is made; not made here.
pub(crate) fn made_copy(this: This) -> Option<std::sync::Arc<HostTexture>> {
    if this.is_null() {
        return None;
    }
    hosted(this, false).map(|made| made.0)
}

pub(super) fn host_texture(
    desc: &ResourceDesc,
    state: u32,
    clear: *const c_void,
) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    host_texture_as(desc, state, clear, false)
}

pub(super) fn host_texture_as(
    desc: &ResourceDesc,
    state: u32,
    clear: *const c_void,
    reserved: bool,
) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    use windows::Win32::Graphics::Direct3D12::*;
    const PC_FLAGS: u32 = 0xf;
    if desc.dimension == BUFFER {
        return None;
    }
    let host = crate::backend::host()?;
    let mut d = *desc;
    d.layout = 0;
    d.alignment = 0;
    d.flags &= PC_FLAGS;
    const DEPTH: u32 = 2;
    const UNORDERED: u32 = 4;
    if d.flags & DEPTH != 0 {
        if d.flags & UNORDERED != 0 {
            super::twin::DEPTH_UNORDERED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        d.flags &= !UNORDERED;
    }
    d.format = pc_format(d.format);
    const TARGET: u32 = 1;
    if d.flags & (TARGET | UNORDERED) != 0 {
        d.format = target_format(d.format);
    }
    const DENY_SHADER_RESOURCE: u32 = 8;
    if d.flags & DENY_SHADER_RESOURCE == 0
        && let Some(&(_, typeless)) = DEPTH_FORMATS.iter().find(|&&(f, _)| f == d.format)
    {
        d.format = typeless;
    }
    if d.flags & UNORDERED != 0 && SRGB.iter().any(|&(srgb, _)| srgb == d.format) {
        d.format = family(d.format);
    }
    let clear_value = (!clear.is_null()).then(|| {
        // SAFETY: the title passes a whole clear value.
        let mut value = unsafe { clear.cast::<[u32; 5]>().read_unaligned() };
        value[0] = pc_format(value[0]);
        if d.flags & (TARGET | UNORDERED) != 0 {
            value[0] = target_format(value[0]);
        }
        value
    });
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
    if !reserved
        && fills(desc)
        && d.sample_count <= 1
        && clear_value.is_none()
        && texture_layout(desc).is_some_and(|t| !t.planes[0].layout.desc.swizzle.xor())
        // SAFETY: the description has the host layout, as below.
        && let Some(t) = unsafe {
            crate::memory::pool::texture(
                &host.device,
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(crate::backend::legal_for(d.flags, crate::backend::texture_state(state)) as i32),
            )
        }
    {
        crate::backend::name(&t, || {
            format!("texture {}x{} f{} flags {:#x} (in a block)", d.width, d.height, d.format, d.flags)
        });
        return Some(t);
    }
    if !reserved && crate::memory::pool::no_grow() {
        return None;
    }
    let mut texture: Option<ID3D12Resource> = None;
    let made = if reserved {
        d.layout = D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE.0 as u32;
        // SAFETY: as below; a texture the title fills has no clear value.
        unsafe {
            host.device.CreateReservedResource(
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(crate::backend::legal_for(d.flags, crate::backend::texture_state(state)) as i32),
                None,
                &mut texture,
            )
        }
    } else {
        // SAFETY: the description has the host layout; the out pointer is ours.
        unsafe {
            host.device.CreateCommittedResource(
                &heap,
                crate::backend::not_resident(),
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(crate::backend::legal_for(d.flags, crate::backend::texture_state(state)) as i32),
                clear_value.as_ref().map(|v| std::ptr::from_ref(v).cast()),
                &mut texture,
            )
        }
    };
    if let Err(e) = &made {
        crate::memory::residency::allocation_error(&e);
        tracing::warn!(error = %e, desc = ?d, reserved, "no host texture");
    }
    if !reserved
        && made.is_ok()
        && let Some(t) = &texture
        && let Err(e) = crate::backend::resident(&host.device, t).and_then(|()| crate::backend::mapped(&host.device))
    {
        tracing::warn!(error = %e, desc = ?d, "a host texture is not resident");
        texture = None;
    }
    if let Some(t) = &texture {
        crate::backend::name(t, || {
            format!("texture {}x{} f{} flags {:#x}", d.width, d.height, d.format, d.flags)
        });
    }
    texture
}

pub(crate) fn host_tiled(this: This) -> Option<OnDemand> {
    if this.is_null() || !crate::memory::tiles::wanted() {
        return None;
    }
    hosted(this, false).map(|(host, _)| host).filter(|host| host.tiled.is_some()).map(OnDemand)
}

pub(crate) fn first_shown(this: This) -> Option<u32> {
    host_tiled(this)?.tiled()?.first_shown()
}

pub(crate) struct OnDemand(std::sync::Arc<HostTexture>);

// SAFETY: the host texture is named only in queue calls, which any thread may make.
unsafe impl Send for OnDemand {}
// SAFETY: as above.
unsafe impl Sync for OnDemand {}

impl OnDemand {
    pub(crate) fn tiled(&self) -> Option<&crate::memory::tiles::Tiled> {
        self.0.tiled.as_ref()
    }

    pub(crate) fn uploaded(&self, held: u32, memory: u64) -> bool {
        let Some(tiled) = &self.0.tiled else {
            return false;
        };
        let (changed, waiting) = tiled.uploaded(&self.0.texture, held, room(crate::backend::host().and_then(|h| h.video_memory())));
        if changed {
            crate::descriptors::host_heap::levels_changed(memory);
        }
        if waiting {
            crate::memory::tiles::STATS[9].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        waiting
    }

    pub(crate) fn card_wrote(&self, levels: u32, memory: u64) {
        if let Some(tiled) = &self.0.tiled
            && tiled.card_wrote(&self.0.texture, levels)
        {
            crate::descriptors::host_heap::levels_changed(memory);
        }
    }
}
