use crate::ShaderError;
use crate::dxbc::u32_at;

const MAX_RTS0_SAMPLERS: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaticSampler {
    pub filter: u32,
    pub address: [u32; 3],
    pub mip_lod_bias: f32,
    pub max_anisotropy: u32,
    pub comparison: u32,
    pub border: u32,
    pub min_lod: f32,
    pub max_lod: f32,
}

impl Eq for StaticSampler {}

impl StaticSampler {
    pub fn from_words(w: [u32; 4]) -> Self {
        let bits = |word: u32, at: u32, n: u32| word >> at & ((1 << n) - 1);
        let address = |m: u32| [1, 2, 3, 5, 3, 5, 4, 5][m as usize];
        let (mag, min, mip) = (bits(w[2], 20, 2), bits(w[2], 22, 2), bits(w[2], 26, 2));
        let reduction = match bits(w[0], 29, 2) {
            1 => 2,
            2 => 3,
            _ => 0,
        };
        let filter = if mag >= 2 || min >= 2 {
            0x55 | reduction << 7
        } else {
            (min & 1) << 4 | (mag & 1) << 2 | u32::from(mip == 2) | reduction << 7
        };
        let lod = |v: u32| v as f32 / 256.0;
        let (min_lod, max_lod) = if mip == 0 {
            (0.0, 0.0)
        } else {
            (lod(bits(w[1], 0, 12)), lod(bits(w[1], 12, 12)))
        };
        StaticSampler {
            filter,
            address: [0, 3, 6].map(|at| address(bits(w[0], at, 3))),
            mip_lod_bias: ((bits(w[2], 0, 14) << 18) as i32 >> 18) as f32 / 256.0,
            max_anisotropy: 1 << bits(w[0], 9, 3).min(4),
            comparison: bits(w[0], 12, 3) + 1,
            border: bits(w[3], 30, 2) % 3,
            min_lod,
            max_lod,
        }
    }

    pub fn from_rts0(part: &[u8]) -> Result<Vec<Self>, ShaderError> {
        let version = u32_at(part, 0)?;
        let stride = match version {
            1 | 2 => 52usize,
            3 => 56usize,
            _ => return Err(ShaderError::Malformed("unknown RTS0 version")),
        };
        let count = u32_at(part, 0x0c)? as usize;
        if count > MAX_RTS0_SAMPLERS {
            return Err(ShaderError::Malformed("implausible RTS0 static sampler count"));
        }
        if count == 0 {
            return Ok(Vec::new());
        }
        let offset = u32_at(part, 0x10)? as usize;
        let end = count
            .checked_mul(stride)
            .and_then(|bytes| offset.checked_add(bytes))
            .ok_or(ShaderError::Malformed("RTS0 static sampler table overflow"))?;
        if end > part.len() {
            return Err(ShaderError::Malformed("RTS0 static sampler table out of bounds"));
        }
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let at = offset + i * stride;
            out.push(StaticSampler {
                filter: u32_at(part, at)?,
                address: [
                    u32_at(part, at + 4)?,
                    u32_at(part, at + 8)?,
                    u32_at(part, at + 12)?,
                ],
                mip_lod_bias: f32::from_bits(u32_at(part, at + 16)?),
                max_anisotropy: u32_at(part, at + 20)?,
                comparison: u32_at(part, at + 24)?,
                border: u32_at(part, at + 28)?,
                min_lod: f32::from_bits(u32_at(part, at + 32)?),
                max_lod: f32::from_bits(u32_at(part, at + 36)?),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_compiled_samplers_decode() {
        let point = StaticSampler::from_words([0x3088, 0x0054_0100, 0x1400_0080, 0x8000_0000]);
        assert_eq!(
            point,
            StaticSampler {
                filter: 0,
                address: [1, 2, 3],
                mip_lod_bias: 0.5,
                max_anisotropy: 1,
                comparison: 4,
                border: 2,
                min_lod: 1.0,
                max_lod: 5.25,
            }
        );
        let aniso = StaticSampler::from_words([0x369E, 0x00FF_F000, 0x18F0_3E00, 0x8000_0000]);
        assert_eq!(
            (aniso.filter, aniso.address, aniso.max_anisotropy),
            (0x55, [4, 5, 3], 8)
        );
        assert_eq!((aniso.mip_lod_bias, aniso.max_lod), (-2.0, 4095.0 / 256.0));
        let compare = StaticSampler::from_words([0x3006, 0x00FF_F000, 0x1450_0000, 0x4000_0000]);
        assert_eq!(
            (compare.filter, compare.address, compare.border),
            (0x14, [4, 1, 1], 1)
        );
        let minimum =
            StaticSampler::from_words([0x2000_3003, 0x00FF_F000, 0x1850_0000, 0x8000_0000]);
        assert_eq!((minimum.filter, minimum.address[0]), (0x115, 5));
        let mag_mip = StaticSampler::from_words([0x3002, 0, 0x1810_0000, 0x8000_0000]);
        assert_eq!((mag_mip.filter, mag_mip.max_lod), (0x05, 0.0));
        let max_aniso =
            StaticSampler::from_words([0x4000_3800, 0x00FF_F000, 0x18F0_0F00, 0x8000_0000]);
        assert_eq!(
            (
                max_aniso.filter,
                max_aniso.max_anisotropy,
                max_aniso.mip_lod_bias
            ),
            (0x1D5, 16, 15.0)
        );
    }

    #[test]
    fn parses_rts0_static_samplers() {
        let mut rts = Vec::new();
        // Version 1.1 (2), 0 params, 2 static samplers at offset 24
        for w in [2u32, 0, 24, 2, 24, 0] {
            rts.extend_from_slice(&w.to_le_bytes());
        }
        // Sampler 0: linear clamp
        for w in [
            0x15u32,
            3,
            3,
            3,
            (-0.5f32).to_bits(),
            16,
            4,
            2,
            0.0f32.to_bits(),
            f32::MAX.to_bits(),
            0,
            0,
            0,
        ] {
            rts.extend_from_slice(&w.to_le_bytes());
        }
        // Sampler 1: point wrap
        for w in [
            0u32,
            1,
            1,
            1,
            0.25f32.to_bits(),
            1,
            1,
            0,
            1.0f32.to_bits(),
            8.0f32.to_bits(),
            1,
            0,
            0,
        ] {
            rts.extend_from_slice(&w.to_le_bytes());
        }
        let parsed = StaticSampler::from_rts0(&rts).unwrap();
        assert_eq!(
            parsed,
            [
                StaticSampler {
                    filter: 0x15,
                    address: [3, 3, 3],
                    mip_lod_bias: -0.5,
                    max_anisotropy: 16,
                    comparison: 4,
                    border: 2,
                    min_lod: 0.0,
                    max_lod: f32::MAX,
                },
                StaticSampler {
                    filter: 0,
                    address: [1, 1, 1],
                    mip_lod_bias: 0.25,
                    max_anisotropy: 1,
                    comparison: 1,
                    border: 0,
                    min_lod: 1.0,
                    max_lod: 8.0,
                },
            ]
        );

        // Version 1.2 (3) uses 56-byte entries.
        let mut rts12 = Vec::new();
        for w in [3u32, 0, 24, 1, 24, 0] {
            rts12.extend_from_slice(&w.to_le_bytes());
        }
        rts12.extend_from_slice(&rts[24..24 + 52]);
        rts12.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(StaticSampler::from_rts0(&rts12).unwrap(), [parsed[0]]);

        // Empty sampler table and error checks.
        let empty: Vec<u8> = [2u32, 0, 24, 0, 24, 0]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        assert!(StaticSampler::from_rts0(&empty).unwrap().is_empty());
        assert!(StaticSampler::from_rts0(&rts[..rts.len() - 4]).is_err());
        let mut bad_ver = rts.clone();
        bad_ver[0] = 9;
        assert!(StaticSampler::from_rts0(&bad_ver).is_err());
    }
}
