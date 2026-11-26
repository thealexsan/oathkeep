//! Texture layouts of the console GPU (RDNA2, GFX10.3): a texture's size and
//! alignment, where each mip and array slice starts, the address of each
//! element, and copies between tiled memory and linear rows.
//!
//! Units: an element is a texel, or a 4x4 block of a block-compressed
//! format. Mips of one array slice (or one depth slab of a thick 3D texture)
//! are stored together, smallest first: the mip tail (the small mips sharing
//! one block) at offset 0, mip 0 last.
//!
//! The algorithm follows AMD's address library (addrlib), as shipped in Mesa
//! under `src/amd/addrlib`: block dimensions, mip placement and mip tails
//! from `gfx10addrlib.cpp` and `addrlib2.cpp`
//! (`ComputeSurfaceInfoMacroTiled`, `ComputeThinBlockDimension`,
//! `GetMipTailDim`, `GetMaxNumMipsInTail`, `HwlComputeSurfaceInfoLinear`,
//! `ComputeSurfaceAddrFromCoordMacroTiled`), and the 256-byte micro tile
//! patterns in `MICRO` from `gfx10SwizzlePattern.h` (the RB+ tables, as on
//! RDNA2). addrlib carries this notice:
//!
//! Copyright (C) 2007-2026 Advanced Micro Devices, Inc. All rights reserved.
//! SPDX-License-Identifier: MIT
//!
//! Permission is hereby granted, free of charge, to any person obtaining a
//! copy of this software and associated documentation files (the
//! "Software"), to deal in the Software without restriction, including
//! without limitation the rights to use, copy, modify, merge, publish,
//! distribute, sublicense, and/or sell copies of the Software, and to permit
//! persons to whom the Software is furnished to do so, subject to the
//! following conditions: the above copyright notice and this permission
//! notice shall be included in all copies or substantial portions of the
//! Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
//! EXPRESS OR IMPLIED.


mod copy;
mod format;
mod layout;
mod swizzle;
#[cfg(test)]
mod tests;

pub use format::{Format, format};
pub use layout::{Layout, Mip, Tail};
pub use swizzle::Swizzle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Tex1d,
    Tex2d,
    Tex3d,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The description makes no texture (zero size, bad element size, a
    /// mode this dimension cannot use, too many mips).
    Invalid,
    /// A valid layout whose addressing is not implemented here.
    Unsupported,
    /// A buffer is smaller than the layout or the rows need.
    TooSmall,
}

/// One plane of a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desc {
    pub dimension: Dimension,
    /// In texels.
    pub width: u32,
    pub height: u32,
    /// Depth of a 3D texture, else the array size.
    pub depth: u32,
    pub mips: u32,
    /// 1, 2, 4, 8 or 16.
    pub element_bytes: u32,
    /// Texels per element side: 1, or 4 for block compression.
    pub block: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
}

/// Elements along one side of mip `mip`, as data and copies see it: texels
/// halve rounding down (to at least 1), then round up to whole elements.
pub(super) fn dim(texels: u32, mip: u32, block: u32) -> u32 {
    (texels >> mip).max(1).div_ceil(block)
}

/// Elements along one side of mip `mip` for placement (padding, tail,
/// offsets): addrlib halves mip 0's elements rounding up, which can be one
/// more than [`dim`] for odd sizes.
pub(super) fn padded_dim(texels: u32, mip: u32, block: u32) -> u32 {
    texels.max(1).div_ceil(block).div_ceil(1 << mip)
}

/// A texture as the console allocates it: a D3D12-style description with
/// a swizzle mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureDesc {
    pub dimension: Dimension,
    /// In texels.
    pub width: u32,
    pub height: u32,
    /// Depth of a 3D texture, else the array size.
    pub depth_or_array: u32,
    /// 0: the full chain.
    pub mips: u32,
    /// DXGI format number.
    pub format: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    /// From the start of the texture.
    pub offset: u64,
    pub stencil: bool,
    pub layout: Layout,
}

/// Every data plane of a texture (depth, then stencil), placed one after
/// the other.
///
/// shortcut: no compression metadata planes (HTILE, CMASK, FMASK, DCC);
/// the console adds them to render targets and depth buffers unless the
/// description denies them, so those are smaller here than on the console.
/// The host GPU renders them, so nothing reads that memory; add the planes
/// if a title sizes a pool from console sizes and runs out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    pub planes: Vec<Plane>,
    pub size: u64,
    pub alignment: u64,
}

impl Texture {
    pub fn new(desc: &TextureDesc) -> Result<Texture, Error> {
        let f = format(desc.format).ok_or(Error::Invalid)?;
        let depth3d = if desc.dimension == Dimension::Tex3d {
            desc.depth_or_array
        } else {
            1
        };
        let mips = match desc.mips {
            0 => 32 - desc.width.max(desc.height).max(depth3d).max(1).leading_zeros(),
            n => n,
        };
        let plane = |bytes| Desc {
            dimension: desc.dimension,
            width: desc.width,
            height: desc.height,
            depth: desc.depth_or_array,
            mips,
            element_bytes: bytes,
            block: f.block,
            samples: desc.samples.max(1),
            swizzle: desc.swizzle,
        };
        let mut planes = vec![Plane {
            offset: 0,
            stencil: false,
            layout: Layout::new(&plane(f.bytes))?,
        }];
        if f.stencil {
            let layout = Layout::new(&plane(1))?;
            let offset = planes[0].layout.size.next_multiple_of(layout.alignment);
            planes.push(Plane {
                offset,
                stencil: true,
                layout,
            });
        }
        let last = &planes[planes.len() - 1];
        let size = last.offset + last.layout.size;
        let alignment = planes.iter().map(|p| p.layout.alignment).max().unwrap_or(1);
        Ok(Texture {
            planes,
            size,
            alignment,
        })
    }
}
