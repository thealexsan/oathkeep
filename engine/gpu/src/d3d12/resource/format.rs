use oathkeep_formats::texture;

use super::{AllocationInfo, BUFFER, KIB_64, MIB_4, ResourceDesc, TEXTURE_3D};
#[cfg(windows)]
use super::{host_resource, resource};
use crate::d3d12::object::This;

/// Bytes per element and the element's width in texels (4 for block
/// compression), by DXGI format number. `None` for 1-bit and video
/// formats. The console adds 116/117 (10-bit float color), 118-120 (16-bit
/// depth with 8-bit stencil, two planes: 3 bytes per texel), 189 (signed
/// 10-bit color), 190 (two 4-bit channels) and 191 (24/8 color).
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

/// A texture's layout in console memory, as the console's layout library
/// gives it (`oathkeep-hle`'s `xg` serves the same): the description's
/// layout field is `0x100 | swizzle mode`, 1 linear, 0 the library's pick.
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

/// Bytes and alignment a resource takes.
pub(super) fn footprint(desc: &ResourceDesc) -> Option<AllocationInfo> {
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

/// A format as the host has it.
pub(crate) fn pc_format(format: u32) -> u32 {
    match format {
        118 => 45,
        119 => 46,
        120 => 47,
        other => other,
    }
}

/// The format of a texture the host GPU renders to or writes as unordered
/// access, for the title's `format`.
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
                let Some(host) = crate::backend::host() else {
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
                    tracing::info!(format = f, stand_in = to, "the host does not render to this format: a stand-in is used");
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

/// Whether the host copy of the title texture `this` is in a stand-in format.
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

/// The typeless format of a format's family (itself if it has none).
pub(crate) fn family(format: u32) -> u32 {
    const FAMILIES: [(u32, u32); 22] = [
        (1, 4), (5, 8), (9, 14), (15, 18), (19, 22), (23, 25), (27, 32), (33, 38),
        (39, 43), (44, 47), (48, 52), (53, 59), (60, 64), (70, 72), (73, 75), (76, 78),
        (79, 81), (82, 84), (90, 91), (92, 93), (94, 96), (97, 99),
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

/// sRGB formats and their linear twins.
pub(crate) const SRGB: [(u32, u32); 3] = [(29, 28), (91, 87), (93, 88)];

/// The format a view of a title texture must name when its host copy is typeless.
#[cfg(windows)]
pub(crate) fn view_format(this: This) -> Option<u32> {
    let host = host_resource(this)?;
    let own = pc_format(resource(this).desc.format);
    // SAFETY: a live resource.
    let format = unsafe { host.GetDesc() }.Format.0 as u32;
    (format != own && format != target_format(own)).then_some(own)
}

#[cfg(not(windows))]
pub(crate) fn view_format(_: This) -> Option<u32> {
    None
}

/// Depth formats with their typeless family.
pub(crate) const DEPTH_FORMATS: [(u32, u32); 4] = [(20, 19), (40, 39), (45, 44), (55, 53)];

/// Mip levels, the full chain for 0.
pub(super) fn mip_count(desc: &ResourceDesc) -> u32 {
    let depth = if desc.dimension == TEXTURE_3D {
        u64::from(desc.depth_or_array_size)
    } else {
        1
    };
    match desc.mip_levels {
        0 => 64 - desc.width.max(u64::from(desc.height)).max(depth).max(1).leading_zeros(),
        n => u32::from(n),
    }
}
