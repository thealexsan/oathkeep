pub(crate) fn expand_rects(src: &[u8], dst: &mut [u8], stride: usize, elements: &[(u32, u32)]) {
    for (rect, out) in src
        .chunks_exact(3 * stride)
        .zip(dst.chunks_exact_mut(6 * stride))
    {
        let v = |i: usize| &rect[i * stride..(i + 1) * stride];
        for (slot, i) in [0, 1, 2, 0, 2].into_iter().enumerate() {
            out[slot * stride..(slot + 1) * stride].copy_from_slice(v(i));
        }
        let fourth = &mut out[5 * stride..];
        fourth.copy_from_slice(v(0));
        for &(offset, format) in elements {
            let at = offset as usize;
            let Some((n, kind)) = components(format) else {
                continue;
            };
            let inside = stride.saturating_sub(at) / kind.bytes();
            for c in 0..n.min(inside) {
                let get = |i: usize| kind.read(&v(i)[at..], c);
                let value = get(0) + get(2) - get(1);
                kind.write(&mut fourth[at..], c, value);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    F32,
    F16,
    Unorm8,
    Unorm16,
}

fn components(format: u32) -> Option<(usize, Kind)> {
    Some(match format {
        2 => (4, Kind::F32),
        6 => (3, Kind::F32),
        16 => (2, Kind::F32),
        41 => (1, Kind::F32),
        10 => (4, Kind::F16),
        34 => (2, Kind::F16),
        54 => (1, Kind::F16),
        11 => (4, Kind::Unorm16),
        35 => (2, Kind::Unorm16),
        28 | 29 | 87 | 91 => (4, Kind::Unorm8),
        _ => return None,
    })
}

impl Kind {
    fn bytes(self) -> usize {
        match self {
            Kind::F32 => 4,
            Kind::F16 | Kind::Unorm16 => 2,
            Kind::Unorm8 => 1,
        }
    }

    fn read(self, at: &[u8], c: usize) -> f32 {
        let b = &at[c * self.bytes()..];
        match self {
            Kind::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Kind::F16 => half_to_f32(u16::from_le_bytes([b[0], b[1]])),
            Kind::Unorm16 => f32::from(u16::from_le_bytes([b[0], b[1]])) / 65535.0,
            Kind::Unorm8 => f32::from(b[0]) / 255.0,
        }
    }

    fn write(self, at: &mut [u8], c: usize, v: f32) {
        let n = self.bytes();
        let b = &mut at[c * n..(c + 1) * n];
        match self {
            Kind::F32 => b.copy_from_slice(&v.to_le_bytes()),
            Kind::F16 => b.copy_from_slice(&f32_to_half(v).to_le_bytes()),
            Kind::Unorm16 => {
                b.copy_from_slice(&((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_le_bytes())
            }
            Kind::Unorm8 => b[0] = (v.clamp(0.0, 1.0) * 255.0).round() as u8,
        }
    }
}

fn half_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let man = u32::from(h & 0x3ff);
    let bits = match (exp, man) {
        (0, 0) => sign,
        (0, m) => {
            let v = m as f32 * 2f32.powi(-24);
            return if sign != 0 { -v } else { v };
        }
        (31, m) => sign | 0x7f80_0000 | m << 13,
        (e, m) => sign | (e + 112) << 23 | m << 13,
    };
    f32::from_bits(bits)
}

fn f32_to_half(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let man = bits & 0x7f_ffff;
    if exp >= 31 {
        return sign | 0x7c00;
    }
    if exp <= 0 {
        if exp < -10 {
            return sign;
        }
        let sig = (1u32 << 23) | man;
        let shift = (14 - exp) as u32;
        let halfway = 1u32 << (shift - 1);
        let q = sig >> shift;
        let r = sig & ((1u32 << shift) - 1);
        let m = q + u32::from(r > halfway || (r == halfway && (q & 1) != 0));
        return sign | m as u16;
    }
    let half = (exp as u32) << 10;
    sign | (half + ((man + 0x1000) >> 13)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_get_their_fourth_corner() {
        let vertex = |x: f32, y: f32, r: u8| {
            let mut v = Vec::new();
            v.extend(x.to_le_bytes());
            v.extend(y.to_le_bytes());
            v.extend([r, 0, 0, 255]);
            v
        };
        let src: Vec<u8> = [
            vertex(-1.0, 1.0, 0),
            vertex(-1.0, -1.0, 100),
            vertex(1.0, -1.0, 150),
        ]
        .concat();
        let mut dst = vec![0u8; 6 * 12];
        expand_rects(&src, &mut dst, 12, &[(0, 16), (8, 28)]);
        assert_eq!(dst[..36], src[..]);
        assert_eq!(dst[36..48], src[..12]);
        assert_eq!(dst[48..60], src[24..36]);
        assert_eq!(dst[60..], vertex(1.0, 1.0, 50)[..]);
        assert_eq!(f32_to_half(1.5), 0x3e00);
        assert_eq!(half_to_f32(0x3e00), 1.5);
        assert_eq!(half_to_f32(0xc000), -2.0);
    }

    #[test]
    fn halves_round_up_into_the_next_power_of_two() {
        assert_eq!(f32_to_half(2.0 - 2f32.powi(-12)), 0x4000);
        assert_eq!(f32_to_half(0.5 - 2f32.powi(-13)), 0x3800);
        assert_eq!(f32_to_half(65520.0), 0x7c00, "past the largest half");
        let src: Vec<u8> = [0x3c00_u16, 0x0c00, 0x3c00]
            .iter()
            .flat_map(|h| h.to_le_bytes())
            .collect();
        let mut dst = vec![0u8; 6 * 2];
        expand_rects(&src, &mut dst, 2, &[(0, 54)]);
        assert_eq!(dst[10..], 0x4000_u16.to_le_bytes());
    }

    #[test]
    fn elements_past_the_stride_are_left_alone() {
        let vertex = |x: f32, y: f32| [x.to_le_bytes(), y.to_le_bytes()].concat();
        let src = [vertex(0.0, 2.0), vertex(1.0, 3.0), vertex(5.0, 7.0)].concat();
        let mut dst = vec![0u8; 6 * 8];
        expand_rects(&src, &mut dst, 8, &[(0, 16), (4, 2), (8, 28)]);
        assert_eq!(dst[..40], [&src[..], &src[..8], &src[16..]].concat()[..]);
        assert_eq!(dst[40..], vertex(4.0, 6.0)[..]);
    }

    #[test]
    fn half_subnormals_convert_in_both_directions() {
        assert_eq!(f32_to_half(2f32.powi(-24)), 0x0001);
        assert_eq!(f32_to_half(-2f32.powi(-24)), 0x8001);
        assert_eq!(f32_to_half(2f32.powi(-23)), 0x0002);
        assert_eq!(f32_to_half(1023.0 * 2f32.powi(-24)), 0x03ff);
        assert_eq!(f32_to_half(2f32.powi(-25)), 0x0000);
        assert_eq!(f32_to_half(3.0 * 2f32.powi(-25)), 0x0002);
        assert_eq!(f32_to_half(2f32.powi(-26)), 0x0000);
        assert_eq!(f32_to_half(1.5), 0x3e00);
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
    }
}
