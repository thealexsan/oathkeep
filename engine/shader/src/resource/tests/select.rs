use crate::native::{HwStage, RootType};
use crate::ShaderError;

use super::super::*;
use RootType::*;
use super::S_ENDPGM;


use super::{at, lanes, param, track_words};

#[test]
fn a_descriptor_kept_per_lane_is_its_load_and_needs_no_waterfall() {
    // The console compiler's way with a descriptor it joins per lane:
    // the dwords go to VGPRs and come back from the first lane. Every
    // lane holds the one load's dwords, so the descriptor is that load
    // (`lane_copied`), and no waterfall is asked for.
    let mut words = vec![
        0xBE82_0309, // s_mov_b32 s2, s9
        0xBE83_0380, // s_mov_b32 s3, 0
        0xF40C_0001,
        0xFA00_0000, // s_load_dwordx8 s[0:7], s[2:3], null
    ];
    // v_mov_b32 v10.., s0..; v_readfirstlane_b32 s16.., v10..
    words.extend((0..8).map(|k| 0x7E00_0200 | (10 + k) << 17 | k));
    words.extend((0..8).map(|k| 0x7E00_0400 | (16 + k) << 17 | (0x100 + 10 + k)));
    words.extend([
        0xF000_1700,
        0x0004_0D06, // image_load v[13:15], v6, s[16:23]
        S_ENDPGM,
    ]);
    let r = track_words(&words, &[param(Table, 9, 1)], HwStage::Ps);
    let load = (0, 2);
    assert_eq!(r.scalar, [(load, at(0, 0))]);
    let sources: Vec<_> = r.descriptors.iter().map(|d| &d.source).collect();
    assert_eq!(sources, [&Source::Loaded { load, first: 0 }]);
    assert_eq!(r.lane_copied, [load]);
    assert_eq!(r.waterfall, Waterfall::default());
}

#[test]
fn loads_at_one_first_lane_offset_have_one_address() {
    // Two loads of the table entry at the offset v4 holds, each with
    // its own first-lane read, as the console compiler loads a
    // descriptor again wherever it needs it: one address, by which
    // lowering knows them for the same descriptor.
    let r = track_words(
        &[
            0xBE82_0309, // s_mov_b32 s2, s9
            0xBE83_0380, // s_mov_b32 s3, 0
            0x7E08_0504, // v_readfirstlane_b32 s4, v4
            0xF40C_0201,
            0x0800_0000, // s_load_dwordx8 s[8:15], s[2:3], s4
            0x7E0A_0504, // v_readfirstlane_b32 s5, v4
            0xF40C_0401,
            0x0A00_0000, // s_load_dwordx8 s[16:23], s[2:3], s5
            0xF000_1700,
            0x0004_0D06, // image_load v[13:15], v6, s[16:23]
            S_ENDPGM,
        ],
        &[param(Table, 9, 1)],
        HwStage::Ps,
    );
    let [(_, a), (_, b)] = r.scalar[..] else {
        panic!("{:?}", r.scalar)
    };
    assert_eq!(a, b);
    assert_eq!((a.param, a.offset), (0, 0));
    assert!(
        a.dynamic.is_some(),
        "the lane's offset is the computed term"
    );
}

#[test]
fn descriptors_selected_by_scc_are_traced_per_outcome() {
    // Two buffer descriptors loaded from the table at s9 and one
    // picked with s_cselect_b64 per half, as the compiler makes a
    // conditional choice of resources without a branch.
    let r = track_words(
        &[
            0xBE8C_0309, // s_mov_b32 s12, s9
            0xBE8D_0380, // s_mov_b32 s13, 0
            0xF408_0006,
            0xFA00_0000, // s_load_dwordx4 s[0:3], s[12:13], null
            0xF408_0106,
            0xFA00_0010, // s_load_dwordx4 s[4:7], s[12:13], 0x10
            0xBF06_800E, // s_cmp_eq_u32 s14, 0
            0x8588_0400, // s_cselect_b64 s[8:9], s[0:1], s[4:5]
            0x858A_0602, // s_cselect_b64 s[10:11], s[2:3], s[6:7]
            0xF420_0404,
            0xFA00_0000, // s_buffer_load_dword s16, s[8:11], null
            S_ENDPGM,
        ],
        &[param(Table, 9, 1)],
        HwStage::Ps,
    );
    let Source::Select { args, .. } = &r.descriptors[0].source else {
        panic!("{:?}", r.descriptors)
    };
    // SCC set picks the first operand.
    assert_eq!(
        args[..],
        [
            Source::Loaded {
                load: (0, 2),
                first: 0
            },
            Source::Loaded {
                load: (0, 3),
                first: 0
            },
        ]
    );
    assert_eq!(r.scalar, [((0, 2), at(0, 0)), ((0, 3), at(0, 0x10))]);
}

/// The FidelityFX upscaler's byte accesses make a loaded buffer
/// descriptor raw first (stride cleared, records times stride, bounds
/// in bytes): the access reads the loaded descriptor. Any other change
/// still fails.
#[test]
fn a_descriptor_made_raw_is_the_loaded_one() {
    let words = |mask: u32| -> Vec<u8> {
        [
            0xBE88_0304, // s_mov_b32 s8, s4
            0xBE89_0380, // s_mov_b32 s9, 0
            0xF408_0204,
            0xFA00_0160, // s_load_dwordx4 s[8:11], s[8:9], 0x160
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0x938C_FF09,
            0x000E_0010, // s_bfe_u32 s12, s9, 0x000e0010
            0x848C_810C, // s_max_u32 s12, s12, 1
            0x8709_FF09,
            mask, // s_and_b32 s9, s9, mask
            0x870B_FF0B,
            0xCFFF_FFFF, // s_and_b32 s11, s11, 0xcfffffff
            0x930A_0C0A, // s_mul_i32 s10, s10, s12
            0x880B_FF0B,
            0x3000_0000, // s_or_b32 s11, s11, 0x30000000
            0xE078_1000,
            0x8002_0007, // buffer_store_dwordx4 v[0:3], v7, s[8:11], 0 offen
            S_ENDPGM,
        ]
        .iter()
        .flat_map(|w: &u32| w.to_le_bytes())
        .collect()
    };
    let run = |mask| {
        let p = crate::ir::build(&words(mask), false, 256).unwrap();
        track(&p, &[param(Table, 4, 1)], HwStage::Cs, lanes(&p).as_ref())
    };
    assert!(run(0xC000_FFFF).is_ok());
    assert_eq!(
        run(0xC000_FFFE).unwrap_err(),
        ShaderError::Unsupported {
            offset: 0x3c,
            what: "a descriptor changed after loading"
        }
    );
}

/// Immediate sampler words the upscaler's compiler makes with a bit
/// mask and a 64-bit move of a literal.
#[test]
fn sampler_words_from_bit_masks_and_wide_literals_are_constants() {
    let code: Vec<u8> = [
        0xB010_3092, // s_movk_i32 s16, 0x3092
        0x9211_8C8C, // s_bfm_b32 s17, 12, 12
        0xBE92_04FF,
        0x1850_0000, // s_mov_b64 s[18:19], 0x18500000
        S_ENDPGM,
    ]
    .iter()
    .flat_map(|w: &u32| w.to_le_bytes())
    .collect();
    let p = crate::ir::build(&code, false, 256).unwrap();
    let layout = [param(Table, 4, 1)];
    let t = Tracer::new(&p, &layout, HwStage::Cs, None).unwrap();
    let block = &p.blocks[0];
    let value = |n: usize, k: usize| t.origin(block.insts[n].writes[k].1);
    assert_eq!(value(1, 0), Term::Const(0x00FF_F000));
    assert_eq!(value(2, 0), Term::Const(0x1850_0000));
    assert_eq!(value(2, 1), Term::Const(0));
}


#[test]
fn a_selection_that_mixes_two_descriptors_fails() {
    // The high half picked from the first load whatever SCC is: when
    // clear, the descriptor is half of one load and half of the other.
    let code: Vec<u8> = [
        0xBE8C_0309, // s_mov_b32 s12, s9
        0xBE8D_0380, // s_mov_b32 s13, 0
        0xF408_0006,
        0xFA00_0000, // s_load_dwordx4 s[0:3], s[12:13], null
        0xF408_0106,
        0xFA00_0010, // s_load_dwordx4 s[4:7], s[12:13], 0x10
        0xBF06_800E, // s_cmp_eq_u32 s14, 0
        0x8588_0400, // s_cselect_b64 s[8:9], s[0:1], s[4:5]
        0x858A_0202, // s_cselect_b64 s[10:11], s[2:3], s[2:3]
        0xF420_0404,
        0xFA00_0000, // s_buffer_load_dword s16, s[8:11], null
        S_ENDPGM,
    ]
    .iter()
    .flat_map(|w: &u32| w.to_le_bytes())
    .collect();
    let p = crate::ir::build(&code, false, 256).unwrap();
    let err = track(&p, &[param(Table, 9, 1)], HwStage::Ps, lanes(&p).as_ref()).unwrap_err();
    assert_eq!(
        err,
        ShaderError::Unsupported {
            offset: 36,
            what: "a descriptor changed after loading"
        }
    );
}

