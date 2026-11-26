use super::*;

#[test]
fn layouts_match_addrlib() {
    use Dimension::{Tex1d, Tex2d, Tex3d};
    #[rustfmt::skip]
    let cases = [
        // mode, dimension, element bytes, texels per element side,
        // [width, height, depth or array], mips,
        //     (size, first tail mip, hash)
        (Swizzle::SW_256B_S, Tex2d, 1, 1, [83, 31, 1], 6,
            (5120, 6, 0x435e_9cad_96ff_dd81)),
        (Swizzle::SW_256B_D, Tex2d, 16, 4, [109, 71, 1], 6,
            (13824, 6, 0xbc2b_b84f_c528_7cb3)),
        (Swizzle::SW_4KB_S, Tex2d, 8, 1, [250, 175, 1], 8,
            (495616, 4, 0x1e87_7240_4dcd_c56e)),
        (Swizzle::SW_4KB_S, Tex2d, 16, 1, [70, 70, 3], 4,
            (479232, 4, 0xf948_6d1d_511c_d75c)),
        (Swizzle::SW_4KB_D, Tex2d, 1, 1, [33, 17, 1], 6,
            (8192, 1, 0xaae1_0e3f_15a1_892e)),
        (Swizzle::SW_64KB_S, Tex2d, 2, 1, [40, 9, 1], 3,
            (65536, 0, 0xf7a3_7fe6_41c2_d565)),
        (Swizzle::SW_64KB_S, Tex2d, 4, 1, [100, 60, 6], 7,
            (786432, 1, 0xe6b7_50a0_b65e_09a3)),
        (Swizzle::SW_64KB_S, Tex2d, 8, 1, [65, 33, 1], 7,
            (131072, 1, 0xdf0a_7ddb_9b93_41ee)),
        (Swizzle::SW_64KB_D, Tex2d, 16, 4, [256, 256, 1], 9,
            (131072, 1, 0xe569_7630_03d8_3e28)),
        (Swizzle::SW_64KB_D, Tex2d, 4, 1, [100, 60, 1], 7,
            (131072, 1, 0xb1db_1641_adaf_77a6)),
        (Swizzle::SW_64KB_D, Tex2d, 1, 1, [255, 129, 2], 8,
            (262144, 1, 0xd706_42f5_9a01_5115)),
        (Swizzle::LINEAR, Tex2d, 4, 1, [30, 20, 1], 5,
            (10240, 5, 0x819a_9c12_81e6_40f2)),
        (Swizzle::LINEAR, Tex2d, 2, 1, [17, 9, 3], 5,
            (15360, 5, 0xbf1a_74f1_7803_df62)),
        (Swizzle::LINEAR, Tex1d, 4, 1, [1000, 1, 1], 10,
            (9216, 10, 0xd37e_f9ac_a5de_cab4)),
        (Swizzle::SW_4KB_S, Tex3d, 4, 1, [64, 32, 16], 7,
            (188416, 3, 0xf06b_d42a_7183_90e6)),
        (Swizzle::SW_4KB_S, Tex3d, 4, 1, [4, 4, 16], 5,
            (8192, 0, 0xbf87_001b_ad72_d375)),
        (Swizzle::SW_4KB_S, Tex3d, 1, 1, [1, 1, 17], 2,
            (8192, 0, 0xcdf4_4ee3_33e5_e03d)),
        (Swizzle::SW_64KB_S, Tex3d, 4, 1, [16, 16, 64], 4,
            (262144, 0, 0x5d1a_a7af_c371_e8e5)),
        (Swizzle::SW_64KB_S, Tex3d, 1, 1, [8, 8, 128], 4,
            (262144, 0, 0x8fec_f010_6542_6225)),
        (Swizzle::SW_64KB_S, Tex3d, 16, 1, [33, 17, 40], 6,
            (1966080, 3, 0xec9c_fd44_b89c_2fd5)),
        (Swizzle::LINEAR, Tex3d, 4, 1, [9, 7, 5], 3,
            (16640, 3, 0x6d5b_351d_4a3d_7b5a)),
    ];
    for (swizzle, dimension, element_bytes, block, extent, mips, expected) in cases {
        let [width, height, depth] = extent;
        let d = Desc {
            dimension,
            width,
            height,
            depth,
            mips,
            element_bytes,
            block,
            samples: 1,
            swizzle,
        };
        let l = Layout::new(&d).unwrap();
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for (mip, m) in (0..).zip(&l.mips) {
            let slices = if dimension == Tex3d { m.depth } else { depth };
            for z in 0..slices {
                for y in 0..m.height {
                    for x in 0..m.width {
                        for byte in l.offset(mip, x, y, z).unwrap().to_le_bytes() {
                            h = (h ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                        }
                    }
                }
            }
        }
        assert_eq!((l.size, l.first_tail_mip, h), expected, "{d:?}");
    }
}

/// The smaller mips lie first: they are read from the memory that holds
/// them alone, as from the whole texture's.
#[test]
fn smaller_mips_need_only_the_start_of_the_texture() {
    for swizzle in [Swizzle::SW_4KB_S, Swizzle(9)] {
        let l = Layout::new(&Desc { block: 4, ..desc(1024, 1024, 9, 8, swizzle) }).unwrap();
        assert_eq!(l.bytes_from(0), l.size);
        let needs: Vec<u64> = (0..9).map(|mip| l.bytes_from(mip)).collect();
        assert!(needs.windows(2).all(|w| w[0] >= w[1]), "{needs:?}");
        assert!(needs[3] * 4 < l.size, "{needs:?}");
        let tiled: Vec<u8> = (0..l.size).map(|i| (i * 7 >> 3) as u8).collect();
        for mip in 1..9 {
            let m = l.mips[mip as usize];
            let pitch = m.width as usize * 8;
            let read = |tiled: &[u8]| {
                let mut linear = vec![0; pitch * m.height as usize];
                l.detile(tiled, mip, 0, &mut linear, pitch, pitch * m.height as usize).map(|()| linear)
            };
            let len = l.bytes_from(mip) as usize;
            assert_eq!(read(&tiled[..len]), read(&tiled));
            assert_eq!(read(&tiled[..len - 1]), Err(Error::TooSmall));
        }
    }
    // Slices each hold their chain: the whole texture.
    let l = Layout::new(&Desc { depth: 2, block: 4, ..desc(1024, 1024, 9, 8, Swizzle::SW_4KB_S) }).unwrap();
    assert_eq!(l.bytes_from(4), l.size);
}

#[test]
fn depth_stencil_is_two_planes() {
    let t = Texture::new(&TextureDesc {
        dimension: Dimension::Tex2d,
        width: 1920,
        height: 1080,
        depth_or_array: 1,
        mips: 1,
        format: 20,
        samples: 1,
        swizzle: Swizzle::SW_64KB_Z_X,
    })
    .unwrap();
    assert_eq!(t.planes.len(), 2);
    // 4-byte depth in 128x128 blocks, 1-byte stencil in 256x256 blocks.
    assert_eq!(t.planes[0].layout.size, 1920 * 1152 * 4);
    assert_eq!(t.planes[1].offset, t.planes[0].layout.size);
    assert_eq!(t.planes[1].layout.size, 2048 * 1280);
    assert!(!t.planes[0].layout.addressable());
}

#[test]
fn texture_copies_reject_overflowing_pitches() {
    let layout = Layout::new(&desc(4, 2, 1, 4, Swizzle::LINEAR)).unwrap();
    let mut tiled = vec![0; layout.size as usize];
    let mut linear = [0; 32];
    assert_eq!(
        layout.detile(&tiled, 0, 0, &mut linear, usize::MAX, 0),
        Err(Error::TooSmall)
    );
    assert_eq!(
        layout.tile(&mut tiled, 0, 0, &linear, usize::MAX, 0),
        Err(Error::TooSmall)
    );
}

fn volume_pitch_layout(swizzle: Swizzle) -> Layout {
    Layout::new(&Desc {
        dimension: Dimension::Tex3d,
        depth: 2,
        ..desc(4, 2, 1, 4, swizzle)
    })
    .unwrap()
}

#[test]
fn depth_pitch_rejects_overlapping_detile() {
    for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
        let l = volume_pitch_layout(swizzle);
        let tiled = vec![0x42; l.size as usize];
        let mut linear = [0xcc; 72];

        for pitch in [0, 16, 35] {
            assert_eq!(
                l.detile(&tiled, 0, 0, &mut linear, 20, pitch),
                Err(Error::TooSmall),
                "{swizzle:?}, pitch {pitch}"
            );

            assert_eq!(linear, [0xcc; 72]);
        }
    }
}

#[test]
fn depth_pitch_rejects_overlapping_tile() {
    for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
        let l = volume_pitch_layout(swizzle);
        let mut tiled = vec![0xcc; l.size as usize];
        let linear = [0x42; 72];

        for pitch in [0, 16, 35] {
            assert_eq!(
                l.tile(&mut tiled, 0, 0, &linear, 20, pitch),
                Err(Error::TooSmall),
                "{swizzle:?}, pitch {pitch}"
            );

            assert!(tiled.iter().all(|&b| b == 0xcc));
        }
    }
}

#[test]
fn depth_pitch_accepts_exact_and_padded_slices() {
    for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
        let l = volume_pitch_layout(swizzle);

        for pitch in [36usize, 40, 64] {
            let len = pitch + 36;
            let mut tiled = vec![0; l.size as usize];

            let linear: Vec<u8> = (0..len).map(|i| i as u8).collect();

            let mut back = vec![0xcc; len];

            l.tile(&mut tiled, 0, 0, &linear, 20, pitch).unwrap();

            l.detile(&tiled, 0, 0, &mut back, 20, pitch).unwrap();

            for z in 0..2 {
                for y in 0..2 {
                    let start = z * pitch + y * 20;

                    assert_eq!(back[start..start + 16], linear[start..start + 16]);
                }
            }
        }
    }
}

#[test]
fn volume_copy_rejects_overflowing_depth_pitch() {
    let layout = Layout::new(&Desc {
        dimension: Dimension::Tex3d,
        depth: 3,
        ..desc(4, 2, 1, 4, Swizzle::LINEAR)
    })
    .unwrap();
    let tiled = vec![0; layout.size as usize];
    let mut linear = [0; 96];
    assert_eq!(
        layout.detile(&tiled, 0, 0, &mut linear, 16, usize::MAX),
        Err(Error::TooSmall)
    );
}

#[test]
fn bad_descriptions_and_buffers_are_refused() {
    assert_eq!(
        Layout::new(&desc(0, 4, 1, 4, Swizzle::SW_4KB_S)),
        Err(Error::Invalid)
    );
    assert_eq!(
        Layout::new(&desc(4, 4, 1, 12, Swizzle::SW_4KB_S)),
        Err(Error::Invalid)
    );
    assert_eq!(
        Layout::new(&desc(4, 4, 1, 4, Swizzle(3))),
        Err(Error::Invalid)
    );
    let l = Layout::new(&desc(64, 64, 1, 4, Swizzle::SW_4KB_S)).unwrap();
    let mut out = vec![0; 64 * 64 * 4];
    assert_eq!(
        l.detile(&[0; 100], 0, 0, &mut out, 256, 0),
        Err(Error::TooSmall)
    );
    assert_eq!(l.offset(0, 64, 0, 0), Err(Error::Invalid));
}
