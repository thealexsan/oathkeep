
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::draw::Root;

#[cfg(test)]
/// A view in the heap for [`Host::dispatch`].
pub enum View<'a> {
    /// An RGBA32F texture SRV (width, height, texels).
    Texture(u32, u32, &'a [[f32; 4]]),
    /// An RGBA32F cube SRV (face size, mip count, texels in D3D subresource order).
    Cube(u32, u16, &'a [[f32; 4]]),
    /// A raw buffer UAV of this many dwords, zeroed.
    RawUav(u32),
    /// An RGBA32F texture UAV (width, height), zeroed.
    TextureUav(u32, u32),
    /// An R32_UINT texture UAV (width, height), zeroed (atomics).
    UintTextureUav(u32, u32),
    /// A typed RGBA16F buffer UAV of this many elements, zeroed.
    HalfUav(u32),
    /// A typed buffer UAV pre-filled with `data` (`format`, bytes per element).
    TypedUav {
        data: &'a [u32],
        format: DXGI_FORMAT,
        elem_bytes: u32,
    },
    /// A constant buffer of these dwords.
    Cbv(&'a [u32]),
    /// A buffer SRV over these dwords.
    Buffer { data: &'a [u32], stride: Stride },
}

/// How [`View::Buffer`] is viewed.
#[cfg(test)]
#[derive(Clone, Copy)]
pub enum Stride {
    Raw,
    /// RGBA32F elements.
    Typed,
    /// Structured, elements of this many bytes.
    Structured(u32),
    /// Typed, elements in this format of this many bytes.
    Format(DXGI_FORMAT, u32),
}

/// Bindings for [`Host::dispatch`]: root arguments, the view heap in order,
/// the sampler heap, and the group counts.
#[cfg(test)]
pub struct Dispatch<'a> {
    pub roots: Vec<(u32, Root)>,
    pub views: &'a [View<'a>],
    pub samplers: &'a [D3D12_SAMPLER_DESC],
    pub groups: [u32; 3],
}

