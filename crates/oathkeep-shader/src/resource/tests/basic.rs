use crate::native::{HwStage, RootParam, RootType};

use super::super::*;
use RootType::*;
use super::S_ENDPGM;


use super::{at, param, track_words};

#[test]
fn table_descriptors_are_loaded_from_their_range() {
    let r = track_words(
        &[
            0xBE82_0309, // s_mov_b32 s2, s9
            0xBE89_0380, // s_mov_b32 s9, 0
            0xBE83_0380, // s_mov_b32 s3, 0
            0xF408_0204,
            0xFA00_0000, // s_load_dwordx4 s[8:11], s[8:9], null
            0xF40C_0001,
            0xFA00_0000, // s_load_dwordx8 s[0:7], s[2:3], null
            0xF080_0F08,
            0x0040_0002, // image_sample v[0:3], v[2:3], s[0:7], s[8:11]
            S_ENDPGM,
        ],
        &[param(Table, 8, 1), param(Table, 9, 1)],
        HwStage::Ps,
    );
    assert_eq!(r.scalar, [((0, 3), at(0, 0)), ((0, 4), at(1, 0))]);
    let roles: Vec<_> = r.descriptors.iter().map(|d| (d.role, &d.source)).collect();
    assert_eq!(
        roles,
        [
            (
                Role::Resource,
                &Source::Loaded {
                    load: (0, 4),
                    first: 0
                }
            ),
            (
                Role::Sampler,
                &Source::Loaded {
                    load: (0, 3),
                    first: 0
                }
            ),
        ]
    );
}

#[test]
fn root_descriptors_are_built_in_code() {
    let r = track_words(
        &[
            0x8801_FF0B,
            0x0010_0000, // s_or_b32 s1, s11, 0x100000
            0xBE80_030A, // s_mov_b32 s0, s10
            0xBE82_03C1, // s_mov_b32 s2, -1
            0xBE83_03FF,
            0x1101_4FAC, // s_mov_b32 s3, 0x11014fac
            0xF430_0000,
            0xFA00_0000, // s_buffer_load_dwordx16 s[0:15], s[0:3], null
            S_ENDPGM,
        ],
        &[param(Descriptor, 10, 2)],
        HwStage::Vs,
    );
    assert_eq!(
        r.descriptors[0].source,
        Source::Built {
            address: at(0, 0),
            bits: 0x10_0000,
            words: [u32::MAX, 0x1101_4FAC]
        }
    );
    assert_eq!(
        r.params.iter().map(|p| (p.1, p.2)).collect::<Vec<_>>(),
        [(0, 0), (0, 1)]
    );
}

#[test]
fn heap_indices_from_root_constants_in_a_merged_stage() {
    // User data starts at s8: the constant (index 6) is s14, the heap
    // (index 10) s18.
    let code = [
        0x8F08_850E, // s_lshl_b32 s8, s14, 5
        0xBE93_0380, // s_mov_b32 s19, 0
        0xF408_0209,
        0x1000_0000, // s_load_dwordx4 s[8:11], s[18:19], s8
        0xE000_2000,
        0x8002_0400, // buffer_load_format_x v4, v0, s[8:11], 0 idxen
        S_ENDPGM,
    ];
    let r = track_words(
        &code,
        &[param(Constants, 6, 1), param(ResourceHeap, 10, 1)],
        HwStage::Ms,
    );
    let index = r.params.iter().find(|p| p.1 == 0).unwrap().0;
    let shift = r.scalar[0].1.dynamic.unwrap();
    assert_eq!(r.scalar[0].1.param, 1);
    // The computed term is the shifted constant, not the constant itself.
    assert_ne!(shift, index);
    assert_eq!(
        r.descriptors[0].source,
        Source::Loaded {
            load: (0, 2),
            first: 0
        }
    );
    assert!(r.root_indexed_views.is_empty());
    let table = RootParam {
        slot: 7,
        ..param(Table, 10, 1)
    };
    let indexed = track_words(&code, &[param(Constants, 6, 1), table], HwStage::Ms);
    assert_eq!(
        indexed.root_indexed_views,
        [RootIndexedView {
            table_slot: 7,
            constant_slot: 0,
            constant_dword: 0,
            byte_offset: 0,
            shift: 5,
        }]
    );
    let nonconstant = track_words(&code, &[param(Table, 6, 1), table], HwStage::Ms);
    assert!(nonconstant.root_indexed_views.is_empty());
}

#[test]
fn spilled_sgprs_are_followed_through_vgpr_lanes() {
    let r = track_words(
        &[
            0xD761_0000,
            0x0001_0609, // v_writelane_b32 v0, s9, 3
            0xD760_0002,
            0x0001_0700, // v_readlane_b32 s2, v0, 3
            0xBE83_0380, // s_mov_b32 s3, 0
            0xF40C_0001,
            0xFA00_0020, // s_load_dwordx8 s[0:7], s[2:3], 0x20
            S_ENDPGM,
        ],
        &[param(Table, 9, 1)],
        HwStage::Ps,
    );
    assert_eq!(r.scalar, [((0, 3), at(0, 0x20))]);
}

/// A VGPR a wave reduction used is free again after it: from the first
/// save on it holds parked scalars alone (an upscaler program parks a
/// constant buffer's descriptor that way).
#[test]
fn a_vgpr_used_before_holds_spilled_sgprs_from_its_first_save() {
    let before = [
        0x7E00_0281, // v_mov_b32 v0, 1
        0x7E02_0300, // v_mov_b32 v1, v0
    ];
    let park = [
        0xD761_0000,
        0x0001_0609, // v_writelane_b32 v0, s9, 3
        0xD760_0002,
        0x0001_0700, // v_readlane_b32 s2, v0, 3
        0xBE83_0380, // s_mov_b32 s3, 0
    ];
    let load = [
        0xF40C_0001,
        0xFA00_0020, // s_load_dwordx8 s[0:7], s[2:3], 0x20
        S_ENDPGM,
    ];
    let words = [&before[..], &park, &load].concat();
    let r = track_words(&words, &[param(Table, 9, 1)], HwStage::Ps);
    assert_eq!(r.scalar, [((0, 5), at(0, 0x20))]);
    // Still lane reads: a lane restored before it is saved, and a
    // register with an ordinary use after the first save.
    let restored_first = [&before[..], &park[2..4], &park[..2], &load].concat();
    let used_after = [&before[..], &park, &before[1..], &load].concat();
    for words in [restored_first, used_after] {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, true, 8).unwrap();
        let insts = p.blocks.iter().flat_map(|b| &b.insts);
        assert_eq!(insts.filter(|i| i.name() == "v_readlane_b32").count(), 1);
    }
}

/// Two lanes hold a pair (a lane mask, say) and later one of them a
/// scalar, as in a water volume program that parks more than the lanes
/// of one VGPR hold: the pair still moves as one.
#[test]
fn a_lane_spilled_to_again_later_does_not_split_an_earlier_pair() {
    let pair = [
        0xD761_0000u32,
        0x0001_0004, // v_writelane_b32 v0, s4, 0
        0xD761_0000,
        0x0001_0205, // v_writelane_b32 v0, s5, 1
        0xBE84_0380, // s_mov_b32 s4, 0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xD760_0004,
        0x0001_0100, // v_readlane_b32 s4, v0, 0
        0xD760_0005,
        0x0001_0300, // v_readlane_b32 s5, v0, 1
    ];
    let scalar = [
        0xD761_0000u32,
        0x0001_0006, // v_writelane_b32 v0, s6, 0
        0xBE86_0380, // s_mov_b32 s6, 0
        0xD760_0006,
        0x0001_0100, // v_readlane_b32 s6, v0, 0
        S_ENDPGM,
    ];
    let moves = |words: &[u32]| {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, true, 8).unwrap();
        let names: Vec<_> = p.blocks[0].insts.iter().map(|i| i.name()).collect();
        assert!(!names.iter().any(|n| n.contains("lane")));
        names.iter().filter(|n| **n == "s_mov_b64").count()
    };
    assert_eq!(moves(&[&pair[..], &scalar].concat()), 2);
    // A restore both saves reach keeps the lane one scalar.
    let joined = [
        &pair[..4],
        &[0xBF84_0002], // s_cbranch_scc0 past the second save
        &scalar[..2],
        &pair[4..],
        &scalar[2..],
    ]
    .concat();
    assert_eq!(moves_anywhere(&joined), 0);
}

fn moves_anywhere(words: &[u32]) -> usize {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let p = crate::ir::build(&code, true, 8).unwrap();
    let insts = p.blocks.iter().flat_map(|b| &b.insts);
    insts.filter(|i| i.name() == "s_mov_b64").count()
}

#[test]
fn descriptors_joined_by_control_flow_are_traced_per_edge() {
    let r = track_words(
        &[
            0xBE82_0309, // s_mov_b32 s2, s9
            0xBE83_0380, // s_mov_b32 s3, 0
            0xBF06_8001, // s_cmp_eq_u32 s1, 0
            0xBF85_0003, // s_cbranch_scc1 -> 0x1c
            0xF408_0001,
            0xFA00_0000, // s_load_dwordx4 s[0:3], s[2:3], null
            0xBF82_0002, // s_branch -> 0x24
            0xF408_0001,
            0xFA00_0010, // 0x1c: s_load_dwordx4 s[0:3], s[2:3], 0x10
            0xF420_0100,
            0xFA00_0000, // 0x24: s_buffer_load_dword s4, s[0:3], null
            S_ENDPGM,
        ],
        &[param(Table, 9, 1)],
        HwStage::Ps,
    );
    let Source::Phi { args, .. } = &r.descriptors[0].source else {
        panic!("{:?}", r.descriptors)
    };
    let offsets: Vec<_> = args
        .iter()
        .map(|a| match a {
            Source::Loaded { load, .. } => {
                r.scalar.iter().find(|s| s.0 == *load).unwrap().1.offset
            }
            a => panic!("{a:?}"),
        })
        .collect();
    assert_eq!(offsets.len(), 2);
    assert!(
        offsets.contains(&0) && offsets.contains(&0x10),
        "{offsets:?}"
    );
}

/// Two paths add an index to one table in the code and meet before one load
/// (hair `CS_InterpolateStrands`): the join is the computed term, counted
/// from the `s_getpc_b64`.
#[test]
fn two_paths_into_one_code_table_load() {
    let r = track_words(
        &[
            0xBE82_1F00, // 0x00: s_getpc_b64 s[2:3]
            0x8002_FF02,
            0x0000_0040, // s_add_u32 s2, s2, 0x40
            0x8203_8003, // s_addc_u32 s3, s3, 0
            0xBF06_8001, // 0x10: s_cmp_eq_u32 s1, 0
            0xBF85_0005, // 0x14: s_cbranch_scc1 -> 0x2c
            0xD70F_0457,
            0x0000_054A, // 0x18: v_add_co_u32 v87, s4, v74, s2
            0xD528_0458,
            0x0010_0680, // v_add_co_ci_u32 v88, s4, 0, s3, s4
            0xBF82_0004, // 0x28: s_branch -> 0x3c
            0xD70F_0457,
            0x0000_054B, // 0x2c: v_add_co_u32 v87, s4, v75, s2
            0xD528_0458,
            0x0010_0680, // v_add_co_ci_u32 v88, s4, 0, s3, s4
            0xDC34_8000,
            0x577D_0057, // 0x3c: global_load_dwordx2 v[87:88], v[87:88], null
            S_ENDPGM,
        ],
        &[],
        HwStage::Cs,
    );
    assert_eq!(r.scalar.len(), 1);
    let a = r.scalar[0].1;
    assert_eq!((a.param, a.offset), (CODE, 0));
    assert!(a.dynamic.is_some(), "the join is the computed term, the byte in the code");
}

/// An address into a table in the code computed per lane and taken from
/// the first lane (`avgLuminanceCompute` `ColorDownsample`): the per-lane
/// sum carries the `s_getpc_b64` base, so the first-lane read is looked
/// through and the lane's index is the computed term. (A first-lane read
/// of an index alone stays the computed term itself; see
/// `the_console_waterfall_over_a_first_lane_offset_is_one`.)
#[test]
fn a_code_table_address_taken_from_the_first_lane() {
    let r = track_words(
        &[
            0xBE82_1F00, // 0x00: s_getpc_b64 s[2:3]
            0xD70F_0457,
            0x0000_054A, // v_add_co_u32 v87, s4, v74, s2
            0xD528_0458,
            0x0010_0680, // v_add_co_ci_u32 v88, s4, 0, s3, s4
            0x7E0C_0557, // v_readfirstlane_b32 s6, v87
            0x7E0E_0558, // v_readfirstlane_b32 s7, v88
            0xF400_0203,
            0xFA00_0000, // s_load_dword s8, s[6:7], 0x0
            S_ENDPGM,
        ],
        &[],
        HwStage::Cs,
    );
    assert_eq!(r.scalar.len(), 1);
    let a = r.scalar[0].1;
    assert_eq!((a.param, a.offset), (CODE, 4));
    assert!(a.dynamic.is_some(), "the lane's index is the computed term");
}

