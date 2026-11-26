//! `xg_xs.dll`: the console's texture layout library. Titles ask it which
//! swizzle mode a texture should use, how large it is, where each mip and
//! slice lies, and to copy linear data into tiled memory and back. Served
//! from `oathkeep_formats::texture`.
//!
//! The title imports `XGCreateTextureComputer` and
//! `XGSuggestSwizzleMode` and calls these computer methods: Release (0x08),
//! the layout (0x10), copies into (0x40) and out of (0x48) a subresource,
//! and the 64 KiB tiling (0x50). The other slots log and fail.
#![allow(non_snake_case)]
// Title code passes pointers the real library would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod computer;

#[cfg(test)]
mod tests;

pub use computer::Computer;

use std::sync::atomic::AtomicU32;

use oathkeep_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

use computer::{This, VTABLE};

/// `D3D12_RESOURCE_DESC`, whose layout field carries `0x100 | swizzle
/// mode` on the console (1 row major, 0 "pick one").
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ResourceDesc {
    pub dimension: u32,
    pub alignment: u64,
    pub width: u64,
    pub height: u32,
    pub depth_or_array: u16,
    pub mips: u16,
    pub format: u32,
    pub samples: u32,
    pub quality: u32,
    pub layout: u32,
    pub flags: u32,
}
const _: () = assert!(size_of::<ResourceDesc>() == 0x38);

const SWIZZLED: u32 = 0x100;
const ROW_MAJOR: u32 = 1;
const ALLOW_RENDER_TARGET: u32 = 1;
const ALLOW_DEPTH_STENCIL: u32 = 2;
const TEXTURE_1D: u32 = 2;
const TEXTURE_3D: u32 = 4;

/// The mode the console's library picks for a description: linear for 1D,
/// the XOR render and depth modes for targets, else 4 KiB standard
/// (`.ttpf` textures without a mode of their own use it).
///
/// shortcut: the rule of the library's older optimal-mode function (any
/// size, 2D, 3D and cubes alike); `XGSuggestSwizzleMode` itself is newer
/// than every copy we could check. Revisit if streamed textures come out
/// scrambled.
fn suggested(desc: &ResourceDesc) -> Swizzle {
    if desc.dimension == TEXTURE_1D {
        Swizzle::LINEAR
    } else if desc.flags & ALLOW_DEPTH_STENCIL != 0 {
        Swizzle::SW_64KB_Z_X
    } else if desc.flags & ALLOW_RENDER_TARGET != 0 {
        Swizzle::SW_64KB_R_X
    } else {
        Swizzle::SW_4KB_S
    }
}

/// `(desc, flags, out layout)`; the title passes flags 0 and puts the
/// result in the description's layout field.
pub extern "system" fn XGSuggestSwizzleMode(
    desc: *const ResourceDesc,
    _flags: u32,
    out: *mut u32,
) -> HRESULT {
    // SAFETY: the title passes a description and room for the layout
    // (either may be null).
    match unsafe { (desc.as_ref(), out.as_mut()) } {
        (Some(desc), Some(out)) => {
            *out = SWIZZLED | suggested(desc).0;
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub extern "system" fn XGCreateTextureComputer(
    desc: *const ResourceDesc,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description and room for a pointer.
    let (Some(&desc), false) = (unsafe { desc.as_ref() }, out.is_null()) else {
        return E_INVALIDARG;
    };
    let Some(texture) = texture(&desc) else {
        tracing::warn!(?desc, "no texture layout");
        return E_INVALIDARG;
    };
    tracing::debug!(
        width = desc.width,
        height = desc.height,
        format = desc.format,
        layout = format_args!("{:#x}", desc.layout),
        size = texture.size,
        "texture computer"
    );
    let mips = texture.planes[0].layout.mips.len() as u32;
    let computer = Box::new(Computer {
        vtable: &VTABLE,
        refs: AtomicU32::new(1),
        desc,
        mips,
        texture,
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(computer)) };
    S_OK
}

fn texture(desc: &ResourceDesc) -> Option<Texture> {
    let dimension = match desc.dimension {
        TEXTURE_1D => Dimension::Tex1d,
        3 => Dimension::Tex2d,
        TEXTURE_3D => Dimension::Tex3d,
        _ => return None,
    };
    let swizzle = match desc.layout {
        0 => suggested(desc),
        ROW_MAJOR => Swizzle::LINEAR,
        l if l & !0xff == SWIZZLED => Swizzle(l & 0xff),
        _ => return None,
    };
    let texture = Texture::new(&TextureDesc {
        dimension,
        width: u32::try_from(desc.width).ok()?,
        height: desc.height,
        depth_or_array: u32::from(desc.depth_or_array),
        mips: u32::from(desc.mips),
        format: desc.format,
        samples: desc.samples.max(1),
        swizzle,
    })
    .ok()?;
    // The layout struct has room for 15 mips.
    (texture.planes[0].layout.mips.len() <= MIPS).then_some(texture)
}

const MIPS: usize = 15;

/// One mip in the layout the title reads (offsets 0x00, 0x08, 0x10 and
/// 0x1c are read by the title; the rest follow the library's
/// older public shape).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct MipOut {
    pub size: u64,
    pub offset: u64,
    pub slice_size: u64,
    pub pitch_texels: u32,
    pub pitch_bytes: u32,
    pub alignment: u32,
    /// Width, height in elements, depth or array size.
    pub padded: [u32; 3],
    pub elements: [u32; 2],
    pub texels: [u32; 2],
    pub depth_or_array: u32,
    pub samples: u32,
    pub swizzle: u32,
    pub bank_rotation: [u64; 2],
    pub slice_depth: u32,
    pub in_tail: u32,
    /// Where a tail mip starts in elements: x bits 0-10, y 11-21.
    pub tail_position: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PlaneOut {
    /// 1 data, 7 depth, 8 stencil (metadata planes are not made).
    pub usage: u32,
    pub size: u64,
    pub slice_size: u64,
    pub offset: u64,
    pub alignment: u64,
    pub element_bytes: u32,
    pub standard_mips: u32,
    pub tail_mips: u32,
    pub mips: [MipOut; MIPS],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LayoutOut {
    pub size: u64,
    pub alignment: u64,
    pub mips: u32,
    pub planes: u32,
    pub plane: [PlaneOut; 4],
    pub dimension: u32,
}
const _: () = assert!(size_of::<MipOut>() == 0x70);
const _: () = assert!(size_of::<PlaneOut>() == 0x6c8);
const _: () = assert!(size_of::<LayoutOut>() == 0x1b40);

#[repr(C)]
pub struct PackedMips {
    pub standard: u8,
    pub packed: u8,
    pub tiles: u32,
    pub start: u32,
}

#[repr(C)]
pub struct SubresourceTiling {
    pub width: u32,
    pub height: u16,
    pub depth: u16,
    pub start: u32,
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xg",
        dlls: &["xg_xs.dll"],
        exports: vec![
            hle_export!(XGCreateTextureComputer),
            hle_export!(XGSuggestSwizzleMode),
        ],
    }
}
