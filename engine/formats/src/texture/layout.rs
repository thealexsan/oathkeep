use super::swizzle::{EMPTY_PATTERN, SwizzlePattern, shared_pattern};
use super::{Desc, Dimension, Error, Swizzle, dim, padded_dim};

/// Where one mip lives inside a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mip {
    /// Size in elements (depth 1 unless the texture is 3D).
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    /// Padded size in elements: whole blocks, or the tail's share of one.
    pub pitch: u32,
    pub rows: u32,
    pub depth_pitch: u32,
    /// Byte offset of its first block in the slice (0 in the tail).
    pub offset: u64,
    /// In the mip tail: byte offset inside the tail block and the element
    /// position the mip starts at.
    pub tail: Option<Tail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    pub offset: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub desc: Desc,
    /// Block dimensions in elements (linear: the pitch alignment, 1, 1).
    pub block: [u32; 3],
    /// 3D blocks span `block[2]` depth slices (GFX10 3D `S` and `D` modes).
    pub thick: bool,
    /// Array slices, depth slabs of `block[2]` slices (thick), or depth
    /// slices (thin and linear 3D).
    pub slices: u32,
    /// Bytes of one slice: its whole mip chain.
    pub slice_size: u64,
    pub size: u64,
    pub alignment: u64,
    /// The first mip in the tail; `mips.len()` without one.
    pub first_tail_mip: u32,
    pub mips: Vec<Mip>,
    /// Byte offsets inside a block for each coordinate, per axis.
    /// Empty for linear and unsupported modes.
    pub(super) pattern: &'static SwizzlePattern,
}

const MAX_MIPS: u32 = 16;

/// The largest width, height and depth (or array size) accepted, in texels.
/// GFX10's image descriptors hold at most 16384 texels per side, so no
/// texture the GPU can use is refused. The bound also keeps every size
/// computed below within `u64` (and its `u32` steps), which unchecked
/// values from a title would otherwise overflow.
pub(super) const MAX_EXTENT: u32 = 1 << 14;

/// 256-byte micro tile dimensions by log2 element size, 2D and 3D.
const BLOCK_256_2D: [[u32; 2]; 5] = [[16, 16], [16, 8], [8, 8], [8, 4], [4, 4]];
const BLOCK_256_3D: [[u32; 3]; 5] = [[8, 4, 8], [4, 4, 8], [4, 4, 4], [4, 2, 4], [2, 2, 4]];
const BLOCK_1K_3D: [[u32; 3]; 5] = [[16, 8, 8], [8, 8, 8], [8, 8, 4], [8, 4, 4], [4, 4, 4]];

impl Layout {
    pub fn new(desc: &Desc) -> Result<Layout, Error> {
        let d = *desc;
        let block_log2 = d.swizzle.block_log2().ok_or(Error::Invalid)?;
        let valid = (1..=MAX_EXTENT).contains(&d.width)
            && (1..=MAX_EXTENT).contains(&d.height)
            && (1..=MAX_EXTENT).contains(&d.depth)
            && (1..=MAX_MIPS).contains(&d.mips)
            && d.element_bytes.is_power_of_two()
            && d.element_bytes <= 16
            && matches!(d.block, 1 | 4)
            && d.samples.is_power_of_two()
            && d.samples <= 8
            && (d.samples == 1
                || (d.dimension == Dimension::Tex2d && d.mips == 1 && d.swizzle != Swizzle::LINEAR))
            && (d.dimension != Dimension::Tex1d || d.height == 1)
            && (d.dimension != Dimension::Tex3d
                || matches!(d.swizzle.0, 0 | 5 | 9 | 17 | 21 | 24..=27));
        if !valid {
            return Err(Error::Invalid);
        }
        if block_log2 == 0 {
            Ok(Layout::linear(d))
        } else {
            Ok(Layout::tiled(d, block_log2))
        }
    }

    /// `HwlComputeSurfaceInfoLinear`: rows aligned to 256 bytes, mips packed
    /// smallest first, a 3D texture's depth slices each hold the chain.
    fn linear(d: Desc) -> Layout {
        let bpe = u64::from(d.element_bytes);
        let align = 256 / d.element_bytes;
        let three_d = d.dimension == Dimension::Tex3d;
        let mut mips = vec![];
        let mut offset = 0;
        for i in (0..d.mips).rev() {
            let h = padded_dim(d.height, i, d.block);
            let pitch = padded_dim(d.width, i, d.block).next_multiple_of(align);
            mips.push(Mip {
                width: dim(d.width, i, d.block),
                height: dim(d.height, i, d.block),
                depth: if three_d { dim(d.depth, i, 1) } else { 1 },
                pitch,
                rows: h,
                depth_pitch: if three_d { d.depth } else { 1 },
                offset,
                tail: None,
            });
            offset += u64::from(pitch) * u64::from(h) * bpe;
        }
        mips.reverse();
        Layout {
            desc: d,
            block: [align, 1, 1],
            thick: false,
            slices: d.depth,
            slice_size: offset,
            size: offset * u64::from(d.depth),
            alignment: 256,
            first_tail_mip: d.mips,
            mips,
            pattern: &EMPTY_PATTERN,
        }
    }

    fn tiled(d: Desc, block_log2: u32) -> Layout {
        let element_log2 = d.element_bytes.trailing_zeros();
        let bpe = u64::from(d.element_bytes);
        let block_bytes = 1u64 << block_log2;
        let thick = d.dimension == Dimension::Tex3d && (d.swizzle.standard() || d.swizzle.display());
        let block = if thick {
            // ComputeThickBlockDimension
            let amp = (block_log2 - 10) / 3;
            let rest = (block_log2 - 10) % 3;
            let b = BLOCK_1K_3D[element_log2 as usize];
            [b[0] << amp, b[1] << (amp + rest / 2), b[2] << (amp + u32::from(rest != 0))]
        } else {
            // ComputeThinBlockDimension
            let samples_log2 = d.samples.trailing_zeros();
            let elements_log2 = block_log2 - element_log2 - samples_log2;
            let wide = samples_log2 & 1 == 0 || block_log2 & 1 != 0;
            let w = (elements_log2 + u32::from(wide)) / 2;
            [1 << w, 1 << (elements_log2 - w), 1]
        };
        let slices = d.depth.next_multiple_of(block[2]);
        let three_d = d.dimension == Dimension::Tex3d;
        // Mip depths placed by a thick layout (thin ones store depth as
        // slices).
        let mip_depth = |i: u32| if thick { padded_dim(d.depth, i, 1) } else { 1 };
        let depth_of = |i: u32| if three_d { dim(d.depth, i, 1) } else { 1 };
        let (width, height) = (|i| dim(d.width, i, d.block), |i| dim(d.height, i, d.block));
        let padded = |i| (padded_dim(d.width, i, d.block), padded_dim(d.height, i, d.block));
        let mut mips = Vec::with_capacity(d.mips as usize);
        let mut first_tail_mip = d.mips;
        let mut slice_size = 0u64;
        if d.mips == 1 {
            mips.push(Mip {
                width: width(0),
                height: height(0),
                depth: depth_of(0),
                pitch: width(0).next_multiple_of(block[0]),
                rows: height(0).next_multiple_of(block[1]),
                depth_pitch: if three_d { slices } else { 1 },
                offset: 0,
                tail: None,
            });
            slice_size = u64::from(mips[0].pitch)
                * u64::from(mips[0].rows)
                * bpe
                * u64::from(d.samples);
        } else {
            // GetMipTailDim: the largest mip the tail takes.
            let mut tail_max = block;
            if thick {
                match block_log2 % 3 {
                    0 => tail_max[1] >>= 1,
                    1 => tail_max[0] >>= 1,
                    _ => tail_max[2] >>= 1,
                }
            } else {
                tail_max[0] >>= 1;
            }
            // GetMaxNumMipsInTail; 256-byte blocks have no tail
            // (`ComputeSurfaceInfoMicroTiled`).
            let effective = if thick {
                block_log2 - (block_log2 - 8) / 3
            } else {
                block_log2
            };
            let max_in_tail = match effective {
                8 => 0,
                9..=11 => 1 + (1 << (effective - 9)),
                _ => effective - 4,
            };
            // dsMipmapHtileFix: 8- and 16-bit depth enter the tail as 32-bit.
            let mut fixed = tail_max;
            if d.swizzle.z_order() && element_log2 <= 1 {
                let b = BLOCK_256_2D[element_log2 as usize];
                fixed[0] /= b[0] / BLOCK_256_2D[2][0];
                fixed[1] /= b[1] / BLOCK_256_2D[2][1];
            }
            let mut sizes = vec![];
            for i in 0..d.mips {
                let (w, h) = padded(i);
                if w <= fixed[0] && h <= fixed[1] && d.mips - i <= max_in_tail {
                    first_tail_mip = i;
                    slice_size += block_bytes / u64::from(block[2]);
                    break;
                }
                let pitch = w.next_multiple_of(block[0]);
                let rows = h.next_multiple_of(block[1]);
                let depth = mip_depth(i).next_multiple_of(block[2]);
                let one = u64::from(pitch) * u64::from(rows) * bpe;
                slice_size += one;
                sizes.push(one * u64::from(block[2]));
                mips.push(Mip {
                    width: width(i),
                    height: height(i),
                    depth: depth_of(i),
                    pitch,
                    rows,
                    depth_pitch: if three_d { depth } else { 1 },
                    offset: 0,
                    tail: None,
                });
            }
            // Standard mips from the smallest up, after the tail block.
            let mut offset = if first_tail_mip < d.mips {
                block_bytes
            } else {
                0
            };
            for i in (0..first_tail_mip as usize).rev() {
                mips[i].offset = offset;
                offset += sizes[i];
            }
            let micro = if thick {
                let b = BLOCK_256_3D[element_log2 as usize];
                [b[0], b[1]]
            } else {
                BLOCK_256_2D[element_log2 as usize]
            };
            let (mut pitch, mut rows) = (tail_max[0], tail_max[1]);
            for i in first_tail_mip..d.mips {
                let m = max_in_tail - 1 - (i - first_tail_mip);
                let offset = if m > 6 { 16 << m } else { m << 8 };
                // Address bits 8.. of the offset, odd ones x, even ones y.
                let bits = |from: u32| (0..6).map(|b| ((offset >> (from + 2 * b)) & 1) << b).sum();
                let (mut x, mut y): (u32, u32) = (bits(9), bits(8));
                if block_log2 & 1 != 0 {
                    std::mem::swap(&mut x, &mut y);
                    if element_log2 & 1 != 0 {
                        y = (y << 1) | (x & 1);
                        x >>= 1;
                    }
                }
                mips.push(Mip {
                    width: width(i),
                    height: height(i),
                    depth: depth_of(i),
                    pitch,
                    rows,
                    depth_pitch: if thick {
                        mip_depth(i).next_multiple_of(BLOCK_256_3D[element_log2 as usize][2])
                    } else {
                        1
                    },
                    offset: 0,
                    tail: Some(Tail {
                        offset,
                        x: x * micro[0],
                        y: y * micro[1],
                    }),
                });
                pitch = (pitch >> 1).max(micro[0]);
                rows = (rows >> 1).max(micro[1]);
            }
        }
        // Thick layouts store each depth slab's chain together.
        let slab = if thick { u64::from(block[2]) } else { 1 };
        let slice_size = slice_size * slab;
        let slices = slices / block[2];
        let addressable = !d.swizzle.xor() && d.samples == 1;
        let pattern = if addressable {
            let kind = if thick {
                2
            } else {
                usize::from(d.swizzle.display())
            };
            shared_pattern(kind, element_log2, block_log2)
        } else {
            &EMPTY_PATTERN
        };
        Layout {
            desc: d,
            block,
            thick,
            slices,
            slice_size,
            size: slice_size * u64::from(slices),
            alignment: block_bytes,
            first_tail_mip,
            mips,
            pattern,
        }
    }

    /// Whether [`Layout::offset`] and the copies work for this layout:
    /// linear and the modes without pipe or bank XOR, single-sampled.
    ///
    /// shortcut: no addressing for the XOR modes (`_X`, `_T`) and MSAA;
    /// they need the GPU's pipe and bank configuration (Series X and S
    /// differ). Titles use them for render targets and depth, which the
    /// host renders; add them when a title copies into or out of one.
    pub fn addressable(&self) -> bool {
        self.desc.swizzle == Swizzle::LINEAR || !self.pattern[0].is_empty()
    }

    /// The slice and the depth inside it for `z` (an array slice or a 3D
    /// depth slice).
    pub(super) fn split(&self, z: u32) -> (u32, u32) {
        if self.thick {
            (z / self.block[2], z % self.block[2])
        } else {
            (z, 0)
        }
    }

    /// Byte offset of element (`x`, `y`) of mip `mip` in array slice or
    /// depth slice `z`. Coordinates must lie inside the mip.
    pub fn offset(&self, mip: u32, x: u32, y: u32, z: u32) -> Result<u64, Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = if self.desc.dimension == Dimension::Tex3d {
            m.depth
        } else {
            self.slices
        };
        let inside = x < m.width && y < m.height && z < depth;
        if !inside || !self.addressable() {
            return Err(if inside {
                Error::Unsupported
            } else {
                Error::Invalid
            });
        }
        let (slice, z) = self.split(z);
        let base = u64::from(slice) * self.slice_size + m.offset;
        if self.desc.swizzle == Swizzle::LINEAR {
            let row = u64::from(y) * u64::from(m.pitch) + u64::from(x);
            return Ok(base + row * u64::from(self.desc.element_bytes));
        }
        Ok(base + self.row(m, y, z) + self.column(m, x))
    }

    /// The part of an address that depends on the row (and depth in a
    /// thick block).
    pub(super) fn row(&self, m: &Mip, y: u32, z: u32) -> u64 {
        let [bw, bh, _] = self.block;
        let log2 = self.alignment.trailing_zeros();
        // In the tail the pattern of the shifted position gives the mip's
        // offset inside the tail block.
        let y = y + m.tail.map_or(0, |t| t.y);
        let blocks = u64::from(y / bh) * u64::from(m.pitch / bw);
        (blocks << log2) + u64::from(self.pattern[1][(y % bh) as usize] | self.pattern[2][z as usize])
    }

    /// The part of an address that depends on the column.
    pub(super) fn column(&self, m: &Mip, x: u32) -> u64 {
        let bw = self.block[0];
        let x = x + m.tail.map_or(0, |t| t.x);
        (u64::from(x / bw) << self.alignment.trailing_zeros())
            + u64::from(self.pattern[0][(x % bw) as usize])
    }

    /// Depth slices a subresource of mip `mip` covers: the mip's depth for
    /// 3D textures, else 1 (one array slice).
    pub fn subresource_depth(&self, mip: u32) -> u32 {
        self.mips.get(mip as usize).map_or(0, |m| m.depth)
    }

    /// The bytes from the texture's start that mip `first` and the smaller
    /// ones take: the smaller a mip, the nearer the start it lies, so a
    /// title that streams textures has memory for the smaller mips only and
    /// another texture right behind them. All of the layout where slices
    /// follow each other (each holds its whole chain).
    pub fn bytes_from(&self, first: u32) -> u64 {
        let whole = first == 0
            || self.slices != 1
            || self.thick
            || self.desc.dimension == Dimension::Tex3d
            || self.desc.samples != 1
            || first >= self.desc.mips;
        if whole {
            return self.size;
        }
        self.mips[first as usize..]
            .iter()
            .map(|m| match m.tail {
                Some(_) => self.alignment,
                None => m.offset + u64::from(m.pitch) * u64::from(m.rows) * u64::from(self.desc.element_bytes),
            })
            .max()
            .unwrap_or(self.size)
            .min(self.size)
    }
}
