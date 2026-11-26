use super::{Dimension, Error, Layout, Swizzle};

impl Layout {
    /// Copies one subresource out of tiled memory (the whole texture,
    /// [`Layout::size`] bytes, or the [`Layout::bytes_from`] of the mip)
    /// into linear rows: mip `mip` of array slice
    /// `slice` (3D: every depth slice of the mip, `slice` 0). Rows are
    /// `row_pitch` bytes apart, depth slices `depth_pitch`.
    pub fn detile(
        &self,
        tiled: &[u8],
        mip: u32,
        slice: u32,
        linear: &mut [u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        self.walk::<true>(tiled, linear, mip, slice, row_pitch, depth_pitch)
    }

    /// The reverse of [`Layout::detile`]: linear rows into tiled memory.
    pub fn tile(
        &self,
        tiled: &mut [u8],
        mip: u32,
        slice: u32,
        linear: &[u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        self.walk::<false>(linear, tiled, mip, slice, row_pitch, depth_pitch)
    }

    /// Copies contiguous runs in one direction between the two layouts.
    #[allow(clippy::too_many_arguments)]
    fn walk<const DETILE: bool>(
        &self,
        src: &[u8],
        dst: &mut [u8],
        mip: u32,
        slice: u32,
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = m.depth;
        let first = if self.desc.dimension == Dimension::Tex3d {
            if slice != 0 {
                return Err(Error::Invalid);
            }
            0
        } else {
            slice
        };
        if first >= self.slices {
            return Err(Error::Invalid);
        }
        if !self.addressable() {
            return Err(Error::Unsupported);
        }
        let n = self.desc.element_bytes as usize;
        let bytes = m.width as usize * n;
        let slice_bytes = (m.height as usize - 1)
            .checked_mul(row_pitch)
            .and_then(|offset| offset.checked_add(bytes))
            .ok_or(Error::TooSmall)?;

        // Multiple depth slices must not overwrite each other.
        if depth > 1 && depth_pitch < slice_bytes {
            return Err(Error::TooSmall);
        }

        let need = (depth as usize - 1)
            .checked_mul(depth_pitch)
            .and_then(|offset| offset.checked_add(slice_bytes))
            .ok_or(Error::TooSmall)?;
        let (tiled_len, linear_len) = if DETILE {
            (src.len(), dst.len())
        } else {
            (dst.len(), src.len())
        };
        let tiled_need = if DETILE { self.bytes_from(mip) } else { self.size };
        if tiled_len < tiled_need as usize || linear_len < need || row_pitch < bytes {
            return Err(Error::TooSmall);
        }
        let linear = self.desc.swizzle == Swizzle::LINEAR;
        // The row and depth contribute one common base to every column, so
        // the same contiguous runs repeat on every row, including mip tails.
        let mut runs = Vec::new();
        if !linear {
            let mut first_linear = 0;
            let mut first_tiled = self.column(m, 0) as usize;
            for x in 1..m.width {
                let offset = self.column(m, x) as usize;
                let at = x as usize * n;
                if offset != first_tiled + (at - first_linear) {
                    runs.push((first_tiled, first_linear, at - first_linear));
                    first_tiled = offset;
                    first_linear = at;
                }
            }
            runs.push((first_tiled, first_linear, bytes - first_linear));
        }
        // A mip can end partway through the last run. Check the other runs
        // once so each row can copy them as fixed-size chunks.
        let (body, last) = runs.split_at(runs.len().saturating_sub(1));
        let chunk_size = body.first().map(|run| run.2).filter(|&size| {
            body.iter()
                .all(|&(offset, _, len)| len == size && offset.is_multiple_of(size))
        });
        for z in 0..depth {
            let (s, inner) = self.split(first + z);
            let base = u64::from(s) * self.slice_size + m.offset;
            for y in 0..m.height {
                let linear_row = z as usize * depth_pitch + y as usize * row_pitch;
                if linear {
                    let tiled_row = base + u64::from(y) * u64::from(m.pitch) * n as u64;
                    copy_run::<DETILE>(src, dst, tiled_row as usize, linear_row, bytes);
                } else {
                    let tiled_row = (base + self.row(m, y, inner)) as usize;
                    match chunk_size {
                        Some(8) if tiled_row.is_multiple_of(8) => {
                            copy_runs::<8, DETILE>(src, dst, body, tiled_row, linear_row)
                        }
                        Some(16) if tiled_row.is_multiple_of(16) => {
                            copy_runs::<16, DETILE>(src, dst, body, tiled_row, linear_row)
                        }
                        Some(32) if tiled_row.is_multiple_of(32) => {
                            copy_runs::<32, DETILE>(src, dst, body, tiled_row, linear_row)
                        }
                        _ => copy_runs::<0, DETILE>(src, dst, body, tiled_row, linear_row),
                    }
                    copy_runs::<0, DETILE>(src, dst, last, tiled_row, linear_row);
                }
            }
        }
        Ok(())
    }
}

fn copy_run<const DETILE: bool>(
    src: &[u8],
    dst: &mut [u8],
    tiled: usize,
    linear: usize,
    len: usize,
) {
    let (from, to) = if DETILE {
        (tiled, linear)
    } else {
        (linear, tiled)
    };
    dst[to..][..len].copy_from_slice(&src[from..][..len]);
}

/// A row of equal runs is contiguous in linear memory. Treat it as a
/// slice of arrays so only the tiled chunk needs a bounds check per copy.
/// Use the byte ranges for irregular runs and the last run of each row.
fn copy_runs<const N: usize, const DETILE: bool>(
    src: &[u8],
    dst: &mut [u8],
    runs: &[(usize, usize, usize)],
    tiled_row: usize,
    linear_row: usize,
) {
    if N == 0 {
        for &(tiled, linear, len) in runs {
            copy_run::<DETILE>(src, dst, tiled_row + tiled, linear_row + linear, len);
        }
    } else if DETILE {
        let tiled = src.as_chunks::<N>().0;
        let linear = dst[linear_row..][..runs.len() * N].as_chunks_mut::<N>().0;
        for (out, &(offset, ..)) in linear.iter_mut().zip(runs) {
            *out = tiled[(tiled_row + offset) / N];
        }
    } else {
        let tiled = dst.as_chunks_mut::<N>().0;
        let linear = src[linear_row..][..runs.len() * N].as_chunks::<N>().0;
        for (input, &(offset, ..)) in linear.iter().zip(runs) {
            tiled[(tiled_row + offset) / N] = *input;
        }
    }
}
