use crate::native::{HwStage, RootType};
use crate::ShaderError;

use super::super::*;
use RootType::*;
use super::S_ENDPGM;
use super::waterfall::{BAD_RETRY, SPLIT, refusal, waterfall, word_at};
use super::{lanes, param};
use super::{WATERFALL_FETCHED_WAVE32, WATERFALL_FETCHED_WAVE64, WATERFALL_JOINED_BUFFER_WAVE32, WATERFALL_JOINED_MIXED_WAVE32, WATERFALL_JOINED_WAVE32, WATERFALL_LOOPED_WAVE32};


/// The waterfall with `v_cmpx_eq` narrowing EXEC itself, and the retry
/// as `todo & !EXEC` (`folded`: in one `s_andn2_wrexec`), as LLVM's
/// AMDGPU backend writes it for GFX10 and later.
pub(crate) fn cmpx_waterfall(wave64: bool, folded: bool) -> Vec<u32> {
    let (base, todo, restore): (&[u32], u32, u32) = if wave64 {
        (WATERFALL_FETCHED_WAVE64, 0xBE94_047E, 0xBEFE_0414)
    } else {
        (WATERFALL_JOINED_WAVE32, 0xBE97_037E, 0xBEFE_0317)
    };
    let reads = word_at(base, 0x7E18_0504);
    let sample = word_at(base, 0xF09C_0F08);
    let mut w = base[..=word_at(base, todo)].to_vec();
    // s_mov_b32 s21, exec_lo / s_mov_b64 s[22:23], exec
    w.push(if wave64 { 0xBE96_047E } else { 0xBE95_037E });
    w.extend(&base[reads..reads + 8]); // v_readfirstlane_b32 s12..s19
    w.extend([
        0x7DE4_080C, // v_cmpx_eq_u64 s[12:13], v[4:5]
        0x7DE4_0C0E, // v_cmpx_eq_u64 s[14:15], v[6:7]
        0x7DE4_1010, // v_cmpx_eq_u64 s[16:17], v[8:9]
        0x7DE4_1412, // v_cmpx_eq_u64 s[18:19], v[10:11]
    ]);
    w.extend(&base[sample..sample + 2]); // image_sample_lz
    match (wave64, folded) {
        // s_andn2_wrexec_b32 s21, s21; s_cbranch_execnz
        (false, true) => w.extend([0xBE95_4715, 0xBF89_FFF0]),
        // s_andn2_b32 s21, s21, exec_lo; s_mov_b32 exec_lo, s21
        (false, false) => w.extend([0x8A15_7E15, 0xBEFE_0315, 0xBF89_FFEF]),
        // s_andn2_wrexec_b64 s[22:23], s[22:23]
        (true, true) => w.extend([0xBE96_3A16, 0xBF89_FFF0]),
        // s_andn2_b64 s[22:23], s[22:23], exec; s_mov_b64 exec, s[22:23]
        (true, false) => w.extend([0x8A96_7E16, 0xBEFE_0416, 0xBF89_FFEF]),
    }
    w.extend(&base[word_at(base, restore)..]);
    w
}

#[test]
fn a_waterfall_narrowed_by_v_cmpx_is_one() {
    for wave64 in [false, true] {
        for folded in [false, true] {
            let r = waterfall(&cmpx_waterfall(wave64, folded), wave64)
                .unwrap_or_else(|e| panic!("wave64 {wave64} folded {folded}: {e:?}"));
            assert_eq!(r.waterfall.compares.len(), 4, "{wave64} {folded}");
            assert_eq!(r.waterfall.reads.len(), 8, "{wave64} {folded}");
        }
    }
}

#[test]
fn a_v_cmpx_waterfall_whose_retry_drops_lanes_is_none() {
    let mut words = cmpx_waterfall(false, false);
    let retry = word_at(&words, 0x8A15_7E15);
    words[retry] = 0x8A15_7E6A; // s_andn2_b32 s21, vcc_lo, exec_lo
    assert_eq!(refusal(&words, false), BAD_RETRY);
}

#[test]
fn a_waterfall_nested_in_another_is_refused_by_name() {
    // An image whose resource and sampler are both per lane, with one
    // loop per operand, the one for the sampler inside the one for the
    // resource (what the SelectionDAG legalisation in LLVM would give).
    // The console gives each lane its own pair. Refused for now: the
    // access of the inner loop reads the first-lane dwords of the outer
    // loop, which another block holds.
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xBE86_0301, // s_mov_b32 s6, s1
        0xBE87_0380, // s_mov_b32 s7, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
        0x7E04_0D00, // v_cvt_f32_u32 v2, v0
        0x7E06_0302, // v_mov_b32 v3, v2
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0x3602_0081, // v_and_b32 v1, 1, v0
        0x3402_0285, // v_lshlrev_b32 v1, 5, v1
        0x3622_0080, // v_and_b32 v17, 0, v0
        0x3422_2284, // v_lshlrev_b32 v17, 4, v17
        0xDC38_8000,
        0x0404_0001, // global_load_dwordx4 v[4:7], v1, s[4:5]
        0xDC38_8010,
        0x0804_0001, // global_load_dwordx4 v[8:11], v1, s[4:5] offset:16
        0xDC38_8000,
        0x1406_0011, // global_load_dwordx4 v[20:23], v17, s[6:7]
        0xBF8C_0070, // s_waitcnt vmcnt(0) lgkmcnt(0)
        0xBE94_037E, // s_mov_b32 s20, exec_lo
        // L1, the loop over the resource:
        0x7E18_0504, // v_readfirstlane_b32 s12, v4
        0x7E1A_0505,
        0x7E1C_0506,
        0x7E1E_0507,
        0x7E20_0508,
        0x7E22_0509,
        0x7E24_050A,
        0x7E26_050B, // v_readfirstlane_b32 s19, v11
        0xD4E2_0015,
        0x0002_080C, // v_cmp_eq_u64_e64 s21, s[12:13], v[4:5]
        0xD4E2_0016,
        0x0002_0C0E, // v_cmp_eq_u64_e64 s22, s[14:15], v[6:7]
        0x8715_1615, // s_and_b32 s21, s21, s22
        0xD4E2_0016,
        0x0002_1010, // v_cmp_eq_u64_e64 s22, s[16:17], v[8:9]
        0x8715_1615, // s_and_b32 s21, s21, s22
        0xD4E2_0016,
        0x0002_1412, // v_cmp_eq_u64_e64 s22, s[18:19], v[10:11]
        0x8715_1615, // s_and_b32 s21, s21, s22
        0xBE95_3C15, // s_and_saveexec_b32 s21, s21
        0xBE9A_037E, // s_mov_b32 s26, exec_lo
        // L2, the loop over the sampler:
        0x7E10_0514, // v_readfirstlane_b32 s8, v20
        0x7E12_0515, // v_readfirstlane_b32 s9, v21
        0x7E14_0516, // v_readfirstlane_b32 s10, v22
        0x7E16_0517, // v_readfirstlane_b32 s11, v23
        0xD4E2_0018,
        0x0002_2808, // v_cmp_eq_u64_e64 s24, s[8:9], v[20:21]
        0xD4E2_0019,
        0x0002_2C0A, // v_cmp_eq_u64_e64 s25, s[10:11], v[22:23]
        0x8718_1918, // s_and_b32 s24, s24, s25
        0xBE98_3C18, // s_and_saveexec_b32 s24, s24
        0xF09C_0F08,
        0x0043_0C02, // image_sample_lz v[12:15], v[2:3], s[12:19], s[8:11]
        0x897E_187E, // s_xor_b32 exec_lo, exec_lo, s24
        0xBF89_FFF2, // s_cbranch_execnz L2
        0xBEFE_031A, // s_mov_b32 exec_lo, s26
        0x897E_157E, // s_xor_b32 exec_lo, exec_lo, s21
        0xBF89_FFDA, // s_cbranch_execnz L1
        0xBEFE_0314, // s_mov_b32 exec_lo, s20
        0xE070_1000,
        0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
        0xBF81_0000, // s_endpgm
    ];
    assert_eq!(refusal(&words, false), SPLIT);
}

#[test]
fn descriptors_read_from_the_first_lane_of_lds_fail_naming_it() {
    // The waterfall over descriptors each lane read from LDS:
    // ds_read_b128 v[4:7], v1 and ds_read_b128 v[8:11], v1 offset:16
    // in place of the global loads.
    let mut words = WATERFALL_FETCHED_WAVE32.to_vec();
    words[5..9].copy_from_slice(&[0xDBFC_0000, 0x0400_0001, 0xDBFC_0010, 0x0800_0001]);
    let err = waterfall(&words, false).unwrap_err();
    assert!(
        matches!(
            err,
            ShaderError::Unsupported {
                what: "a descriptor read from the first lane of an LDS read",
                ..
            }
        ),
        "{err:?}"
    );
}

/// The block of `p` at byte `offset`.
fn block_at(words: &[u32], offset: usize) -> usize {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let p = crate::ir::build(&code, false, 256).unwrap();
    p.blocks.iter().position(|b| b.offset == offset).unwrap()
}

/// The table offset of what the scalar load of `s` reads.
fn table_offset(r: &Resources, s: &Source) -> u32 {
    match *s {
        Source::Loaded { load, first: 0 } => {
            r.scalar.iter().find(|x| x.0 == load).unwrap().1.offset
        }
        ref s => panic!("{s:?}"),
    }
}

#[test]
fn descriptors_read_from_the_first_lane_of_an_if_else_join_are_traced_per_path() {
    let words = WATERFALL_JOINED_WAVE32;
    let r = waterfall(words, false).unwrap();
    let image = r
        .descriptors
        .iter()
        .find(|d| matches!(d.source, Source::LanePhi { .. }))
        .unwrap();
    // The join after the else arm: lanes that ran it (lanes from 16 on)
    // take the second entry (0x20); the others come through the flow
    // block, whose join gives lanes that ran the then arm the first one
    // (0). No lane that skipped the then arm comes that way, so the
    // dwords they held before (the inputs) are not traced.
    let Source::LanePhi { block, args } = &image.source else {
        unreachable!()
    };
    assert_eq!(*block, block_at(words, 0x9c));
    let [Some(flow), Some(other)] = &args[..] else {
        panic!("{args:?}")
    };
    assert_eq!(table_offset(&r, other), 0x20);
    let Source::LanePhi { block, args } = flow else {
        panic!("{flow:?}")
    };
    assert_eq!(*block, block_at(words, 0x70));
    let [None, Some(then)] = &args[..] else {
        panic!("{args:?}")
    };
    assert_eq!(table_offset(&r, then), 0);
    // The loop's compares are of the joined dwords; still recognised.
    assert_eq!(r.waterfall.compares.len(), 4);
    assert_eq!(r.waterfall.reads.len(), 8);
    // Without the lanes' paths the join is refused.
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let p = crate::ir::build(&code, false, 256).unwrap();
    let layout = [param(Table, 0, 1), param(Table, 1, 1)];
    let err = track(&p, &layout, HwStage::Cs, None).unwrap_err();
    assert!(
        matches!(
            err,
            ShaderError::Unsupported {
                what: "a descriptor read from the first lane of a phi, without the lanes' paths",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn descriptors_joined_from_a_per_lane_load_and_a_per_lane_choice_are_traced_per_path() {
    let words = WATERFALL_JOINED_MIXED_WAVE32;
    let r = waterfall(words, false).unwrap();
    let image = r
        .descriptors
        .iter()
        .find(|d| matches!(d.source, Source::LanePhi { .. }))
        .unwrap();
    let Source::LanePhi { block, args } = &image.source else {
        unreachable!()
    };
    assert_eq!(*block, block_at(words, 0xb8));
    let [
        Some(Source::LanePhi { args: flow, .. }),
        Some(Source::Lanes { args: picked, .. }),
    ] = &args[..]
    else {
        panic!("{args:?}")
    };
    // The then arm loads each lane's entry from the table at s0.
    let [None, Some(Source::Fetched { load, first: 0 })] = flow[..] else {
        panic!("{flow:?}")
    };
    let a = r.scalar.iter().find(|s| s.0 == load).unwrap().1;
    assert_eq!((a.param, a.offset), (0, 0));
    assert!(a.dynamic.is_some());
    // The else arm picks the first entry where vcc is set.
    let offsets: Vec<u32> = picked.iter().map(|s| table_offset(&r, s)).collect();
    assert_eq!(offsets, [0, 0x20]);
    assert_eq!(r.waterfall.compares.len(), 4);
}

#[test]
fn a_descriptor_read_from_the_first_lane_of_a_loop_phi_fails_naming_it() {
    let err = waterfall(WATERFALL_LOOPED_WAVE32, false).unwrap_err();
    assert!(
        matches!(
            err,
            ShaderError::Unsupported {
                what: "a descriptor read from the first lane of a phi at a loop header",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn a_per_lane_descriptor_some_lanes_do_not_write_fails() {
    // The else arm of WATERFALL_JOINED_WAVE32 not skipped (its
    // s_cbranch_execz an s_nop): it runs with EXEC clear in the lanes
    // of the then arm, which keep the first entry; the second is not
    // every lane's.
    let mut words = WATERFALL_JOINED_WAVE32.to_vec();
    let skip = words.iter().rposition(|&w| w == 0xBF88_0008).unwrap();
    words[skip] = 0xBF80_0000;
    let err = waterfall(&words, false).unwrap_err();
    assert!(
        matches!(
            err,
            ShaderError::Unsupported {
                what: "a per-lane descriptor written where some lanes may keep the old one",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn a_buffer_descriptor_joined_per_lane_is_traced_on_the_paths_of_lanes_that_load() {
    let words = WATERFALL_JOINED_BUFFER_WAVE32;
    let r = waterfall(words, false).unwrap();
    let buffer = r
        .descriptors
        .iter()
        .find(|d| matches!(d.source, Source::LanePhi { .. }))
        .unwrap();
    // The outer join: lanes from the else arm's inner join, or (below
    // 16) through the flow block.
    let Source::LanePhi { block, args } = &buffer.source else {
        unreachable!()
    };
    assert_eq!(*block, block_at(words, 0x78));
    let [
        Some(Source::LanePhi { args: flow, .. }),
        Some(Source::LanePhi {
            block: inner,
            args: nested,
        }),
    ] = &args[..]
    else {
        panic!("{args:?}")
    };
    let [None, Some(then)] = &flow[..] else {
        panic!("{flow:?}")
    };
    assert_eq!(table_offset(&r, then), 0);
    // Lanes from 24 on skip the inner arm and keep the inputs, but do
    // not load: that edge is not traced.
    assert_eq!(*inner, block_at(words, 0x74));
    let [None, Some(other)] = &nested[..] else {
        panic!("{nested:?}")
    };
    assert_eq!(table_offset(&r, other), 0x20);
    assert_eq!(r.waterfall.compares.len(), 2);
    assert_eq!(r.waterfall.reads.len(), 4);
}

#[test]
fn a_guarded_access_excludes_dead_lane_edges_at_a_join() {
    // Lanes failing `v_cmp_gt_u32` clear EXEC and take `s_cbranch_execz`
    // directly to the join block without initializing the immediate
    // sampler `s[8:11]`. The join's `image_sample_lz` is guarded by EXEC,
    // so only the arm where `s[8:11]` was built can execute it.
    let code: Vec<u8> = [
        0xBE84_0300u32, // s_mov_b32 s4, s0
        0xBE85_0380,    // s_mov_b32 s5, 0
        0xF40C_0302,
        0xFA00_0000,    // s_load_dwordx8 s[12:19], s[4:5], 0x0
        0x7E04_0D00,    // v_cvt_f32_u32 v2, v0
        0x7E06_0302,    // v_mov_b32 v3, v2
        0x7D88_0090,    // v_cmp_gt_u32 vcc_lo, 16, v0
        0xBF8C_C07F,    // s_waitcnt lgkmcnt(0)
        0x877E_6A7E,    // s_and_b32 exec_lo, exec_lo, vcc_lo
        0xBF88_0004,    // s_cbranch_execz -> join (skips 4 dwords)
        0xBE88_0380,    // s_mov_b32 s8, 0
        0xBE89_0380,    // s_mov_b32 s9, 0
        0xBE8A_0380,    // s_mov_b32 s10, 0
        0xBE8B_0380,    // s_mov_b32 s11, 0
        0xF09C_0F08,
        0x0043_0C02,    // image_sample_lz v[12:15], v[2:3], s[12:19], s[8:11]
        S_ENDPGM,
    ]
    .iter()
    .flat_map(|w| w.to_le_bytes())
    .collect();
    let p = crate::ir::build(&code, false, 256).unwrap();
    let structured = lanes(&p).expect("structured form");
    assert!(structured.guards[2][0].is_some(), "join access must carry an EXEC guard");
    let r = track(&p, &[param(Table, 0, 1)], HwStage::Cs, Some(&structured)).unwrap();
    assert!(
        r.descriptors
            .iter()
            .any(|d| d.role == Role::Sampler && d.source == Source::Constant(vec![0; 4])),
        "guarded join should resolve the immediate sampler from the live arm: {:?}",
        r.descriptors
    );
}
