#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::ptr::NonNull;

use oathkeep_formats::texture;

use crate::object::{
    self, E_INVALIDARG, E_OUTOFMEMORY, HEAP, HRESULT, Iid, Object, RESOURCE, S_OK, Table, This,
    guid,
};

pub(crate) const PAGEABLE_IIDS: [Iid; 3] = [
    guid(
        0x63ee58fb,
        0x1268,
        0x4835,
        [0x86, 0xda, 0xf0, 0x08, 0xce, 0x62, 0xf0, 0xd6],
    ),
    guid(
        0x905db94b,
        0xa00c,
        0x4140,
        [0x9d, 0xf5, 0x2b, 0x64, 0xca, 0x9e, 0xa3, 0x57],
    ),
    guid(
        0xc4fec28f,
        0x7966,
        0x4e95,
        [0x9f, 0x94, 0xf4, 0x31, 0xcb, 0x56, 0xc3, 0xb8],
    ),
];
const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x6b3b2502,
        0x6e51,
        0x45b3,
        [0x90, 0xee, 0x98, 0x84, 0x26, 0x5e, 0x8d, 0xf3],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];
const RESOURCE_IIDS: [Iid; 4] = [
    guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

const KIB_64: u64 = 64 << 10;
const MIB_4: u64 = 4 << 20;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapProperties {
    kind: u32,
    cpu_pages: u32,
    pool: u32,
    creation_nodes: u32,
    visible_nodes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    size: u64,
    properties: HeapProperties,
    alignment: u64,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResourceDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocationInfo {
    size: u64,
    alignment: u64,
}

const _: () = assert!(size_of::<HeapProperties>() == 20);
const _: () = assert!(size_of::<HeapDesc>() == 48);
const _: () = assert!(size_of::<ResourceDesc>() == 56);

const BUFFER: u32 = 1;
const TEXTURE_3D: u32 = 4;

pub(crate) fn element(format: u32) -> Option<(u64, u64)> {
    Some(match format {
        1..=4 => (16, 1),
        5..=8 => (12, 1),
        9..=22 => (8, 1),
        23..=47 | 67 | 87..=93 | 116 | 117 | 189 | 191 => (4, 1),
        118..=120 => (3, 1),
        48..=59 | 68 | 69 | 85 | 86 | 115 => (2, 1),
        60..=65 | 190 => (1, 1),
        70..=72 | 79..=81 => (8, 4),
        73..=78 | 82..=84 | 94..=99 => (16, 4),
        _ => return None,
    })
}

pub(crate) fn texture_layout(desc: &ResourceDesc) -> Option<texture::Texture> {
    use texture::{Dimension, Swizzle, TextureDesc};
    const TEXTURE_1D: u32 = 2;
    const TEXTURE_2D: u32 = 3;
    const RENDER_TARGET: u32 = 1;
    const DEPTH: u32 = 2;
    let dimension = match desc.dimension {
        TEXTURE_1D => Dimension::Tex1d,
        TEXTURE_2D => Dimension::Tex2d,
        TEXTURE_3D => Dimension::Tex3d,
        _ => return None,
    };
    let swizzle = match desc.layout {
        0 if dimension == Dimension::Tex1d => Swizzle::LINEAR,
        0 if desc.flags & DEPTH != 0 => Swizzle::SW_64KB_Z_X,
        0 if desc.flags & RENDER_TARGET != 0 => Swizzle::SW_64KB_R_X,
        0 => Swizzle::SW_4KB_S,
        1 => Swizzle::LINEAR,
        l if l & !0xff == 0x100 => Swizzle(l & 0xff),
        _ => return None,
    };
    texture::Texture::new(&TextureDesc {
        dimension,
        width: u32::try_from(desc.width).ok()?,
        height: desc.height,
        depth_or_array: u32::from(desc.depth_or_array_size),
        mips: u32::from(desc.mip_levels),
        format: desc.format,
        samples: desc.sample_count.max(1),
        swizzle,
    })
    .ok()
}

fn footprint(desc: &ResourceDesc) -> Option<AllocationInfo> {
    if desc.dimension != BUFFER
        && let Some(t) = texture_layout(desc)
    {
        let alignment = desc.alignment.max(t.alignment);
        if !alignment.is_power_of_two() {
            return None;
        }
        return Some(AllocationInfo {
            size: t.size.checked_next_multiple_of(alignment)?,
            alignment,
        });
    }
    let bytes = if desc.dimension == BUFFER {
        desc.width
    } else {
        let (element, block) = element(desc.format)?;
        let (depth, slices) = if desc.dimension == TEXTURE_3D {
            (u64::from(desc.depth_or_array_size), 1)
        } else {
            (1, u64::from(desc.depth_or_array_size))
        };
        let height = u64::from(desc.height);
        let mips = mip_count(desc);
        let mut chain = 0u64;
        for mip in 0..mips.min(64) {
            let w = (desc.width >> mip).max(1).div_ceil(block);
            let h = (height >> mip).max(1).div_ceil(block);
            let d = (depth >> mip).max(1);
            chain = chain.checked_add(w.checked_mul(h)?.checked_mul(d)?.checked_mul(element)?)?;
        }
        chain
            .checked_mul(slices)?
            .checked_mul(u64::from(desc.sample_count.max(1)))?
    };
    if bytes == 0 {
        return None;
    }
    let minimum = if desc.sample_count > 1 { MIB_4 } else { KIB_64 };
    let alignment = desc.alignment.max(minimum);
    if !alignment.is_power_of_two() {
        return None;
    }
    Some(AllocationInfo {
        size: bytes.checked_next_multiple_of(KIB_64)?,
        alignment,
    })
}

pub(crate) fn pc_format(format: u32) -> u32 {
    match format {
        118 => 45,
        119 => 46,
        120 => 47,
        other => other,
    }
}

pub(crate) fn target_format(format: u32) -> u32 {
    const STAND_INS: [(u32, u32); 3] = [(86, 28), (115, 28), (67, 10)];
    let Some(i) = STAND_INS.iter().position(|&(f, _)| f == format) else {
        return format;
    };
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::*;
        static REFUSED: std::sync::OnceLock<[bool; 3]> = std::sync::OnceLock::new();
        let refused = REFUSED.get_or_init(|| {
            STAND_INS.map(|(f, to)| {
                let Some(host) = crate::host::host() else {
                    return false;
                };
                let mut support = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(f as i32),
                    ..Default::default()
                };
                // SAFETY: a plain query into our own structure.
                let asked = unsafe {
                    host.device.CheckFeatureSupport(
                        D3D12_FEATURE_FORMAT_SUPPORT,
                        (&raw mut support).cast(),
                        size_of_val(&support) as u32,
                    )
                };
                let both = D3D12_FORMAT_SUPPORT1_RENDER_TARGET.0
                    | D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0;
                let refused = asked.is_err() || support.Support1.0 & both != both;
                if refused {
                    tracing::info!(
                        format = f,
                        stand_in = to,
                        "the host does not render to this format: a stand-in is used"
                    );
                }
                refused
            })
        });
        if refused[i] {
            return STAND_INS[i].1;
        }
    }
    format
}

#[cfg(windows)]
pub(crate) fn stands_in(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    let own = pc_format(resource(this).desc.format);
    target_format(own) != own
        && host_resource(this)
            // SAFETY: a live resource.
            .is_some_and(|host| unsafe { host.GetDesc() }.Format.0 as u32 == target_format(own))
}

pub(crate) static DEPTH_UNORDERED: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

#[cfg(windows)]
pub(crate) fn depth_and_unordered(this: This) -> bool {
    !this.is_null() && resource(this).desc.flags & 6 == 6
}

#[cfg(windows)]
pub(crate) struct Twin {
    pub(crate) colour: std::sync::Arc<HostTexture>,
    pub(crate) behind: std::sync::atomic::AtomicU32,
    pub(crate) through: Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource>,
    pub(crate) plane: Option<std::sync::Arc<HostTexture>>,
}

#[cfg(windows)]
impl Drop for Twin {
    fn drop(&mut self) {
        TWINS[0].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(windows)]
pub(crate) static TWINS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

#[cfg(windows)]
pub(crate) static PLANES: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(windows)]
pub(crate) fn twin(this: This) -> Option<std::sync::Arc<Twin>> {
    if this.is_null() {
        return None;
    }
    hosted(this, true).and_then(|made| made.1)
}

#[cfg(windows)]
pub(crate) fn twin_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (_, twin) = hosted(this, true)?;
    // SAFETY: as in `host_resource`: the twin lives as long as the host
    // copy, which a depth texture keeps until the title releases it.
    twin.map(|t| unsafe { &*std::ptr::from_ref(&t.colour.texture) })
}

#[cfg(windows)]
pub(crate) fn twin_plane_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (_, twin) = hosted(this, true)?;
    let twin = twin?;
    // SAFETY: as in `twin_resource`.
    twin.plane
        .as_ref()
        .map(|p| unsafe { &*std::ptr::from_ref(&p.texture) })
}

#[cfg(windows)]
fn twin_texture(
    desc: &ResourceDesc,
    depth: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
) -> Option<std::sync::Arc<Twin>> {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::Win32::Graphics::Direct3D12::*;
    const DEPTH_AND_UNORDERED: u32 = 6;
    const UNORDERED: u32 = 4;
    const UNORDERED_ACCESS: u32 = 0x8;
    const ONE_PLANE: [u32; 2] = [39, 53];
    if desc.dimension == BUFFER || desc.flags & DEPTH_AND_UNORDERED != DEPTH_AND_UNORDERED {
        return None;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_DEPTH_TWINS").is_none_or(|v| v != "0")) {
        return None;
    }
    const WITH_STENCIL: [u32; 2] = [19, 44];
    const R8_TYPELESS: u32 = 60;
    let typeless = family(pc_format(desc.format));
    let stencil = WITH_STENCIL.contains(&typeless);
    static STENCIL_ONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let stencil_only = *STENCIL_ONLY
        .get_or_init(|| std::env::var_os("OATHKEEP_DEPTH_TWINS").is_some_and(|v| v == "stencil"));
    if !stencil && stencil_only {
        return None;
    }
    if !(stencil || ONE_PLANE.contains(&typeless)) || desc.sample_count > 1 {
        TWINS[1].fetch_add(1, Relaxed);
        return None;
    }
    let host = crate::host::host()?;
    let mut d = *desc;
    d.layout = 0;
    d.alignment = 0;
    d.flags = UNORDERED;
    d.format = if stencil { R8_TYPELESS } else { typeless };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: the description has the host layout, as in `host_texture`;
    // the out pointer is ours.
    let made = unsafe {
        host.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            (&raw const d).cast(),
            D3D12_RESOURCE_STATES(UNORDERED_ACCESS as i32),
            None,
            &mut texture,
        )
    };
    if let Err(e) = made {
        tracing::warn!(error = %e, desc = ?d, "no colour twin for a depth texture");
    }
    let Some(texture) = texture else {
        TWINS[1].fetch_add(1, Relaxed);
        return None;
    };
    crate::host::name(&texture, || {
        format!("depth twin {}x{} f{}", d.width, d.height, d.format)
    });
    let through = if stencil {
        // SAFETY: a live texture.
        let of = unsafe { depth.GetDesc() };
        let planes = u32::from(of.MipLevels) * u32::from(of.DepthOrArraySize);
        let mut bytes = 0;
        // SAFETY: a plain query; the out pointer is ours.
        unsafe {
            host.device.GetCopyableFootprints(
                &of,
                planes,
                planes,
                0,
                None,
                None,
                None,
                Some(&mut bytes),
            );
        }
        let Some(buffer) = host.uav_buffer(bytes) else {
            TWINS[1].fetch_add(1, Relaxed);
            return None;
        };
        Some(buffer)
    } else {
        None
    };
    static NO_PLANE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let no_plane = *NO_PLANE
        .get_or_init(|| std::env::var_os("OATHKEEP_DEPTH_TWINS").is_some_and(|v| v == "nodepth"));
    let plane = if stencil && !no_plane {
        d.format = if (118..=120).contains(&desc.format) {
            53
        } else {
            39
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: as for the first twin.
        let made = unsafe {
            host.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(UNORDERED_ACCESS as i32),
                None,
                &mut texture,
            )
        };
        if let Err(e) = made {
            tracing::warn!(error = %e, desc = ?d, "no twin for a depth plane");
        }
        texture.map(|texture| {
            crate::host::name(&texture, || {
                format!("depth plane twin {}x{} f{}", d.width, d.height, d.format)
            });
            std::sync::Arc::new(HostTexture::new(texture, UNORDERED_ACCESS))
        })
    } else {
        None
    };
    TWINS[0].fetch_add(1, Relaxed);
    crate::descriptor::host::bump_stage_texture_epoch();
    Some(std::sync::Arc::new(Twin {
        colour: std::sync::Arc::new(HostTexture::new(texture, UNORDERED_ACCESS)),
        behind: std::sync::atomic::AtomicU32::new(0),
        through,
        plane,
    }))
}

pub(crate) fn family(format: u32) -> u32 {
    const FAMILIES: [(u32, u32); 22] = [
        (1, 4),
        (5, 8),
        (9, 14),
        (15, 18),
        (19, 22),
        (23, 25),
        (27, 32),
        (33, 38),
        (39, 43),
        (44, 47),
        (48, 52),
        (53, 59),
        (60, 65),
        (70, 72),
        (73, 75),
        (76, 78),
        (79, 81),
        (82, 84),
        (90, 91),
        (92, 93),
        (94, 96),
        (97, 99),
    ];
    match format {
        87 => 90,
        88 => 92,
        f => FAMILIES
            .iter()
            .find(|&&(first, last)| (first..=last).contains(&f))
            .map_or(f, |&(first, _)| first),
    }
}

pub(crate) const SRGB: [(u32, u32); 3] = [(29, 28), (91, 87), (93, 88)];

pub(crate) fn view_format(this: This) -> Option<u32> {
    let host = host_resource(this)?;
    let own = pc_format(resource(this).desc.format);
    // SAFETY: a live resource.
    let format = unsafe { host.GetDesc() }.Format.0 as u32;
    (format != own && format != target_format(own)).then_some(own)
}

pub(crate) const DEPTH_FORMATS: [(u32, u32); 4] = [(20, 19), (40, 39), (45, 44), (55, 53)];

fn mip_count(desc: &ResourceDesc) -> u32 {
    let depth = if desc.dimension == TEXTURE_3D {
        u64::from(desc.depth_or_array_size)
    } else {
        1
    };
    match desc.mip_levels {
        0 => {
            64 - desc
                .width
                .max(u64::from(desc.height))
                .max(depth)
                .max(1)
                .leading_zeros()
        }
        n => u32::from(n),
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedFootprint {
    pub(crate) offset: u64,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    pub(crate) row_pitch: u32,
}

impl PlacedFootprint {
    pub(crate) fn bytes(&self) -> Option<u64> {
        let (element, block) = element(self.format)?;
        let lines = u64::from(self.height).div_ceil(block) * u64::from(self.depth);
        let last = u64::from(self.width).div_ceil(block) * element;
        Some(u64::from(self.row_pitch) * lines.saturating_sub(1) + last)
    }

    pub(crate) fn host_rows(&self) -> Option<(PlacedFootprint, [u64; 4])> {
        let (element, block) = element(self.format)?;
        let lines = (u64::from(self.height).div_ceil(block) * u64::from(self.depth)).max(1);
        let row = u64::from(self.width).div_ceil(block) * element;
        let pitch = row.max(1).next_multiple_of(256);
        let host = PlacedFootprint {
            offset: 0,
            row_pitch: u32::try_from(pitch).ok()?,
            ..*self
        };
        Some((host, [pitch * lines, row, lines, u64::from(self.row_pitch)]))
    }
}
const _: () = assert!(size_of::<PlacedFootprint>() == 32);

const NO_FOOTPRINT: PlacedFootprint = PlacedFootprint {
    offset: u64::MAX,
    format: 0,
    width: 0,
    height: 0,
    depth: 0,
    row_pitch: 0,
};

fn subresource(desc: &ResourceDesc, index: u32) -> Option<(PlacedFootprint, u32, u64)> {
    const PITCH: u64 = 256;
    if desc.dimension == BUFFER {
        let footprint = PlacedFootprint {
            offset: 0,
            format: 0,
            width: u32::try_from(desc.width).ok()?,
            height: 1,
            depth: 1,
            row_pitch: u32::try_from(desc.width.next_multiple_of(PITCH)).ok()?,
        };
        return (index == 0).then_some((footprint, 1, desc.width));
    }
    let mips = mip_count(desc);
    let slices = if desc.dimension == TEXTURE_3D {
        1
    } else {
        u32::from(desc.depth_or_array_size).max(1)
    };
    let (mip, slice) = (index % mips, index / mips);
    if slice >= slices {
        return None;
    }
    let (element, block) = element(desc.format)?;
    let width = (desc.width >> mip).max(1).next_multiple_of(block);
    let height = (u64::from(desc.height) >> mip)
        .max(1)
        .next_multiple_of(block);
    let depth = if desc.dimension == TEXTURE_3D {
        (u32::from(desc.depth_or_array_size) >> mip).max(1)
    } else {
        1
    };
    let row_size = width / block * element;
    let footprint = PlacedFootprint {
        offset: 0,
        format: desc.format,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        depth,
        row_pitch: u32::try_from(row_size.next_multiple_of(PITCH)).ok()?,
    };
    Some((footprint, u32::try_from(height / block).ok()?, row_size))
}

pub(crate) extern "system" fn copyable_footprints(
    _: This,
    desc: *const ResourceDesc,
    first: u32,
    count: u32,
    base: u64,
    layouts: *mut PlacedFootprint,
    rows: *mut u32,
    row_sizes: *mut u64,
    total: *mut u64,
) {
    const PLACEMENT: u64 = 512;
    // SAFETY: the title passes a description (null is tolerated).
    let desc = unsafe { desc.as_ref() };
    let mut end = base;
    let mut known = desc.is_some();
    for i in 0..count {
        let one = desc.and_then(|d| subresource(d, first + i));
        let (layout, row_count, row_size) = match one {
            Some((mut footprint, row_count, row_size)) => {
                footprint.offset = end.next_multiple_of(PLACEMENT);
                let pitch = u64::from(footprint.row_pitch);
                let lines = u64::from(row_count) * u64::from(footprint.depth);
                end = footprint.offset + pitch * lines.saturating_sub(1) + row_size;
                (footprint, row_count, row_size)
            }
            None => {
                known = false;
                (NO_FOOTPRINT, u32::MAX, u64::MAX)
            }
        };
        let i = i as usize;
        // SAFETY: each output is null or holds `count` entries.
        unsafe {
            if !layouts.is_null() {
                layouts.add(i).write(layout);
            }
            if !rows.is_null() {
                rows.add(i).write(row_count);
            }
            if !row_sizes.is_null() {
                row_sizes.add(i).write(row_size);
            }
        }
    }
    if !known {
        tracing::warn!(?desc, "no footprint for this resource");
    }
    if !total.is_null() {
        // SAFETY: checked non-null.
        unsafe { total.write(if known { end - base } else { u64::MAX }) };
    }
}

pub(crate) extern "system" fn allocation_info(
    out: *mut AllocationInfo,
    _: This,
    _visible: u32,
    count: u32,
    descs: *const ResourceDesc,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descs, count as usize) }
    };
    // SAFETY: the caller passes room for the value.
    unsafe { out.write(place(descs.iter(), |_, _| ())) };
    out
}

#[repr(C)]
pub(crate) struct ResourceDesc1 {
    desc: ResourceDesc,
    mip_region: [u32; 3],
}
const _: () = assert!(size_of::<ResourceDesc1>() == 72);

#[repr(C)]
pub(crate) struct AllocationInfo1 {
    offset: u64,
    alignment: u64,
    size: u64,
}

pub(crate) extern "system" fn allocation_info2(
    out: *mut AllocationInfo,
    _: This,
    _visible: u32,
    count: u32,
    descs: *const ResourceDesc1,
    each: *mut AllocationInfo1,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descs, count as usize) }
    };
    let total = place(descs.iter().map(|d| &d.desc), |i, info| {
        if !each.is_null() {
            // SAFETY: the title passes room for `count` entries.
            unsafe { each.add(i).write(info) };
        }
    });
    // SAFETY: the caller passes room for the value.
    unsafe { out.write(total) };
    out
}

fn place<'a>(
    descs: impl Iterator<Item = &'a ResourceDesc>,
    mut each: impl FnMut(usize, AllocationInfo1),
) -> AllocationInfo {
    let mut total = AllocationInfo {
        size: 0,
        alignment: KIB_64,
    };
    for (i, desc) in descs.enumerate() {
        let Some(one) = footprint(desc) else {
            tracing::warn!(?desc, "no size for this resource");
            each(
                i,
                AllocationInfo1 {
                    offset: u64::MAX,
                    alignment: 0,
                    size: u64::MAX,
                },
            );
            return AllocationInfo {
                size: u64::MAX,
                alignment: 0,
            };
        };
        tracing::debug!(?desc, ?one, "allocation info");
        let offset = total.size.next_multiple_of(one.alignment);
        each(
            i,
            AllocationInfo1 {
                offset,
                alignment: one.alignment,
                size: one.size,
            },
        );
        total.size = offset + one.size;
        total.alignment = total.alignment.max(one.alignment);
    }
    total
}

pub(crate) struct Memory {
    ptr: NonNull<u8>,
    layout: Layout,
    attributes: Option<u64>,
}

pub type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
pub type FreeRoutine = extern "system" fn(*mut c_void, u64);
static ALLOCATOR: std::sync::OnceLock<(AllocRoutine, FreeRoutine)> = std::sync::OnceLock::new();

pub fn use_allocator(alloc: AllocRoutine, free: FreeRoutine) {
    let _ = ALLOCATOR.set((alloc, free));
}

pub type PageMapper = fn(u64, u64, u64) -> bool;
static PAGE_MAPPER: std::sync::OnceLock<PageMapper> = std::sync::OnceLock::new();

pub fn use_page_mapper(mapper: PageMapper) {
    let _ = PAGE_MAPPER.set(mapper);
}

pub(crate) fn page_mapper() -> Option<PageMapper> {
    PAGE_MAPPER.get().copied()
}

fn resource_attributes(info: AllocationInfo, heap_kind: u32) -> u64 {
    const GRAPHICS_ALLOCATOR: u64 = 128 << 16;
    const DEFAULT_HEAP: u32 = 1;
    let pages: u64 = if info.size >= 2 << 20 { 2 } else { 1 };
    let memory: u64 = if heap_kind == DEFAULT_HEAP { 1 } else { 3 };
    let alignment = u64::from(info.alignment.max(1).trailing_zeros());
    GRAPHICS_ALLOCATOR | pages << 14 | alignment << 24 | memory << 29
}

impl Memory {
    fn for_resource(info: AllocationInfo, heap_kind: u32) -> Result<Memory, HRESULT> {
        let Some(&(alloc, _)) = ALLOCATOR.get() else {
            return Memory::new(info);
        };
        let layout = Layout::from_size_align(info.size as usize, info.alignment as usize)
            .map_err(|_| E_INVALIDARG)?;
        let attributes = resource_attributes(info, heap_kind);
        let ptr =
            NonNull::new(alloc(layout.size(), attributes).cast::<u8>()).ok_or(E_OUTOFMEMORY)?;
        // SAFETY: the allocator handed out `size` bytes.
        unsafe { ptr.as_ptr().write_bytes(0, layout.size()) };
        Ok(Memory {
            ptr,
            layout,
            attributes: Some(attributes),
        })
    }

    pub(crate) fn zeroed(size: u64, alignment: u64) -> Result<Memory, HRESULT> {
        Memory::new(AllocationInfo { size, alignment })
    }

    pub(crate) fn address(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    fn new(info: AllocationInfo) -> Result<Memory, HRESULT> {
        let layout = usize::try_from(info.size)
            .ok()
            .zip(usize::try_from(info.alignment).ok())
            .filter(|&(size, _)| size > 0)
            .and_then(|(size, align)| Layout::from_size_align(size, align).ok())
            .ok_or(E_INVALIDARG)?;
        // SAFETY: the size is non-zero.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }).ok_or(E_OUTOFMEMORY)?;
        if info.size >= 32 << 20 {
            tracing::info!(mb = info.size >> 20, at = ?ptr, "memory for the title from our allocator (a heap or a descriptor heap)");
        }
        Ok(Memory {
            ptr,
            layout,
            attributes: None,
        })
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        match (self.attributes, ALLOCATOR.get()) {
            (Some(attributes), Some(&(_, free))) => free(self.ptr.as_ptr().cast(), attributes),
            // SAFETY: allocated in `new` with this layout.
            _ => unsafe { dealloc(self.ptr.as_ptr(), self.layout) },
        }
    }
}

#[repr(C)]
struct Heap {
    object: Object,
    desc: HeapDesc,
    memory: Memory,
}

pub(crate) extern "system" fn create_heap(
    _: This,
    desc: *const HeapDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(
        size = desc.size,
        kind = desc.properties.kind,
        flags = format_args!("{:#x}", desc.flags),
        "heap"
    );
    if desc.alignment != 0 && !desc.alignment.is_power_of_two() {
        return E_INVALIDARG;
    }
    let info = AllocationInfo {
        size: desc.size,
        alignment: desc.alignment.max(KIB_64),
    };
    let memory = match Memory::new(info) {
        Ok(memory) => memory,
        Err(hr) => return hr,
    };
    let heap = Box::new(Heap {
        object: Object::new(&HEAP_FUNCTIONS),
        desc,
        memory,
    });
    object::hand_out(HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn heap_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(HEAP, iid, object::is(iid, &HEAP_IIDS).then_some(this), out)
}
extern "system" fn heap_release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}
extern "system" fn heap_desc(this: This, out: *mut HeapDesc) {
    if !out.is_null() {
        // SAFETY: `this` is a heap; the title passes room for a description.
        unsafe { out.write((*this.cast::<Heap>()).desc) };
    }
}

enum Backing {
    Committed(Memory),
    Placed { heap: This, offset: u64 },
    Address(*mut u8),
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let Backing::Placed { heap, .. } = *self {
            heap_release(heap);
        }
    }
}

#[repr(C)]
struct Resource {
    object: Object,
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    #[cfg(windows)]
    host: Hosted,
}

#[cfg(windows)]
pub(crate) struct HostTexture {
    pub(crate) texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    pub(crate) state: std::sync::atomic::AtomicU32,
    bytes: u64,
    filled: bool,
    allow_uav: bool,
    pub(crate) tiled: Option<crate::tiles::Tiled>,
}

#[cfg(windows)]
type Made = (std::sync::Arc<HostTexture>, Option<std::sync::Arc<Twin>>);

#[cfg(windows)]
struct Hosted {
    made: std::sync::Mutex<Option<Made>>,
    state: u32,
    clear: Option<[u32; 5]>,
    used: std::sync::atomic::AtomicU64,
    refused: std::sync::atomic::AtomicBool,
    gone: std::sync::atomic::AtomicBool,
    placed: std::sync::atomic::AtomicBool,
}

#[cfg(windows)]
pub(crate) static KINDS: [std::sync::atomic::AtomicU64; 7] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 7];

#[cfg(windows)]
static TEXTURES: std::sync::Mutex<std::collections::BTreeSet<usize>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

#[cfg(windows)]
fn textures() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<usize>> {
    TEXTURES.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(windows)]
static TARGETS_AT: std::sync::RwLock<std::collections::BTreeMap<u64, usize>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

#[cfg(windows)]
pub(crate) fn alias_of(this: This) -> Option<This> {
    use std::sync::atomic::Ordering::Relaxed;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if this.is_null()
        || !*ON.get_or_init(|| std::env::var_os("OATHKEEP_ALIAS_COPY").is_none_or(|v| v != "0"))
    {
        return None;
    }
    static WHOLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let whole =
        *WHOLE.get_or_init(|| std::env::var_os("OATHKEEP_ALIAS_COPY").is_none_or(|v| v != "1"));
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
    let targets = TARGETS_AT.read().unwrap_or_else(|e| e.into_inner());
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
    same.then_some(other)
}

#[cfg(windows)]
fn lazy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LAZY_TEXTURES").is_none_or(|v| v != "0"))
}

#[cfg(windows)]
fn fills(desc: &ResourceDesc) -> bool {
    const TARGET_DEPTH_UNORDERED: u32 = 0x7;
    desc.dimension != BUFFER && desc.flags & TARGET_DEPTH_UNORDERED == 0
}

#[cfg(windows)]
pub(crate) fn title_fills(this: This) -> bool {
    !this.is_null() && fills(&resource(this).desc)
}

#[cfg(windows)]
pub(crate) fn host_waits(this: This) -> bool {
    if this.is_null() || !lazy() {
        return false;
    }
    let r = resource(this);
    r.desc.dimension != BUFFER
        && !r.host.refused.load(std::sync::atomic::Ordering::Relaxed)
        && r.host
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
}

#[cfg(windows)]
fn hosted(this: This, make: bool) -> Option<Made> {
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
        let clear = r
            .host
            .clear
            .as_ref()
            .map_or(std::ptr::null(), |c| std::ptr::from_ref(c).cast::<c_void>());
        const TEXTURE_2D: u32 = 3;
        let mut tiled = None;
        let mut texture = None;
        let shape_fits = fills(&r.desc)
            && r.desc.dimension == TEXTURE_2D
            && r.desc.depth_or_array_size == 1
            && r.desc.mip_levels > 1
            && r.desc.sample_count <= 1;
        if shape_fits && r.host.placed.load(Relaxed) && crate::tiles::wanted() {
            let bytes = footprint(&r.desc).map_or(0, |f| f.size);
            let start = r.data() as u64;
            let readable =
                texture_layout(&r.desc).is_some_and(|t| !t.planes[0].layout.desc.swizzle.xor());
            let partly = readable
                && bytes >= crate::tiles::floor()
                && (!crate::texture::committed(r.data().cast_const(), bytes as usize)
                    || crate::descriptor::host::next_start(start, start.saturating_add(bytes))
                        .is_some()
                    || crate::descriptor::host::first_viewed(start).is_some_and(|first| first > 0));
            let reserved = if partly {
                host_texture_as(&r.desc, r.host.state, clear, true)
            } else {
                None
            };
            let cut = reserved
                .as_ref()
                .zip(crate::host::host())
                .and_then(|(t, host)| {
                    crate::tiles::Tiled::make(&host.device, t, u32::from(r.desc.mip_levels))
                });
            if cut.is_some() {
                tiled = cut;
                texture = reserved;
            }
        }
        let texture = match texture {
            Some(texture) => Some(texture),
            None => host_texture(&r.desc, r.host.state, clear),
        };
        let Some(texture) = texture else {
            r.host.refused.store(true, Relaxed);
            return None;
        };
        let twin = twin_texture(&r.desc, &texture);
        if r.desc.flags & 6 == 6 {
            tracing::info!(
                resource = ?this,
                desc = ?r.desc,
                twin = twin.is_some(),
                "depth texture with unordered access"
            );
        }
        let state = crate::host::legal_state(&texture, crate::host::texture_state(r.host.state));
        let mut host = match tiled {
            Some(tiled) => HostTexture::new_tiled(texture, state, tiled),
            None => HostTexture::new(texture, state),
        };
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
        crate::descriptor::texture_made(this as usize);
    }
    r.host.used.store(crate::host::moment(), Relaxed);
    made.clone()
}

#[cfg(all(windows, feature = "diagnostics"))]
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

/// Returns the host D3D12 resource(s) eligible for a targeted
/// `D3D12_RESOURCE_BARRIER_TYPE_UAV` barrier on `this`.
///
/// Never triggers lazy host creation and never returns a resource created
/// without `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` (e.g. a depth texture
/// whose UAV writes target its colour/plane `Twin` instead).
#[cfg(windows)]
pub(crate) fn uav_barrier_targets(
    this: This,
) -> Option<[Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource>; 2]> {
    if this.is_null() {
        return None;
    }
    let (host, twin) = hosted(this, false)?;
    if let Some(twin) = twin {
        let colour = twin.colour.allow_uav.then(|| twin.colour.texture.clone());
        let plane = twin
            .plane
            .as_ref()
            .filter(|p| p.allow_uav)
            .map(|p| p.texture.clone());
        if colour.is_some() || plane.is_some() {
            return Some([colour, plane]);
        }
        return None;
    }
    host.allow_uav.then(|| [Some(host.texture.clone()), None])
}

#[cfg(windows)]
pub(crate) fn commit_headroom() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        std::env::var("OATHKEEP_COMMIT_HEADROOM_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(2048)
            << 20
    })
}

#[cfg(windows)]
pub(crate) fn speculative_room() -> bool {
    let headroom = commit_headroom();
    if headroom > 0
        && let Some(avail) = crate::host::available_commit()
        && avail < headroom.saturating_add(1024 << 20)
    {
        return false;
    }
    room()
}

#[cfg(windows)]
pub(crate) fn room() -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    const FLOOR: u64 = 512 << 20;
    const WITHOUT_BUDGET: u64 = 4 << 30;
    let headroom = commit_headroom();
    if headroom > 0
        && let Some(avail) = crate::host::available_commit()
        && avail < headroom
    {
        return false;
    }
    static LIMIT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        let mb = std::env::var("OATHKEEP_TEXTURE_BUDGET_MB").ok()?;
        mb.trim().parse::<u64>().ok().map(|mb| mb << 20)
    });
    let filled = KINDS[1].load(Relaxed) + crate::tiles::given_total();
    if let Some(limit) = limit {
        return filled < limit;
    }
    let pct = room_percent();
    match crate::host::host().and_then(|h| h.video_memory()) {
        Some([budget, used, ..]) => {
            if used + crate::shadow::card_room_kept() < budget / 100 * pct {
                true
            } else {
                pct >= 90 && filled < FLOOR
            }
        }
        None => filled < WITHOUT_BUDGET,
    }
}

#[cfg(windows)]
pub(crate) fn room_lately() -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tenth = crate::host::now_ms() / 100 + 1;
    let last = LAST.load(Relaxed);
    if last >> 1 == tenth {
        return last & 1 != 0;
    }
    let room = room();
    LAST.store((tenth << 1) | u64::from(room), Relaxed);
    room
}

#[cfg(windows)]
fn room_percent() -> u64 {
    static PERCENT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *PERCENT.get_or_init(|| {
        std::env::var("OATHKEEP_TEXTURE_ROOM_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(90, |percent| percent.clamp(50, 97))
    })
}

#[cfg(windows)]
pub(crate) fn placed_known(address: u64, desc: &[u32; 14]) -> Option<This> {
    let mut key = *desc;
    key[1] = 0;
    key[13] = 0;
    let map = PLACED_TEXTURES.read().unwrap_or_else(|e| e.into_inner());
    map.get(&(address, key))
        .or_else(|| map.get(&(address, *desc)))
        .map(|&r| r as This)
}

#[cfg(windows)]
pub(crate) fn copy_state(this: This) -> Option<(u8, u64, u64)> {
    use std::sync::atomic::Ordering::Relaxed;
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let memory = r.data() as u64;
    let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
    Some(match made.as_ref() {
        Some((host, _)) => (2, host.bytes, memory),
        None => (
            u8::from(r.host.gone.load(Relaxed)),
            footprint(&r.desc).map_or(0, |f| f.size),
            memory,
        ),
    })
}

#[cfg(windows)]
pub(crate) fn card_accounting(
    room: bool,
    due: [u64; 5],
    never: Vec<(u64, u64)>,
    gone: Vec<(u64, u64)>,
) {
    use std::sync::atomic::Ordering::Relaxed;
    static WALKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static BEFORE: [std::sync::atomic::AtomicU64; 5] =
        [const { std::sync::atomic::AtomicU64::new(0) }; 5];
    if WALKING.swap(true, Relaxed) {
        return;
    }
    let began = std::time::Instant::now();
    let mut copies: Vec<(usize, ResourceDesc, u64, bool)> = Vec::new();
    let mut without = [0u64; 2];
    {
        let all = textures();
        for &at in all.iter() {
            // SAFETY: as in `let_go_idle`.
            let r = unsafe { &*(at as *const Resource) };
            if !fills(&r.desc) {
                continue;
            }
            let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
            match made.as_ref() {
                Some((host, _)) => copies.push(match &host.tiled {
                    Some(tiled) => (r.data() as usize, r.desc, tiled.given_bytes(), true),
                    None => (r.data() as usize, r.desc, host.bytes, false),
                }),
                None => without[usize::from(r.host.gone.load(Relaxed))] += 1,
            }
        }
    }
    let kinds = KINDS.each_ref().map(|k| k.load(Relaxed));
    let mut since = [0u64; 5];
    for ((slot, now), before) in since
        .iter_mut()
        .zip([kinds[2], kinds[3], kinds[4], kinds[5], kinds[6]])
        .zip(&BEFORE)
    {
        *slot = now.saturating_sub(before.swap(now, Relaxed));
    }
    let video = crate::host::host().and_then(|h| h.video_memory());
    let noted_us = began.elapsed().as_micros() as u64;
    let walk = move || {
        fn texels(m: &texture::Mip) -> u64 {
            u64::from(m.width) * u64::from(m.height) * u64::from(m.depth)
        }
        let began = std::time::Instant::now();
        let mut c = [0u64; 7];
        let mut on_demand = [0u64; 2];
        for &(memory, desc, bytes, by_level) in &copies {
            c[0] += 1;
            c[1] += bytes;
            if by_level {
                on_demand[0] += 1;
                on_demand[1] += bytes;
                continue;
            }
            let Some(layout) = texture_layout(&desc).map(|t| t.planes[0].layout.clone()) else {
                c[6] += 1;
                continue;
            };
            if layout.size == 0
                || crate::texture::committed(memory as *const u8, layout.size as usize)
            {
                continue;
            }
            c[3] += 1;
            let mips = &layout.mips;
            let tail = (layout.first_tail_mip as usize).min(mips.len());
            let there = |from: u64, to: u64| {
                (0..u64::from(layout.slices)).all(|slice| {
                    let at = memory as u64 + slice * layout.slice_size + from;
                    crate::texture::committed(at as *const u8, (to - from) as usize)
                })
            };
            let all: u64 = mips.iter().map(texels).sum::<u64>().max(1);
            let (mut missing, mut any) = (0u64, false);
            for (level, m) in mips.iter().enumerate().take(tail) {
                let end = if level == 0 {
                    layout.slice_size
                } else {
                    mips[level - 1].offset
                };
                if end <= m.offset {
                    continue;
                }
                if there(m.offset, end) {
                    any = true;
                } else {
                    missing += texels(m);
                    c[4] += u64::from(level == 0);
                }
            }
            if tail < mips.len() {
                if there(0, layout.alignment.min(layout.slice_size).max(1)) {
                    any = true;
                } else {
                    missing += mips[tail..].iter().map(texels).sum::<u64>();
                }
            }
            c[2] += (u128::from(bytes) * u128::from(missing) / u128::from(all)) as u64;
            c[5] += u64::from(!any);
        }
        let mapped = |list: &[(u64, u64)]| {
            let mut n = [0u64; 7];
            for &(at, len) in list {
                n[0] += 1;
                n[1] += len;
                let there = crate::texture::mapped_bytes(at as *const u8, len as usize);
                if len > 0 && there >= len {
                    n[2] += 1;
                    n[3] += len;
                } else if there > 0 {
                    n[4] += 1;
                    n[5] += there;
                } else {
                    n[6] += 1;
                }
            }
            n
        };
        let (never, gone) = (mapped(&never), mapped(&gone));
        let [budget_mb, used_mb] = video.map_or([0, 0], |v| [v[0] >> 20, v[1] >> 20]);
        let [
            copies_n,
            copies_mb,
            unmapped_mb,
            partly,
            largest,
            nothing,
            unknown,
        ] = [c[0], c[1] >> 20, c[2] >> 20, c[3], c[4], c[5], c[6]];
        let [without_never, without_gone] = without;
        let [due_slots, due_uploaded, due_other, alive_n, alive_mb] =
            [due[0], due[1], due[2], due[3], due[4] >> 20];
        let [
            never_n,
            never_mb,
            never_whole,
            never_whole_mb,
            never_part,
            never_part_mb,
            never_none,
        ] = [
            never[0],
            never[1] >> 20,
            never[2],
            never[3] >> 20,
            never[4],
            never[5] >> 20,
            never[6],
        ];
        let [
            gone_n,
            gone_mb,
            gone_whole,
            gone_whole_mb,
            gone_part,
            gone_part_mb,
            gone_none,
        ] = [
            gone[0],
            gone[1] >> 20,
            gone[2],
            gone[3] >> 20,
            gone[4],
            gone[5] >> 20,
            gone[6],
        ];
        let [let_go, let_go_mb, again, again_mb, made] =
            [since[0], since[1] >> 20, since[2], since[3] >> 20, since[4]];
        let walk_ms = began.elapsed().as_millis() as u64;
        let [by_level_n, by_level_mb] = [on_demand[0], on_demand[1] >> 20];
        tracing::info!(
            "card accounting: room {room}, the card {used_mb} MB in use of a budget of {budget_mb} MB; host copies of textures the title \
             fills: {copies_n} alive {copies_mb} MB, of which {unmapped_mb} MB are levels whose title memory is not mapped ({partly} copies \
             with such a level, {largest} of them the largest level, {nothing} with no level mapped; {unknown} without a console layout, not \
             asked); title textures without a host copy: {without_never} never had one, {without_gone} had one let go; due slots: \
             {due_slots} ({due_uploaded} uploaded before, {due_other} of nothing the title fills), their textures: {alive_n} with a host \
             copy alive ({alive_mb} MB, no card memory needed); {never_n} that never had one ({never_mb} MB of title memory: {never_whole} \
             all mapped {never_whole_mb} MB, {never_part} in part {never_part_mb} MB mapped, {never_none} not mapped); {gone_n} whose host \
             copy was let go ({gone_mb} MB: {gone_whole} all mapped {gone_whole_mb} MB, {gone_part} in part {gone_part_mb} MB mapped, \
             {gone_none} not mapped); since the line before: {let_go} host copies let go {let_go_mb} MB, {again} made again {again_mb} MB, \
             {made} made in all; noting took {noted_us} us on the frame's thread, the walk {walk_ms} ms on its own; of the host \
             copies {by_level_n} are made level by level and hold {by_level_mb} MB of tiles (in the MB alive, not in the levels not mapped)"
        );
        WALKING.store(false, Relaxed);
    };
    if std::thread::Builder::new()
        .name("card accounting".into())
        .spawn(walk)
        .is_err()
    {
        WALKING.store(false, Relaxed);
    }
}

#[cfg(windows)]
pub(crate) fn let_go_idle() {
    let_go_idle_with(false);
}

#[cfg(windows)]
pub(crate) fn let_go_idle_urgent() {
    let_go_idle_with(true);
}

#[cfg(windows)]
fn let_go_idle_with(urgent: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    let (idle_ms, idle_frames) = if urgent {
        (0, 1)
    } else {
        let avail_commit = crate::host::available_commit();
        (
            crate::host::vram_idle_ms(crate::host::host().and_then(|h| h.video_memory()))
                .min(crate::host::commit_idle_ms(avail_commit)),
            crate::host::commit_idle_frames(avail_commit),
        )
    };
    let (mut went, mut bytes, mut first) = (0u64, 0u64, None);
    let all = if urgent {
        let Ok(guard) = TEXTURES.try_lock() else {
            return;
        };
        guard
    } else {
        textures()
    };
    for &at in all.iter() {
        // SAFETY: a texture leaves the table before it is freed, under the
        // lock held here (`Drop for Resource`).
        let r = unsafe { &*(at as *const Resource) };
        if !fills(&r.desc) || crate::texture::host_wrote(at) {
            continue;
        }
        let gone = {
            let mut made = if urgent {
                let Ok(guard) = r.host.made.try_lock() else {
                    continue;
                };
                guard
            } else {
                r.host.made.lock().unwrap_or_else(|e| e.into_inner())
            };
            let idle = made.as_ref().is_some_and(|(host, _)| {
                crate::host::long_ago(r.host.used.load(Relaxed), idle_ms, idle_frames)
                    && std::sync::Arc::strong_count(host) == 1
                    && crate::host::references(&host.texture) == 1
            });
            if idle {
                crate::texture::unstage(at as This);
                made.take()
            } else {
                None
            }
        };
        if let Some((host, _)) = gone {
            r.host.gone.store(true, Relaxed);
            KINDS[2].fetch_add(1, Relaxed);
            KINDS[3].fetch_add(host.bytes, Relaxed);
            went += 1;
            bytes += host.bytes;
            if first.is_none() {
                first = Some(at);
            }
            crate::host::retire(host);
        }
    }
    if went > 0 {
        crate::host::trail(|| {
            format!(
                "sweep: {went} host copies of textures the title fills let go ({} KiB), each held by its title texture alone (no list, no slot); the first: title texture {:#x}",
                bytes >> 10,
                first.unwrap_or(0)
            )
        });
    }
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn targets_census() -> [u64; 12] {
    use std::sync::atomic::Ordering::Relaxed;
    let mut c = [0u64; 12];
    let all = textures();
    for &at in all.iter() {
        // SAFETY: as in `let_go_idle`.
        let r = unsafe { &*(at as *const Resource) };
        let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
        let Some((host, twin)) = made.as_ref() else {
            continue;
        };
        let copied_into = host.filled && crate::texture::host_wrote(at);
        if host.filled && !copied_into {
            continue;
        }
        let bytes = host.bytes + twin.as_ref().map_or(0, |t| t.colour.bytes);
        let mut add = |i: usize| {
            c[i] += 1;
            c[i + 1] += bytes;
        };
        if copied_into {
            add(8);
            continue;
        }
        add(0);
        let placed = r.host.placed.load(Relaxed);
        let unmapped = placed && !crate::texture::committed(r.data().cast_const(), 1);
        let idle = crate::host::long_ago(r.host.used.load(Relaxed), 30_000, 60)
            && std::sync::Arc::strong_count(host) == 1
            && crate::host::references(&host.texture) == 1;
        if unmapped {
            add(2);
        }
        if idle {
            add(4);
        }
        if unmapped && idle {
            add(6);
        }
        if placed {
            add(10);
        }
    }
    c
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn census() -> [u64; 10] {
    const EVERY: u64 = 16;
    let mut count = [0u64; 10];
    let mut placed_filled = 0u64;
    let all = textures();
    for &at in all.iter() {
        // SAFETY: as in `let_go_idle`.
        let r = unsafe { &*(at as *const Resource) };
        let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
        match made.as_ref() {
            Some((host, twin)) => {
                let kind = if host.filled { 4 } else { 0 };
                count[kind] += 1;
                count[kind + 1] += host.bytes + twin.as_ref().map_or(0, |t| t.colour.bytes);
                count[7] += twin.as_ref().map_or(0, |t| t.colour.bytes);
                let placed = r.host.placed.load(std::sync::atomic::Ordering::Relaxed);
                if placed {
                    count[6] += 1;
                }
                if placed && host.filled {
                    placed_filled += 1;
                    if placed_filled.is_multiple_of(EVERY)
                        && !crate::texture::committed(r.data().cast_const(), 1)
                    {
                        count[8] += host.bytes * EVERY;
                        if crate::host::references(&host.texture) > 1 {
                            count[9] += host.bytes * EVERY;
                        }
                    }
                }
            }
            None => {
                count[2] += 1;
                count[3] += footprint(&r.desc).map_or(0, |f| f.size);
            }
        }
    }
    count
}

#[cfg(windows)]
pub(crate) static HOST_TEXTURES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

#[cfg(windows)]
fn host_bytes(texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource) -> u64 {
    let Some(host) = crate::host::host() else {
        return 0;
    };
    // SAFETY: plain queries on a live resource and device.
    unsafe {
        let desc = texture.GetDesc();
        host.device
            .GetResourceAllocationInfo(0, &[desc])
            .SizeInBytes
    }
}

#[cfg(windows)]
impl HostTexture {
    fn new(
        texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
        state: u32,
    ) -> HostTexture {
        use std::sync::atomic::Ordering::Relaxed;
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        let bytes = host_bytes(&texture);
        // SAFETY: `texture` is a live host D3D12 resource.
        let allow_uav = unsafe { texture.GetDesc() }
            .Flags
            .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        HOST_TEXTURES[0].fetch_add(1, Relaxed);
        HOST_TEXTURES[1].fetch_add(bytes, Relaxed);
        HostTexture {
            texture,
            state: std::sync::atomic::AtomicU32::new(state),
            bytes,
            filled: false,
            allow_uav,
            tiled: None,
        }
    }

    fn new_tiled(
        texture: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
        state: u32,
        tiled: crate::tiles::Tiled,
    ) -> HostTexture {
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        // SAFETY: `texture` is a live host D3D12 resource.
        let allow_uav = unsafe { texture.GetDesc() }
            .Flags
            .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        HOST_TEXTURES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        HostTexture {
            texture,
            state: std::sync::atomic::AtomicU32::new(state),
            bytes: 0,
            filled: false,
            allow_uav,
            tiled: Some(tiled),
        }
    }
}

#[cfg(windows)]
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

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn placed_textures() -> u64 {
    PLACED_TEXTURES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .len() as u64
}

#[cfg(windows)]
pub(crate) fn host_transition(this: This, wanted: u32) -> Option<(*mut c_void, u32)> {
    use windows::core::Interface;
    let (host, _) = hosted(this, true)?;
    let before = host
        .state
        .swap(wanted, std::sync::atomic::Ordering::Relaxed);
    (before != wanted).then(|| (host.texture.as_raw(), before))
}

#[cfg(windows)]
pub(crate) fn host_resource<'a>(
    this: This,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    if this.is_null() {
        return None;
    }
    let (host, _) = hosted(this, true)?;
    // SAFETY: the resource holds the host copy until the title releases it
    // (lists and slots that name the texture hold it past that), or until
    // `let_go_idle` takes it: only from a texture the title fills, that no
    // list and no slot holds and that nothing asked for in seconds (the
    // asking just made is noted under the same lock).
    Some(unsafe { &*std::ptr::from_ref(&host.texture) })
}

#[cfg(windows)]
pub(crate) fn host_copy(this: This) -> Option<std::sync::Arc<HostTexture>> {
    if this.is_null() {
        return None;
    }
    hosted(this, true).map(|made| made.0)
}

#[cfg(windows)]
fn host_texture(
    desc: &ResourceDesc,
    state: u32,
    clear: *const c_void,
) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
    host_texture_as(desc, state, clear, false)
}

#[cfg(windows)]
fn host_texture_as(
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
    let host = crate::host::host()?;
    let mut d = *desc;
    d.layout = 0;
    d.alignment = 0;
    d.flags &= PC_FLAGS;
    const DEPTH: u32 = 2;
    const UNORDERED: u32 = 4;
    if d.flags & DEPTH != 0 {
        if d.flags & UNORDERED != 0 {
            DEPTH_UNORDERED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut texture: Option<ID3D12Resource> = None;
    let mut made = if reserved {
        d.layout = D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE.0 as u32;
        // SAFETY: as below; a texture the title fills has no clear value.
        unsafe {
            host.device.CreateReservedResource(
                (&raw const d).cast(),
                D3D12_RESOURCE_STATES(crate::host::legal_for(
                    d.flags,
                    crate::host::texture_state(state),
                ) as i32),
                None,
                &mut texture,
            )
        }
    } else {
        let initial_state = D3D12_RESOURCE_STATES(crate::host::legal_for(
            d.flags,
            crate::host::texture_state(state),
        ) as i32);
        let clear_ptr = clear_value.as_ref().map(|v| std::ptr::from_ref(v).cast());
        // SAFETY: the description has the host layout (checked size, same
        // fields); the out pointer is ours.
        let res = unsafe {
            host.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                (&raw const d).cast(),
                initial_state,
                clear_ptr,
                &mut texture,
            )
        };
        if let Err(ref e) = res
            && e.code() == windows::Win32::Foundation::E_OUTOFMEMORY
        {
            crate::host::reclaim_for_oom();
            texture = None;
            // SAFETY: same as above.
            let retry = unsafe {
                host.device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    (&raw const d).cast(),
                    initial_state,
                    clear_ptr,
                    &mut texture,
                )
            };
            if retry.is_ok() {
                tracing::info!(
                    width = d.width,
                    height = d.height,
                    format = d.format,
                    "host texture succeeded after reclaim_for_oom"
                );
            }
            retry
        } else {
            res
        }
    };
    if let Err(e) = &mut made {
        tracing::warn!(error = %e, desc = ?d, reserved, "no host texture");
    }
    if let Some(t) = &texture {
        crate::host::name(t, || {
            format!(
                "texture {}x{} f{} flags {:#x}",
                d.width, d.height, d.format, d.flags
            )
        });
    }
    texture
}

#[cfg(windows)]
pub(crate) fn host_tiled(this: This) -> Option<OnDemand> {
    if this.is_null() || !crate::tiles::wanted() {
        return None;
    }
    hosted(this, false)
        .map(|(host, _)| host)
        .filter(|host| host.tiled.is_some())
        .map(OnDemand)
}

#[cfg(windows)]
pub(crate) fn first_shown(this: This) -> Option<u32> {
    host_tiled(this)?.tiled()?.first_shown()
}

#[cfg(windows)]
pub(crate) struct OnDemand(std::sync::Arc<HostTexture>);

// SAFETY: the host texture is named only in queue calls, which any thread
// may make; what it has been given is behind a lock of its own.
#[cfg(windows)]
unsafe impl Send for OnDemand {}
// SAFETY: as above.
#[cfg(windows)]
unsafe impl Sync for OnDemand {}

#[cfg(windows)]
impl OnDemand {
    pub(crate) fn tiled(&self) -> Option<&crate::tiles::Tiled> {
        self.0.tiled.as_ref()
    }

    pub(crate) fn uploaded(&self, held: u32, memory: u64) -> bool {
        let Some(tiled) = &self.0.tiled else {
            return false;
        };
        let (changed, waiting) = tiled.uploaded(&self.0.texture, held, room());
        if changed {
            crate::descriptor::host::levels_changed(memory);
        }
        if waiting {
            crate::tiles::STATS[9].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        waiting
    }

    pub(crate) fn card_wrote(&self, levels: u32, memory: u64) {
        if let Some(tiled) = &self.0.tiled
            && tiled.card_wrote(&self.0.texture, levels)
        {
            crate::descriptor::host::levels_changed(memory);
        }
    }
}

pub(crate) fn texture_memory(this: This) -> Option<(*const u8, ResourceDesc)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension != BUFFER).then(|| (r.data().cast_const(), r.desc))
}

pub(crate) fn buffer_bytes(this: This) -> Option<(*mut u8, u64)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension == BUFFER).then(|| (r.data(), r.desc.width))
}

impl Resource {
    fn data(&self) -> *mut u8 {
        match self.backing {
            Backing::Committed(ref memory) => memory.ptr.as_ptr(),
            // SAFETY: placement checked the range lies inside the heap.
            Backing::Placed { heap, offset } => unsafe {
                (*heap.cast::<Heap>())
                    .memory
                    .ptr
                    .as_ptr()
                    .add(offset as usize)
            },
            Backing::Address(at) => at,
        }
    }
}

fn resource<'a>(this: This) -> &'a Resource {
    // SAFETY: called from resource methods only.
    unsafe { &*this.cast::<Resource>() }
}

#[allow(clippy::too_many_arguments)]
fn hand_out_resource(
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    #[cfg(not(windows))]
    let _ = (state, clear);
    let resource = Box::new(Resource {
        object: Object::new(&RESOURCE_FUNCTIONS),
        desc,
        heap,
        heap_flags,
        backing,
        #[cfg(windows)]
        host: Hosted {
            made: std::sync::Mutex::new(None),
            state,
            // SAFETY: the title passes a whole clear value (the format,
            // then color or depth and stencil: 5 words) or null.
            clear: (!clear.is_null()).then(|| unsafe { clear.cast::<[u32; 5]>().read_unaligned() }),
            used: std::sync::atomic::AtomicU64::new(0),
            refused: std::sync::atomic::AtomicBool::new(false),
            gone: std::sync::atomic::AtomicBool::new(false),
            placed: std::sync::atomic::AtomicBool::new(false),
        },
    });
    #[cfg(windows)]
    if desc.dimension != BUFFER {
        let at = std::ptr::from_ref::<Resource>(&resource) as usize;
        textures().insert(at);
        if !fills(&desc) {
            TARGETS_AT
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(resource.data() as u64, at);
        }
        if !lazy() {
            let _ = hosted(at as This, true);
        }
    }
    #[cfg(windows)]
    if desc.dimension == BUFFER {
        let at = std::ptr::from_ref::<Resource>(&resource) as usize;
        crate::shadow::register(at, resource.data() as u64, desc.width);
    }
    object::hand_out(RESOURCE, resource, iid, &RESOURCE_IIDS, out)
}

#[cfg(windows)]
impl Drop for Resource {
    fn drop(&mut self) {
        if self.desc.dimension != BUFFER {
            textures().remove(&(std::ptr::from_ref::<Resource>(self) as usize));
            if !fills(&self.desc) {
                let mut targets = TARGETS_AT.write().unwrap_or_else(|e| e.into_inner());
                let at = self.data() as u64;
                if targets.get(&at) == Some(&(std::ptr::from_ref::<Resource>(self) as usize)) {
                    targets.remove(&at);
                }
            }
        }
        let made = self
            .host
            .made
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        #[cfg(feature = "diagnostics")]
        if made
            .as_ref()
            .is_some_and(|(h, _)| std::sync::Arc::strong_count(h) > 1)
        {
            static OUTLIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = OUTLIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(
                    n,
                    desc = ?self.desc,
                    "a title texture was released while a list not yet reset names its host copy"
                );
            }
        }
        if let Some((host, twin)) = made {
            crate::host::retire(host);
            if let Some(twin) = twin {
                crate::host::retire(twin);
            }
        }
        crate::texture::forget(std::ptr::from_ref::<Resource>(self) as usize);
        crate::descriptor::forget(std::ptr::from_ref::<Resource>(self) as usize);
        if self.desc.dimension == BUFFER {
            crate::shadow::unregister(
                std::ptr::from_ref::<Resource>(self) as usize,
                self.data() as u64,
            );
        }
    }
}

pub(crate) extern "system" fn create_committed(
    _: This,
    heap: *const HeapProperties,
    heap_flags: u32,
    desc: *const ResourceDesc,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes both descriptions (null is tolerated).
    let (Some(&heap), Some(&desc)) = (unsafe { (heap.as_ref(), desc.as_ref()) }) else {
        return E_INVALIDARG;
    };
    let Some(info) = footprint(&desc) else {
        tracing::warn!(?desc, "no size for this resource");
        return E_INVALIDARG;
    };
    tracing::debug!(
        dimension = desc.dimension,
        size = info.size,
        heap = heap.kind,
        "committed resource"
    );
    let memory = match Memory::for_resource(info, heap.kind) {
        Ok(memory) => memory,
        Err(hr) => return hr,
    };
    hand_out_resource(
        desc,
        heap,
        heap_flags,
        Backing::Committed(memory),
        state,
        clear,
        iid,
        out,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) extern "system" fn create_committed3(
    this: This,
    heap: *const HeapProperties,
    heap_flags: u32,
    desc: *const ResourceDesc,
    _layout: u32,
    clear: *const c_void,
    _protected: This,
    _castable_count: u32,
    _castable: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    const COMMON: u32 = 0;
    create_committed(this, heap, heap_flags, desc, COMMON, clear, iid, out)
}

static PAGE_POOLS: std::sync::Mutex<Vec<Option<(u64, u32)>>> = std::sync::Mutex::new(Vec::new());

fn page_pools() -> std::sync::MutexGuard<'static, Vec<Option<(u64, u32)>>> {
    PAGE_POOLS.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) extern "system" fn register_page_pool(
    _: This,
    address: u64,
    pages: u32,
    out: *mut usize,
) -> HRESULT {
    if out.is_null() || pages == 0 || !address.is_multiple_of(KIB_64) {
        return E_INVALIDARG;
    }
    tracing::debug!(address = format_args!("{address:#x}"), pages, "page pool");
    let mut pools = page_pools();
    pools.push(Some((address, pages)));
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(pools.len()) };
    S_OK
}

pub(crate) extern "system" fn unregister_page_pool(_: This, handle: usize) -> HRESULT {
    match page_pools().get_mut(handle.wrapping_sub(1)) {
        Some(pool @ Some(_)) => {
            *pool = None;
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub(crate) extern "system" fn create_at_address(
    _: This,
    address: u64,
    desc: *const ResourceDesc,
    layout: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if address == 0 {
        return E_INVALIDARG;
    }
    tracing::debug!(
        address = format_args!("{address:#x}"),
        dimension = desc.dimension,
        width = desc.width,
        layout,
        "resource at an address"
    );
    const COMMON: u32 = 0;
    const DEFAULT: HeapProperties = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 1,
        visible_nodes: 1,
    };
    hand_out_resource(
        desc,
        DEFAULT,
        0,
        Backing::Address(address as *mut u8),
        COMMON,
        clear,
        iid,
        out,
    )
}

#[cfg(windows)]
static PLACED_TEXTURES: std::sync::RwLock<std::collections::BTreeMap<(u64, [u32; 14]), usize>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

#[cfg(windows)]
static PLACED_RAW: std::sync::Mutex<std::collections::BTreeSet<(u64, [u32; 14])>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

#[cfg(windows)]
static PLACED_TWINS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(windows)]
pub(crate) fn placed_filler_twins() -> u64 {
    PLACED_TWINS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(windows)]
pub(crate) fn placed_bytes(desc: &[u32; 14]) -> Option<u64> {
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe { desc.as_ptr().cast::<ResourceDesc>().read_unaligned() };
    if d.dimension == BUFFER {
        return None;
    }
    footprint(&d).map(|f| f.size)
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn texture_at_made(address: u64, desc: &[u32; 14]) -> Option<This> {
    let mut key = *desc;
    key[1] = 0;
    key[13] = 0;
    let map = PLACED_TEXTURES.read().unwrap_or_else(|e| e.into_inner());
    let found = map
        .get(&(address, key))
        .or_else(|| map.get(&(address, *desc)))?;
    Some(*found as This)
}

#[cfg(windows)]
pub(crate) fn texture_at(address: u64, desc: &[u32; 14]) -> Option<This> {
    static KEPT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let raw = *desc;
    let mut key = *desc;
    if !*KEPT.get_or_init(|| std::env::var_os("OATHKEEP_PLACED_FILLER").is_some_and(|v| v == "1")) {
        key[1] = 0;
        key[13] = 0;
    }
    let desc = &key;
    {
        let map = PLACED_TEXTURES.read().unwrap_or_else(|e| e.into_inner());
        if let Some(&r) = map.get(&(address, *desc)) {
            return Some(r as This);
        }
    }
    let mut map = PLACED_TEXTURES.write().unwrap_or_else(|e| e.into_inner());
    if let Some(&r) = map.get(&(address, *desc)) {
        return Some(r as This);
    }
    if raw != key {
        let mut seen = PLACED_RAW.lock().unwrap_or_else(|e| e.into_inner());
        if seen.insert((address, raw)) {
            let others = seen
                .range((address, [0; 14])..=(address, [u32::MAX; 14]))
                .filter(|(_, r)| {
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
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe { desc.as_ptr().cast::<ResourceDesc>().read_unaligned() };
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
    crate::texture::placed_in_host_written(address, desc);
    resource(out)
        .host
        .placed
        .store(true, std::sync::atomic::Ordering::Relaxed);
    Some(out)
}

pub(crate) extern "system" fn create_placed(
    _: This,
    heap: This,
    offset: u64,
    desc: *const ResourceDesc,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if heap.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes one of our heaps.
    let h = unsafe { &*heap.cast::<Heap>() };
    let fits = footprint(&desc).is_some_and(|info| {
        offset.is_multiple_of(info.alignment)
            && offset
                .checked_add(info.size)
                .is_some_and(|end| end <= h.desc.size)
    });
    if !fits {
        tracing::warn!(
            ?desc,
            offset,
            heap = h.desc.size,
            "placed resource does not fit"
        );
        return E_INVALIDARG;
    }
    object::add_ref(heap);
    let backing = Backing::Placed { heap, offset };
    hand_out_resource(
        desc,
        h.desc.properties,
        h.desc.flags,
        backing,
        state,
        clear,
        iid,
        out,
    )
}

extern "system" fn resource_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        RESOURCE,
        iid,
        object::is(iid, &RESOURCE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn resource_release(this: This) -> u32 {
    object::release_boxed::<Resource>(this)
}
extern "system" fn map(this: This, _sub: u32, _read: *const c_void, out: *mut *mut u8) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(resource(this).data()) };
    }
    S_OK
}
extern "system" fn unmap(_: This, _sub: u32, _written: *const c_void) {}
extern "system" fn resource_desc(this: This, out: *mut ResourceDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(resource(this).desc) };
    }
}
pub(crate) extern "system" fn custom_heap_properties(
    out: *mut HeapProperties,
    _: This,
    nodes: u32,
    kind: u32,
) -> *mut HeapProperties {
    const CUSTOM: u32 = 4;
    const NOT_AVAILABLE: u32 = 1;
    const WRITE_BACK: u32 = 3;
    const L0: u32 = 1;
    let nodes = nodes.max(1);
    let cpu_pages = match kind {
        1 => NOT_AVAILABLE,
        2 | 3 => WRITE_BACK,
        _ => 0,
    };
    let properties = HeapProperties {
        kind: CUSTOM,
        cpu_pages,
        pool: if cpu_pages == 0 { 0 } else { L0 },
        creation_nodes: nodes,
        visible_nodes: nodes,
    };
    // SAFETY: the title passes room for the result (null is tolerated).
    if let Some(out) = unsafe { out.as_mut() } {
        *out = properties;
    }
    out
}

extern "system" fn gpu_address(this: This) -> u64 {
    let r = resource(this);
    let at = r.data() as u64;
    #[cfg(feature = "diagnostics")]
    if at >> 40 != 0 {
        static ONCE: [std::sync::Once; 2] = [std::sync::Once::new(), std::sync::Once::new()];
        let backing = match r.backing {
            Backing::Committed(ref m) => format!("committed, attributes {:?}", m.attributes),
            Backing::Placed { offset, .. } => format!("placed in a heap at {offset:#x}"),
            Backing::Address(_) => "at a title address".into(),
        };
        ONCE[usize::from(r.desc.dimension == BUFFER)].call_once(|| {
            tracing::warn!(address = format_args!("{at:#x}"), backing, desc = ?r.desc, "a GPU address above 40 bits")
        });
    }
    at
}
extern "system" fn heap_properties(
    this: This,
    properties: *mut HeapProperties,
    flags: *mut u32,
) -> HRESULT {
    let r = resource(this);
    // SAFETY: either pointer may be null; otherwise the title passes room.
    unsafe {
        if let Some(p) = properties.as_mut() {
            *p = r.heap;
        }
        if let Some(f) = flags.as_mut() {
            *f = r.heap_flags;
        }
    }
    S_OK
}

static HEAP_FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<HEAP, 24>());
    f[0] = heap_query as *const ();
    f[2] = heap_release as *const ();
    f[7] = object::parent_device as *const ();
    f[8] = heap_desc as *const ();
    Table(f)
};

static RESOURCE_FUNCTIONS: Table<32> = {
    let mut f = object::d3d12_object(object::missing_table::<RESOURCE, 32>());
    f[0] = resource_query as *const ();
    f[2] = resource_release as *const ();
    f[7] = object::parent_device as *const ();
    f[8] = map as *const ();
    f[9] = unmap as *const ();
    f[10] = resource_desc as *const ();
    f[11] = gpu_address as *const ();
    f[14] = heap_properties as *const ();
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    type Release = extern "system" fn(This) -> u32;
    type Map = extern "system" fn(This, u32, *const c_void, *mut *mut u8) -> HRESULT;
    type Address = extern "system" fn(This) -> u64;

    const PROPERTIES: HeapProperties = HeapProperties {
        kind: 1,
        cpu_pages: 0,
        pool: 0,
        creation_nodes: 0,
        visible_nodes: 0,
    };

    fn buffer(width: u64) -> ResourceDesc {
        ResourceDesc {
            dimension: BUFFER,
            alignment: 0,
            width,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 0,
            sample_count: 1,
            sample_quality: 0,
            layout: 1,
            flags: 0,
        }
    }

    fn texture(width: u64, height: u32, format: u32, mips: u16) -> ResourceDesc {
        ResourceDesc {
            dimension: 3,
            width,
            height,
            format,
            mip_levels: mips,
            layout: 0,
            ..buffer(0)
        }
    }

    fn heap(size: u64) -> This {
        let desc = HeapDesc {
            size,
            properties: PROPERTIES,
            alignment: 0,
            flags: 0,
        };
        let mut heap = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &desc, &HEAP_IIDS[0], &mut heap),
            S_OK
        );
        heap
    }

    #[test]
    fn heaps_are_zeroed_aligned_memory() {
        let heap = heap(1 << 20);
        // SAFETY: a heap we just made.
        let memory = unsafe { &(*heap.cast::<Heap>()).memory };
        assert_eq!(memory.ptr.as_ptr() as u64 % KIB_64, 0);
        let mut back = std::mem::MaybeUninit::<HeapDesc>::uninit();
        slot::<extern "system" fn(This, *mut HeapDesc)>(heap, 0x40)(heap, back.as_mut_ptr());
        // SAFETY: written above.
        assert_eq!(unsafe { back.assume_init() }.size, 1 << 20);
        assert_eq!(slot::<Release>(heap, 0x10)(heap), 0);

        let bad = HeapDesc {
            size: 1 << 20,
            properties: PROPERTIES,
            alignment: 3,
            flags: 0,
        };
        let mut out = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &bad, &HEAP_IIDS[0], &mut out),
            E_INVALIDARG
        );
    }

    #[test]
    fn device_setup_heap_properties_and_resources_at_addresses() {
        let device = crate::device::device();
        type Custom =
            extern "system" fn(*mut HeapProperties, This, u32, u32) -> *mut HeapProperties;
        let mut p = PROPERTIES;
        let back = slot::<Custom>(device, 0xd0)(&mut p, device, 1, 2);
        assert_eq!(back, &raw mut p);
        assert_eq!(
            (
                p.kind,
                p.cpu_pages,
                p.pool,
                p.creation_nodes,
                p.visible_nodes
            ),
            (4, 3, 1, 1, 1),
            "upload: custom, write-back, L0"
        );
        slot::<Custom>(device, 0xd0)(&mut p, device, 1, 1);
        assert_eq!(p.cpu_pages, 1, "default: not CPU-visible");

        type At = extern "system" fn(
            This,
            u64,
            *const ResourceDesc,
            u32,
            *const c_void,
            *const Iid,
            *mut This,
        ) -> HRESULT;
        let memory = vec![0u8; 0x1_0000];
        let address = memory.as_ptr() as u64;
        let mut r = std::ptr::null();
        let create = slot::<At>(device, 0x428);
        let desc = buffer(0x1_0000);
        let null = std::ptr::null();
        assert_eq!(
            create(
                device,
                address,
                &desc,
                u32::MAX,
                null,
                &RESOURCE_IIDS[0],
                &mut r
            ),
            0
        );
        assert_eq!(slot::<Address>(r, 0x58)(r), address);
        assert_eq!(slot::<Release>(r, 0x10)(r), 0);
        assert_eq!(memory[0xffff], 0);
        assert_ne!(
            create(device, 0, &desc, u32::MAX, null, &RESOURCE_IIDS[0], &mut r),
            0
        );
    }

    #[test]
    fn sizes_cover_every_mip() {
        let size = |d: &ResourceDesc| footprint(d).map(|i| i.size);
        assert_eq!(size(&buffer(256)), Some(KIB_64));
        let layout = |d: &ResourceDesc| texture_layout(d).unwrap().size;
        let rgba = texture(256, 256, 28, 0);
        assert_eq!(size(&rgba), Some(layout(&rgba).next_multiple_of(4096)));
        let bc7 = footprint(&texture(512, 512, 98, 10));
        let console = AllocationInfo {
            size: 0x56000,
            alignment: 0x1000,
        };
        assert_eq!(bc7, Some(console));
        assert!(layout(&rgba) >= 4 * (256 * 256 * 4) / 3, "every mip");
        assert_eq!(size(&texture(1024, 1024, 71, 1)), Some(512 << 10));
        assert_eq!(size(&texture(16, 16, 66, 1)), None, "1-bit format");
        let linear = ResourceDesc {
            layout: 1,
            ..texture(100, 60, 28, 1)
        };
        assert_eq!(
            texture_layout(&linear).unwrap().planes[0]
                .layout
                .desc
                .swizzle
                .0,
            0
        );
        let display = ResourceDesc {
            layout: 0x10a,
            ..texture(100, 60, 28, 1)
        };
        assert_eq!(
            texture_layout(&display).unwrap().planes[0]
                .layout
                .desc
                .swizzle
                .0,
            10
        );

        let mut info = AllocationInfo {
            size: 0,
            alignment: 0,
        };
        let descs = [buffer(100), texture(256, 256, 28, 0)];
        allocation_info(&mut info, std::ptr::null(), 0, 2, descs.as_ptr());
        assert_eq!(
            info,
            AllocationInfo {
                size: KIB_64 + size(&descs[1]).unwrap(),
                alignment: KIB_64
            }
        );
        allocation_info(&mut info, std::ptr::null(), 0, 1, &texture(1, 1, 999, 1));
        assert_eq!(info.size, u64::MAX);
    }

    #[test]
    fn upload_footprints_follow_the_pc_rules() {
        let null = std::ptr::null();
        let mut layouts = [NO_FOOTPRINT; 2];
        let (mut rows, mut sizes, mut total) = ([0; 2], [0; 2], 0);
        let mut footprints = |desc: &ResourceDesc, first, count| {
            let (l, r, s) = (layouts.as_mut_ptr(), rows.as_mut_ptr(), sizes.as_mut_ptr());
            copyable_footprints(null, desc, first, count, 0, l, r, s, &mut total);
            (layouts, rows, sizes, total)
        };
        let (l, r, s, t) = footprints(&texture(100, 60, 28, 2), 0, 2);
        assert_eq!((l[0].row_pitch, r[0], s[0]), (512, 60, 400));
        assert_eq!(l[1].offset, (512 * 59 + 400u64).next_multiple_of(512));
        assert_eq!((l[1].width, l[1].height, r[1]), (50, 30, 30));
        assert_eq!(t, l[1].offset + 256 * 29 + 200);
        let (_, r, s, _) = footprints(&texture(64, 64, 71, 1), 0, 1);
        assert_eq!((r[0], s[0]), (16, 128));
        let (_, _, _, t) = footprints(&texture(64, 64, 71, 1), 1, 1);
        assert_eq!(t, u64::MAX, "no second subresource");
    }

    #[cfg(windows)]
    #[test]
    fn buffer_data_reaches_the_host_texture() {
        use windows::Win32::Graphics::Direct3D12::*;
        let Some(host) = crate::host::host() else {
            return;
        };
        let null = std::ptr::null();
        let iid = &RESOURCE_IIDS[0];
        const COPY_DEST: u32 = 0x400;
        let (mut image, mut source) = (null, null);
        let rgba = texture(4, 4, 28, 1);
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &rgba,
                COPY_DEST,
                null.cast(),
                iid,
                &mut image
            ),
            S_OK
        );
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &buffer(1024),
                0,
                null.cast(),
                iid,
                &mut source
            ),
            S_OK
        );
        let (data, _) = buffer_bytes(source).unwrap();
        // SAFETY: the buffer holds 1024 bytes.
        let bytes = unsafe { std::slice::from_raw_parts_mut(data, 1024) };
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        let queue_iid = guid(
            0x0ec870a6,
            0x5d7e,
            0x4c22,
            [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
        );
        let allocator_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        let (mut queue, mut allocator, mut list) = (null, null, null);
        let queue_desc = [0u32; 4];
        assert_eq!(
            crate::queue::create(null, queue_desc.as_ptr().cast(), &queue_iid, &mut queue),
            S_OK
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator),
            S_OK
        );
        assert_eq!(
            crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list),
            S_OK
        );

        #[repr(C)]
        struct Location {
            resource: This,
            kind: u32,
            footprint: PlacedFootprint,
        }
        let rows = PlacedFootprint {
            offset: 0,
            format: 28,
            width: 4,
            height: 4,
            depth: 1,
            row_pitch: 256,
        };
        let to = Location {
            resource: image,
            kind: 0,
            footprint: PlacedFootprint { offset: 0, ..rows },
        };
        let from = Location {
            resource: source,
            kind: 1,
            footprint: rows,
        };
        type Copy =
            extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const u8);
        slot::<Copy>(list, 0x80)(list, &to, 0, 0, 0, &from, null.cast());
        slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list);
        slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &list);

        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();
        let host_image = host_resource(image).unwrap();
        let back = host.new_list().unwrap();
        let (_, before) = host_transition(image, 0x800).unwrap();
        assert!(std::sync::Arc::strong_count(&host_copy(image).unwrap()) > 2);
        use windows::core::Interface;
        let out_to = Location {
            resource: readback.as_raw().cast_const().cast(),
            kind: 1,
            footprint: rows,
        };
        let out_from = Location {
            resource: host_image.as_raw().cast_const().cast(),
            kind: 0,
            footprint: to.footprint,
        };
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            back.list.ResourceBarrier(&[crate::host::transition(
                host_image,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            back.list.CopyTextureRegion(
                (&raw const out_to).cast(),
                0,
                0,
                0,
                (&raw const out_from).cast(),
                None,
            );
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer is 1024 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 1024,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
        };
        for row in 0..4 {
            let at = row * 256;
            assert_eq!(read[at..at + 16], bytes[at..at + 16], "row {row}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_depth_texture_and_its_twin_copy_into_each_other() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
        let Some(host) = crate::host::host() else {
            return;
        };
        let null = std::ptr::null();
        const DEPTH_WRITE: u32 = 0x10;
        let desc = ResourceDesc {
            flags: 6,
            ..texture(4, 4, 55, 1)
        };
        let mut depth = null;
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &desc,
                DEPTH_WRITE,
                null.cast(),
                &RESOURCE_IIDS[0],
                &mut depth
            ),
            S_OK
        );
        let host_depth = host_resource(depth).unwrap();
        let Some(twin) = twin_resource(depth) else {
            return;
        };
        // SAFETY: live resources.
        let (d, t) = unsafe { (host_depth.GetDesc(), twin.GetDesc()) };
        assert_eq!((d.Format.0, d.Flags.0), (53, 2));
        assert_eq!((t.Format.0, t.Flags.0), (53, 4));

        let mut pattern = vec![0u8; 1024];
        for row in 0..4 {
            for b in 0..8 {
                pattern[row * 256 + b] = (1 + row * 8 + b) as u8;
            }
        }
        let (upload, data) = host.upload_buffer(2048).unwrap();
        // SAFETY: the buffer's mapping, 2048 bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024);
            std::ptr::write_bytes(data.add(1024), 0, 1024);
        }
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();

        let whole = |texture: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let rows = |buffer: &ID3D12Resource, offset: u64| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: offset,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT(53),
                        Width: 4,
                        Height: 4,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        let (written, tested, source, dest) = (
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let moved = crate::host::transition;
        let list = host.new_list().unwrap();
        // SAFETY: an open list and live resources; the two textures are one
        // size in one format family.
        unsafe {
            let l = &list.list;
            l.ResourceBarrier(&[moved(twin, written, dest), moved(host_depth, tested, dest)]);
            l.CopyTextureRegion(&whole(twin), 0, 0, 0, &rows(&upload, 0), None);
            l.ResourceBarrier(&[moved(twin, dest, source)]);
            l.CopyResource(host_depth, twin);
            l.ResourceBarrier(&[moved(twin, source, dest)]);
            l.CopyTextureRegion(&whole(twin), 0, 0, 0, &rows(&upload, 1024), None);
            l.ResourceBarrier(&[moved(host_depth, dest, source)]);
            l.CopyResource(twin, host_depth);
            l.ResourceBarrier(&[moved(twin, dest, source)]);
            l.CopyTextureRegion(&rows(&readback, 0), 0, 0, 0, &whole(twin), None);
            l.ResourceBarrier(&[
                moved(twin, source, written),
                moved(host_depth, source, tested),
            ]);
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copies finished (execute waits); the buffer is 1024 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 1024,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
        };
        for row in 0..4 {
            let at = row * 256;
            assert_eq!(read[at..at + 8], pattern[at..at + 8], "row {row}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_stencil_planes_twin_reaches_the_plane() {
        use std::sync::atomic::Ordering::Relaxed;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
        let Some(host) = crate::host::host() else {
            return;
        };
        let null = std::ptr::null();
        const DEPTH_WRITE: u32 = 0x10;
        let desc = ResourceDesc {
            flags: 6,
            ..texture(4, 4, 45, 1)
        };
        let mut depth = null;
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &desc,
                DEPTH_WRITE,
                null.cast(),
                &RESOURCE_IIDS[0],
                &mut depth
            ),
            S_OK
        );
        let host_depth = host_resource(depth).unwrap();
        let Some(twin) = twin_resource(depth) else {
            return;
        };
        // SAFETY: live resources.
        let (d, t) = unsafe { (host_depth.GetDesc(), twin.GetDesc()) };
        assert_eq!((d.Format.0, d.Flags.0), (44, 2));
        assert_eq!((t.Format.0, t.Flags.0), (60, 4));

        let mut pattern = vec![0u8; 1024];
        for row in 0..4 {
            for b in 0..4 {
                pattern[row * 256 + b] = (1 + row * 4 + b) as u8;
            }
        }
        let (upload, data) = host.upload_buffer(1024).unwrap();
        // SAFETY: the buffer's mapping, 1024 bytes.
        unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024) };
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 4096,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();
        let mut plane = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        // SAFETY: a plain query writing one footprint.
        unsafe {
            host.device
                .GetCopyableFootprints(&d, 1, 1, 0, Some(&mut plane), None, None, None);
        }
        let pattern_rows = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: DXGI_FORMAT(60),
                Width: 4,
                Height: 4,
                Depth: 1,
                RowPitch: 256,
            },
        };

        let whole = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: index,
            },
        };
        let rows = |buffer: &ID3D12Resource, placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| {
            D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: placed,
                },
            }
        };
        let (written, source, dest) = (
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let moved = crate::host::transition;

        let fill = host.new_list().unwrap();
        // SAFETY: an open list and live resources.
        unsafe {
            let l = &fill.list;
            l.ResourceBarrier(&[moved(twin, written, dest)]);
            l.CopyTextureRegion(&whole(twin, 0), 0, 0, 0, &rows(&upload, pattern_rows), None);
            l.ResourceBarrier(&[moved(twin, dest, written)]);
        }
        fill.close();
        host.execute(&[&fill]);

        let list = host.new_list().unwrap();
        assert!(list.twin_use(depth, true, true));
        list.twins_written();
        list.close();
        host.execute(&[&list]);

        let state = host_copy(depth).unwrap().state.load(Relaxed);
        let state = D3D12_RESOURCE_STATES(state as i32);
        let back = host.new_list().unwrap();
        // SAFETY: an open list and live resources; a whole subresource.
        unsafe {
            let l = &back.list;
            l.ResourceBarrier(&[moved(host_depth, state, source)]);
            let (to, from) = (rows(&readback, plane), whole(host_depth, 1));
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            l.ResourceBarrier(&[moved(host_depth, source, state)]);
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copies finished (execute waits); the buffer is 4096 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 4096,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 4096)
        };
        let pitch = plane.Footprint.RowPitch as usize;
        for row in 0..4 {
            let (at, from) = (row * pitch, row * 256);
            assert_eq!(read[at..at + 4], pattern[from..from + 4], "row {row}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_depth_planes_twin_is_drawn_into_the_plane() {
        use std::sync::atomic::Ordering::Relaxed;
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
        use windows::core::s;
        let Some(host) = crate::host::host() else {
            return;
        };
        let null = std::ptr::null();
        const DEPTH_WRITE: u32 = 0x10;
        let desc = ResourceDesc {
            flags: 6,
            ..texture(4, 4, 118, 1)
        };
        let mut depth = null;
        assert_eq!(
            create_committed(
                null,
                &PROPERTIES,
                0,
                &desc,
                DEPTH_WRITE,
                null.cast(),
                &RESOURCE_IIDS[0],
                &mut depth
            ),
            S_OK
        );
        let host_depth = host_resource(depth).unwrap();
        let Some(plane) = twin_plane_resource(depth) else {
            return;
        };
        // SAFETY: live resources.
        let (d, t) = unsafe { (host_depth.GetDesc(), plane.GetDesc()) };
        assert_eq!((d.Format.0, d.Flags.0), (44, 2));
        assert_eq!((t.Format.0, t.Flags.0), (53, 4));

        const CLASSES: [u16; 16] = [
            8, 65, 128, 129, 65, 8, 129, 128, 0, 255, 65, 64, 129, 130, 8, 65,
        ];
        let mut pattern = vec![0u8; 1024];
        for (i, class) in CLASSES.into_iter().enumerate() {
            let at = i / 4 * 256 + i % 4 * 2;
            pattern[at..at + 2].copy_from_slice(&class.to_le_bytes());
        }
        let (upload, data) = host.upload_buffer(1024).unwrap();
        // SAFETY: the buffer's mapping, 1024 bytes.
        unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024) };
        let buffer = |kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES| {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 1024,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a buffer.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
            }
            made.unwrap()
        };
        let readback = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
        let target = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 4,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT(61),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a texture.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut made,
                    )
                    .unwrap();
            }
            made.unwrap()
        };
        let rows_of = |format: i32| D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: DXGI_FORMAT(format),
                Width: 4,
                Height: 4,
                Depth: 1,
                RowPitch: 256,
            },
        };
        let whole = |texture: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let rows = |buffer: &ID3D12Resource, placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| {
            D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: placed,
                },
            }
        };
        let (written, source, dest, tested, drawn) = (
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let moved = crate::host::transition;

        let fill = host.new_list().unwrap();
        // SAFETY: an open list and live resources.
        unsafe {
            let l = &fill.list;
            l.ResourceBarrier(&[moved(plane, written, dest)]);
            l.CopyTextureRegion(&whole(plane), 0, 0, 0, &rows(&upload, rows_of(53)), None);
            l.ResourceBarrier(&[moved(plane, dest, written)]);
        }
        fill.close();
        host.execute(&[&fill]);

        let list = host.new_list().unwrap();
        assert!(list.twin_use(depth, true, true));
        list.twins_written();
        list.close();
        host.execute(&[&list]);

        const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants = 1, b0)"
cbuffer Drawn : register(b0) { uint number; };
[RootSignature(RS)]
float4 corner(uint i : SV_VertexID) : SV_Position {
    float z = float(number) * asfloat(0x37800080);
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, z, 1.0);
}
float4 lit() : SV_Target { return 1.0; }
"#;
        let vs = crate::host::compile(SOURCE, s!("corner"), s!("vs_5_1")).unwrap();
        let ps = crate::host::compile(SOURCE, s!("lit"), s!("ps_5_1")).unwrap();
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = 0xf;
        let mut formats = [DXGI_FORMAT(0); 8];
        formats[0] = DXGI_FORMAT(61);
        // SAFETY: plain creation; the description points at live data.
        let (signature, pipeline) = unsafe {
            let signature: ID3D12RootSignature =
                host.device.CreateRootSignature(0, &vs[..]).unwrap();
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&signature),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                BlendState: blend,
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                    DepthFunc: D3D12_COMPARISON_FUNC_EQUAL,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                RTVFormats: formats,
                DSVFormat: DXGI_FORMAT(45),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState =
                host.device.CreateGraphicsPipelineState(&desc).unwrap();
            (signature, pipeline)
        };
        let state = host_copy(depth).unwrap().state.load(Relaxed);
        let state = D3D12_RESOURCE_STATES(state as i32);
        let colour = host.rtv(Some(&target), None);
        let depth_view = host.dsv(
            host_depth,
            Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: DXGI_FORMAT(45),
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                ..Default::default()
            }),
        );
        let back = host.new_list().unwrap();
        // SAFETY: an open list and live resources and views.
        unsafe {
            let l = &back.list;
            if state != tested {
                l.ResourceBarrier(&[moved(host_depth, state, tested)]);
            }
            l.ClearRenderTargetView(colour, &[0.0; 4], None);
            l.SetGraphicsRootSignature(&signature);
            l.SetPipelineState(&pipeline);
            l.OMSetRenderTargets(1, Some(&colour), false, Some(&depth_view));
            l.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: 4.0,
                Height: 4.0,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            l.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4,
            }]);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(4));
            for class in [65u32, 129] {
                l.SetGraphicsRoot32BitConstant(0, class, 0);
                l.DrawInstanced(3, 1, 0, 0);
            }
            l.ResourceBarrier(&[moved(&target, drawn, source)]);
            let (to, from) = (rows(&readback, rows_of(61)), whole(&target));
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer is 1024 bytes.
        let read = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: 1024,
                    }),
                    Some(&mut mapped),
                )
                .unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
        };
        for (i, class) in CLASSES.into_iter().enumerate() {
            let covered = read[i / 4 * 256 + i % 4] != 0;
            let drawn = matches!(class, 65 | 129);
            assert_eq!(covered, drawn, "texel {i}, class {class}");
        }
    }

    #[test]
    fn committed_and_placed_resources_map_at_their_gpu_address() {
        let null = std::ptr::null();
        let mut committed = null;
        let hr = create_committed(
            null,
            &PROPERTIES,
            0,
            &buffer(256),
            0,
            null.cast(),
            &RESOURCE_IIDS[0],
            &mut committed,
        );
        assert_eq!(hr, S_OK);
        let mut data = std::ptr::null_mut();
        assert_eq!(
            slot::<Map>(committed, 0x40)(committed, 0, null.cast(), &mut data),
            S_OK
        );
        assert_eq!(slot::<Address>(committed, 0x58)(committed), data as u64);
        // SAFETY: the buffer is at least 256 zeroed bytes.
        assert_eq!(unsafe { *data.add(255) }, 0);
        assert_eq!(slot::<Release>(committed, 0x10)(committed), 0);

        let heap = heap(4 * KIB_64);
        let place = |offset, desc: &ResourceDesc, out: &mut This| {
            create_placed(
                null,
                heap,
                offset,
                desc,
                0,
                null.cast(),
                &RESOURCE_IIDS[0],
                out,
            )
        };
        let mut placed = null;
        assert_eq!(place(KIB_64, &buffer(KIB_64), &mut placed), S_OK);
        // SAFETY: a heap we made.
        let base = unsafe { (*heap.cast::<Heap>()).memory.ptr.as_ptr() } as u64;
        assert_eq!(slot::<Address>(placed, 0x58)(placed), base + KIB_64);
        let mut other = null;
        assert_eq!(
            place(4 * KIB_64, &buffer(1), &mut other),
            E_INVALIDARG,
            "past the end"
        );
        assert_eq!(
            place(100, &buffer(1), &mut other),
            E_INVALIDARG,
            "misaligned"
        );
        assert_eq!(slot::<Release>(heap, 0x10)(heap), 1);
        assert_eq!(slot::<Release>(placed, 0x10)(placed), 0);
    }
}
