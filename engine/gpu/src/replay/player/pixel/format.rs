//! Texel coordinate mapping, format decoding, and command helpers for `--pixel`.

use super::super::{Barrier, Cmd, NONE};

/// The texel of a target of `size` that shows the point `(x, y)` of a picture
/// of `picture`, drawn through `view` (x, y, width, height): none if the
/// target has another shape than the picture.
pub(super) fn texel(
    (x, y): (u32, u32),
    picture: (u32, u32),
    view: [f32; 4],
    size: (u32, u32),
) -> Option<(u32, u32)> {
    let (w, h) = (picture.0 as f32, picture.1 as f32);
    if !same_shape((view[2], view[3]), (w, h)) || view[2] < 16.0 {
        return None;
    }
    let a = (view[0] + (x as f32 + 0.5) / w * view[2]).floor();
    let b = (view[1] + (y as f32 + 0.5) / h * view[3]).floor();
    (a >= 0.0 && b >= 0.0 && a < size.0 as f32 && b < size.1 as f32).then_some((a as u32, b as u32))
}

pub(super) fn same_shape(a: (f32, f32), b: (f32, f32)) -> bool {
    (0.97..1.03).contains(&(a.0 * b.1 / (a.1 * b.0).max(1e-6)))
}

/// A float from a small one's bits (5 bits of exponent, `mantissa` of
/// mantissa, no sign).
fn small(bits: u32, mantissa: u32) -> f32 {
    let (e, m) = (
        bits >> mantissa & 31,
        (bits & ((1 << mantissa) - 1)) as f32,
    );
    let unit = (1u32 << mantissa) as f32;
    match e {
        0 => m / unit * 2f32.powi(-14),
        31 if m == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        e => (1.0 + m / unit) * 2f32.powi(e as i32 - 15),
    }
}

/// Bytes of a texel of a format; 0 for formats not read.
pub(super) fn texel_bytes(format: i32) -> usize {
    match format {
        1..=4 => 16,
        5..=8 => 12,
        9..=22 => 8,
        23..=47 => 4,
        48..=59 => 2,
        60..=65 => 1,
        67 | 85..=93 => 4,
        _ => 0,
    }
}

/// A texel in words.
pub(super) fn texel_text(format: i32, b: &[u8]) -> String {
    let n = texel_bytes(format);
    if n == 0 || b.len() < n {
        return format!("{b:02x?}");
    }
    let word = |i: usize| u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
    let float = |i: usize| f32::from_bits(word(i));
    let short = |i: usize| u16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
    let half = |i: usize| {
        let v = small(u32::from(short(i) & 0x7FFF), 10);
        if short(i) >> 15 != 0 { -v } else { v }
    };
    let unorm16 = |i: usize| f32::from(short(i)) / 65535.0;
    let snorm16 = |i: usize| f32::from(short(i) as i16) / 32767.0;
    let unorm8 = |i: usize| f32::from(b[i]) / 255.0;
    let snorm8 = |i: usize| f32::from(b[i] as i8) / 127.0;
    let all: Vec<f32> = match format {
        1 | 2 => (0..4).map(float).collect(),
        5 | 6 => (0..3).map(float).collect(),
        9 | 10 => (0..4).map(half).collect(),
        11 => (0..4).map(unorm16).collect(),
        13 => (0..4).map(snorm16).collect(),
        15 | 16 => (0..2).map(float).collect(),
        23 | 24 => vec![
            (word(0) & 1023) as f32 / 1023.0,
            (word(0) >> 10 & 1023) as f32 / 1023.0,
            (word(0) >> 20 & 1023) as f32 / 1023.0,
            (word(0) >> 30) as f32 / 3.0,
        ],
        26 => vec![
            small(word(0) & 2047, 6),
            small(word(0) >> 11 & 2047, 6),
            small(word(0) >> 22, 5),
        ],
        27..=29 | 87 | 90 | 91 => (0..4).map(unorm8).collect(),
        31 => (0..4).map(snorm8).collect(),
        33 | 34 => (0..2).map(half).collect(),
        35 => (0..2).map(unorm16).collect(),
        37 => (0..2).map(snorm16).collect(),
        39..=41 => vec![float(0)],
        48 | 49 => (0..2).map(unorm8).collect(),
        51 => (0..2).map(snorm8).collect(),
        53 | 54 => vec![half(0)],
        55 | 56 => vec![unorm16(0)],
        58 => vec![snorm16(0)],
        60 | 61 => vec![unorm8(0)],
        63 => vec![snorm8(0)],
        _ => Vec::new(),
    };
    let all: Vec<String> = all.iter().map(|v| format!("{v:.5}")).collect();
    format!("{} {:02x?}", all.join(" "), &b[..n]).trim().to_owned()
}

/// One draw of an indirect call that has many.
pub(super) fn part_of(c: &Cmd, i: u32, stride: u32) -> Cmd {
    match c {
        Cmd::Indirect {
            signature,
            args,
            at,
            ..
        } => Cmd::Indirect {
            signature: *signature,
            most: 1,
            args: *args,
            at: at + u64::from(i) * u64::from(stride),
            count: NONE,
            count_at: 0,
        },
        c => c.clone(),
    }
}

pub(super) fn transition(res: u32, sub: u32, before: u32, after: u32) -> Barrier {
    Barrier::Transition {
        flags: 0,
        res,
        sub,
        before: before as i32,
        after: after as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_lie_in_viewports_of_the_pictures_shape() {
        let whole = |w: u32, h: u32| ([0.0, 0.0, w as f32, h as f32], (w, h));
        let picture = (3840, 2160);
        let (view, size) = whole(1920, 1080);
        assert_eq!(texel((3839, 2159), picture, view, size), Some((1919, 1079)));
        assert_eq!(texel((0, 0), picture, view, size), Some((0, 0)));
        let (view, size) = whole(2048, 2048);
        assert_eq!(texel((0, 0), picture, view, size), None);
        // A part of a larger target.
        let part = [0.0, 0.0, 1920.0, 1080.0];
        assert_eq!(
            texel((1920, 1080), picture, part, (3840, 2160)),
            Some((960, 540))
        );
        let beside = [1920.0, 0.0, 1920.0, 1080.0];
        assert_eq!(texel((1920, 1080), picture, beside, (1920, 1080)), None);
    }

    #[test]
    fn texels_are_read() {
        assert_eq!(
            texel_text(41, &1.5f32.to_le_bytes()),
            "1.50000 [00, 00, c0, 3f]"
        );
        assert_eq!(
            texel_text(28, &[255, 0, 0, 51]),
            "1.00000 0.00000 0.00000 0.20000 [ff, 00, 00, 33]"
        );
        assert!(
            texel_text(10, &[0, 0x3C, 0, 0xBC, 0, 0, 0, 0x38])
                .starts_with("1.00000 -1.00000 0.00000 0.50000 ")
        );
        let v = 0x3C0u32 | 0x380 << 11 | 0x200 << 22;
        assert!(texel_text(26, &v.to_le_bytes()).starts_with("1.00000 0.50000 2.00000 "));
        assert_eq!(texel_text(42, &[1, 2, 3, 4]), "[01, 02, 03, 04]");
        // Too few bytes, or a format that is not read.
        assert_eq!(texel_text(41, &[1]), "[01]");
        assert_eq!(texel_text(98, &[1, 2]), "[01, 02]");
    }

    #[test]
    fn an_indirect_draw_is_taken_apart() {
        let all = Cmd::Indirect {
            signature: 1,
            most: 8,
            args: 2,
            at: 100,
            count: 3,
            count_at: 4,
        };
        let one = Cmd::Indirect {
            signature: 1,
            most: 1,
            args: 2,
            at: 124,
            count: NONE,
            count_at: 0,
        };
        assert_eq!(part_of(&all, 2, 12), one);
    }
}
