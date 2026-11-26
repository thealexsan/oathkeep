mod layout;

use super::layout::MAX_EXTENT;
use super::swizzle::{pattern, spread};
use super::{Desc, Dimension, Error, Layout, Swizzle, Texture, TextureDesc, format};

use super::*;

pub(super) fn desc(w: u32, h: u32, mips: u32, bpe: u32, swizzle: Swizzle) -> Desc {
    Desc {
        dimension: Dimension::Tex2d,
        width: w,
        height: h,
        depth: 1,
        mips,
        element_bytes: bpe,
        block: 1,
        samples: 1,
        swizzle,
    }
}

#[test]
fn linear_layouts_require_one_sample() {
    let linear = desc(256, 256, 1, 4, Swizzle::LINEAR);
    let single = Layout::new(&linear).unwrap();
    assert!(single.addressable());
    assert_eq!(single.size, 256 * 256 * 4);

    for samples in [2, 4, 8] {
        let multisampled = Desc { samples, ..linear };
        assert_eq!(Layout::new(&multisampled), Err(Error::Invalid));

        let tiled = Layout::new(&Desc {
            swizzle: Swizzle::SW_64KB_R_X,
            ..multisampled
        })
        .unwrap();
        assert_eq!(tiled.size, single.size * u64::from(samples));
    }
}

/// Sizes past the hardware's 16384 texels per side are refused instead
/// of overflowing the size arithmetic (found by random-input testing).
#[test]
fn extents_past_the_hardware_limit_are_invalid() {
    for swizzle in [Swizzle::LINEAR, Swizzle::SW_64KB_D_X] {
        let largest = Layout::new(&desc(MAX_EXTENT, MAX_EXTENT, 15, 16, swizzle)).unwrap();
        assert!(largest.size >= u64::from(MAX_EXTENT) * u64::from(MAX_EXTENT) * 16);
        for (w, h) in [
            (MAX_EXTENT + 1, 1),
            (1, MAX_EXTENT + 1),
            (u32::MAX, u32::MAX),
        ] {
            let result = Layout::new(&desc(w, h, 1, 16, swizzle));
            assert_eq!(result, Err(Error::Invalid), "{w}x{h}");
        }
        let mut deep = desc(1, 1, 1, 16, swizzle);
        deep.depth = u32::MAX;
        assert_eq!(Layout::new(&deep), Err(Error::Invalid));
    }
}

#[test]
fn blocks_hold_their_size_in_elements() {
    for log2 in [8u32, 12, 16] {
        let swizzle = match log2 {
            8 => Swizzle::SW_256B_S,
            12 => Swizzle::SW_4KB_S,
            _ => Swizzle::SW_64KB_D,
        };
        for e in 0..5 {
            let l = Layout::new(&desc(1, 1, 1, 1 << e, swizzle)).unwrap();
            let [w, h, d] = l.block;
            assert_eq!(u64::from(w * h * d) << e, 1 << log2, "{log2} {e}");
            // The pattern covers exactly the block's coordinates.
            assert_eq!(l.pattern[0].len() as u32, w);
            assert_eq!(l.pattern[1].len() as u32, h);
        }
    }
    // 64 KiB, 16-byte elements: 64x64 (a 256x256 BC7 texture).
    let l = Layout::new(&desc(64, 64, 1, 16, Swizzle::SW_64KB_D)).unwrap();
    assert_eq!(l.block, [64, 64, 1]);
    assert_eq!((l.size, l.alignment), (65536, 65536));
}

/// Every element of every mip and slice lands on its own address, inside
/// the texture, and tiling then detiling gives the rows back.
#[test]
fn addresses_are_a_bijection_and_copies_round_trip() {
    let cases = [
        desc(64, 64, 7, 16, Swizzle::SW_64KB_D),
        desc(100, 60, 7, 4, Swizzle::SW_64KB_D),
        desc(250, 175, 8, 8, Swizzle::SW_4KB_S),
        desc(33, 17, 6, 1, Swizzle::SW_4KB_D),
        desc(40, 9, 3, 2, Swizzle::SW_64KB_S),
        desc(30, 20, 5, 4, Swizzle::LINEAR),
        desc(83, 31, 6, 1, Swizzle::SW_256B_S),
        Desc {
            block: 4,
            ..desc(109, 71, 6, 16, Swizzle::SW_256B_D)
        },
        Desc {
            depth: 3,
            ..desc(70, 70, 4, 16, Swizzle::SW_4KB_S)
        },
        Desc {
            dimension: Dimension::Tex3d,
            depth: 16,
            ..desc(64, 32, 7, 4, Swizzle::SW_4KB_S)
        },
    ];
    for d in cases {
        let l = Layout::new(&d).unwrap();
        let mut seen = vec![false; l.size as usize];
        let mut tiled = vec![0u8; l.size as usize];
        let n = d.element_bytes as usize;
        let slices = if d.dimension == Dimension::Tex3d { 1 } else { d.depth };
        for slice in 0..slices {
            for mip in 0..d.mips {
                let m = l.mips[mip as usize];
                let active = m.width as usize * n;
                let row = active + 13;
                let plane = row * m.height as usize + 11;
                let linear: Vec<u8> = (0..plane * m.depth as usize)
                    .map(|i| (i * 7 + mip as usize * 13 + slice as usize) as u8)
                    .collect();
                l.tile(&mut tiled, mip, slice, &linear, row, plane).unwrap();
                let mut back = vec![0xcc; linear.len()];
                l.detile(&tiled, mip, slice, &mut back, row, plane).unwrap();
                for z in 0..m.depth {
                    let base = z as usize * plane;
                    for y in 0..m.height {
                        let start = base + y as usize * row;
                        assert_eq!(back[start..start + active], linear[start..start + active]);
                        assert!(back[start + active..start + row].iter().all(|&b| b == 0xcc));
                        for x in 0..m.width {
                            let at = l.offset(mip, x, y, slice + z).unwrap() as usize;
                            let source = start + x as usize * n;
                            assert_eq!(tiled[at..at + n], linear[source..source + n]);
                            assert!(at + n <= seen.len(), "{d:?} mip {mip} outside");
                            assert!(at.is_multiple_of(n));
                            assert!(!seen[at], "{d:?} mip {mip} ({x},{y},{z}) twice");
                            seen[at] = true;
                        }
                    }
                    assert!(back[base + plane - 11..base + plane].iter().all(|&b| b == 0xcc));
                }
            }
        }
    }
}

#[test]
fn tiled_chunks_match_element_offsets_with_odd_row_pitches() {
    for swizzle in [
        Swizzle::SW_256B_S,
        Swizzle::SW_256B_D,
        Swizzle::SW_4KB_S,
        Swizzle::SW_4KB_D,
        Swizzle::SW_64KB_S,
        Swizzle::SW_64KB_D,
    ] {
        for n in [1, 2, 4, 8, 16] {
            let l = Layout::new(&Desc {
                depth: 2,
                ..desc(73, 39, 7, n, swizzle)
            })
            .unwrap();
            let n = n as usize;
            let tiled: Vec<_> = (0..l.size)
                .map(|i| (i.wrapping_mul(0x9e37_79b9) >> 24) as u8)
                .collect();
            let mut retiled = vec![0xbb; tiled.len()];
            let mut expected_tiled = retiled.clone();
            for slice in 0..l.slices {
                for (mip, m) in l.mips.iter().enumerate() {
                    let mip = mip as u32;
                    let bytes = m.width as usize * n;
                    let pitch = bytes + 3;
                    // Keep padding between rows, with exactly the active
                    // bytes in the last row.
                    let len = (m.height as usize - 1) * pitch + bytes;
                    let mut linear = vec![0xcc; len];
                    let mut expected = linear.clone();
                    for y in 0..m.height {
                        for x in 0..m.width {
                            let src = l.offset(mip, x, y, slice).unwrap() as usize;
                            let dst = y as usize * pitch + x as usize * n;
                            expected[dst..][..n].copy_from_slice(&tiled[src..][..n]);
                            expected_tiled[src..][..n].copy_from_slice(&tiled[src..][..n]);
                        }
                    }
                    l.detile(&tiled, mip, slice, &mut linear, pitch, len)
                        .unwrap();
                    assert_eq!(linear, expected, "{swizzle:?} {n} mip {mip} slice {slice}");
                    l.tile(&mut retiled, mip, slice, &expected, pitch, len)
                        .unwrap();
                }
            }
            assert_eq!(retiled, expected_tiled, "{swizzle:?} {n}");
        }
    }
}

/// Hand-checked: in a 64 KiB `D` block of 4-byte elements, x bits go to
/// address bits 2, 3, 6, 9, 11, 13, 15 and y bits to 4, 5, 7, 8, 10, 12,
/// 14; a 4 KiB `S` block of 16-byte elements is 16x16 with y first.
#[test]
fn hand_checked_addresses() {
    let l = Layout::new(&desc(128, 128, 1, 4, Swizzle::SW_64KB_D)).unwrap();
    assert_eq!(l.offset(0, 1, 0, 0), Ok(1 << 2));
    assert_eq!(l.offset(0, 4, 0, 0), Ok(1 << 6));
    assert_eq!(l.offset(0, 0, 4, 0), Ok(1 << 7));
    assert_eq!(l.offset(0, 0, 8, 0), Ok(1 << 8));
    assert_eq!(l.offset(0, 64, 64, 0), Ok((1 << 15) | (1 << 14)));
    let l = Layout::new(&desc(32, 32, 1, 16, Swizzle::SW_4KB_S)).unwrap();
    assert_eq!(l.block, [16, 16, 1]);
    assert_eq!(l.offset(0, 0, 1, 0), Ok(1 << 4));
    assert_eq!(l.offset(0, 1, 0, 0), Ok(1 << 6));
    // Blocks row by row: (16, 0) is the second block, (0, 16) the third.
    assert_eq!(l.offset(0, 16, 0, 0), Ok(4096));
    assert_eq!(l.offset(0, 0, 16, 0), Ok(8192));
}

#[test]
fn layouts_share_identical_swizzle_tables() {
    for (swizzle, kind, dimension, block_log2) in [
        (Swizzle::SW_256B_S, 0, Dimension::Tex2d, 8),
        (Swizzle::SW_256B_D, 1, Dimension::Tex2d, 8),
        (Swizzle::SW_4KB_S, 0, Dimension::Tex2d, 12),
        (Swizzle::SW_4KB_D, 1, Dimension::Tex2d, 12),
        (Swizzle::SW_64KB_S, 0, Dimension::Tex2d, 16),
        (Swizzle::SW_64KB_D, 1, Dimension::Tex2d, 16),
        (Swizzle::SW_4KB_S, 2, Dimension::Tex3d, 12),
        (Swizzle::SW_64KB_S, 2, Dimension::Tex3d, 16),
    ] {
        for element_log2 in 0..5 {
            let d = Desc {
                dimension,
                depth: 8,
                ..desc(128, 96, 7, 1 << element_log2, swizzle)
            };
            let first = Layout::new(&d).unwrap();
            let second = Layout::new(&Desc {
                width: 32,
                height: 24,
                mips: 3,
                depth: 1,
                ..d
            })
            .unwrap();
            let expected = pattern(kind, element_log2, block_log2).map(|bits| spread(&bits));
            assert_eq!(first.pattern, &expected);
            assert!(std::ptr::eq(first.pattern, second.pattern));
            assert!(std::ptr::eq(first.pattern, first.clone().pattern));
        }
    }
}

/// A 256x256 BC7 chain in 64 KiB `D`: one tail block holding mips 1-8,
/// then mip 0; mip 1 sits at x = 32 elements, 32 KiB into the tail.
#[test]
fn mip_tail_comes_first() {
    let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_64KB_D)).unwrap();
    assert_eq!(l.first_tail_mip, 1);
    assert_eq!(l.size, 2 * 65536);
    assert_eq!(l.mips[0].offset, 65536);
    assert_eq!(
        l.mips[1].tail,
        Some(Tail {
            offset: 32768,
            x: 32,
            y: 0
        })
    );
    assert_eq!(l.offset(1, 0, 0, 0), Ok(32768));
    // 4 KiB blocks hold fewer tail mips: 16x16 BC7 mips stay outside.
    let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_4KB_S)).unwrap();
    assert_eq!(l.first_tail_mip, 3);
    assert_eq!(l.mips[2].offset, 4096);
    assert_eq!(l.mips[1].offset, 8192);
    assert_eq!(l.mips[0].offset, 8192 + 16384);
}

/// A volume deeper than its block keeps its whole chain in the tail when
/// every mip's width and height fit it (addrlib's `mipChainInTail`): each
/// depth slab has its own tail block, holding that slab's slices of
/// every mip. 16x16x64 in 64 KiB `S` blocks of 32x32x16 is four slabs
/// of one block; mip 1's 32 slices span the first two. Addresses from
/// addrlib, as in [`layouts_match_addrlib`].
#[test]
fn deep_volumes_keep_their_chain_in_each_slabs_tail() {
    let l = Layout::new(&Desc {
        dimension: Dimension::Tex3d,
        depth: 64,
        ..desc(16, 16, 4, 4, Swizzle::SW_64KB_S)
    })
    .unwrap();
    assert_eq!(l.block, [32, 32, 16]);
    assert_eq!((l.first_tail_mip, l.size), (0, 4 * 65536));
    for (mip, x, y, z, address) in [
        (0, 0, 0, 0, 32768),
        (0, 0, 0, 15, 42128),
        (0, 0, 0, 16, 65536 + 32768),
        (0, 15, 15, 63, 3 * 65536 + 49148),
        (1, 0, 0, 0, 16384),
        (1, 0, 0, 16, 65536 + 16384),
        (1, 0, 0, 31, 65536 + 25744),
        (2, 0, 0, 0, 4096),
        (3, 1, 1, 7, 3740),
    ] {
        let at = l.offset(mip, x, y, z);
        assert_eq!(at, Ok(address), "mip {mip} ({x},{y},{z})");
    }
}
