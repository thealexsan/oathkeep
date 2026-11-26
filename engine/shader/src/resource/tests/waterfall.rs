use crate::native::{HwStage, RootType};
use crate::ShaderError;

use super::super::*;
use RootType::*;
use super::untraceable::{WATERFALL_FETCHED_WAVE32, WATERFALL_FETCHED_WAVE64, WATERFALL_INDEXED_WAVE32, WATERFALL_JOINED_WAVE32, WATERFALL_PICKED_WAVE32};
use super::{lanes, param};


#[test]
fn the_console_waterfall_over_a_first_lane_offset_is_one() {
    let r = waterfall(WATERFALL_INDEXED_WAVE32, false).unwrap();
    let image = r
        .descriptors
        .iter()
        .find(|d| d.role == Role::Resource)
        .unwrap();
    let Source::Loaded { load, first: 0 } = image.source else {
        panic!("{:?}", image.source)
    };
    let (_, address) = r.scalar.iter().find(|s| s.0 == load).unwrap();
    assert!(address.dynamic.is_some(), "{address:?}");
    assert_eq!(r.waterfall.compares.len(), 1);
    assert_eq!(r.waterfall.reads.len(), 1);
    assert_eq!(r.waterfall.loads, [load]);
}

/// [`WATERFALL_JOINED_WAVE32`] with other sources on each path: lanes
/// below 16 load table entry (lane & 1) with two `global_load_dwordx4`,
/// the others pick per lane with `v_cndmask_b32` (lanes below 24 the
/// first entry, the rest the second).
pub(crate) const WATERFALL_JOINED_MIXED_WAVE32: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xBE86_0301, // s_mov_b32 s6, s1
    0xBE87_0380, // s_mov_b32 s7, 0
    0xF40C_0602,
    0xFA00_0000, // s_load_dwordx8 s[24:31], s[4:5], 0x0
    0xF40C_0802,
    0xFA00_0020, // s_load_dwordx8 s[32:39], s[4:5], 0x20
    0xF408_0203,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
    0xF408_0A02,
    0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    0x7E04_0D00, // v_cvt_f32_u32 v2, v0
    0x7E06_0302, // v_mov_b32 v3, v2
    0x3420_0082, // v_lshlrev_b32 v16, 2, v0
    0x3602_0081, // v_and_b32 v1, 1, v0
    0x3402_0285, // v_lshlrev_b32 v1, 5, v1
    0x7D88_0090, // v_cmp_gt_u32 vcc_lo, 16, v0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xBE94_3C6A, // s_and_saveexec_b32 s20, vcc_lo
    0x8914_147E, // s_xor_b32 s20, exec_lo, s20
    0xBF88_0004, // s_cbranch_execz -> 0x68
    0xDC38_8000,
    0x0404_0001, // global_load_dwordx4 v[4:7], v1, s[4:5]
    0xDC38_8010,
    0x0804_0001, // global_load_dwordx4 v[8:11], v1, s[4:5] offset:16
    0xBE94_3D14, // 0x68: s_or_saveexec_b32 s20, s20
    0x897E_147E, // s_xor_b32 exec_lo, exec_lo, s20
    0xBF88_0011, // s_cbranch_execz -> 0xb8
    0x7D88_0098, // v_cmp_gt_u32 vcc_lo, 24, v0
    0x7E08_0218, // v_mov_b32 v4, s24
    0x0208_0820, // v_cndmask_b32 v4, s32, v4, vcc_lo
    0x7E0A_0219, // v_mov_b32 v5, s25
    0x020A_0A21, // v_cndmask_b32 v5, s33, v5, vcc_lo
    0x7E0C_021A, // v_mov_b32 v6, s26
    0x020C_0C22, // v_cndmask_b32 v6, s34, v6, vcc_lo
    0x7E0E_021B, // v_mov_b32 v7, s27
    0x020E_0E23, // v_cndmask_b32 v7, s35, v7, vcc_lo
    0x7E10_021C, // v_mov_b32 v8, s28
    0x0210_1024, // v_cndmask_b32 v8, s36, v8, vcc_lo
    0x7E12_021D, // v_mov_b32 v9, s29
    0x0212_1225, // v_cndmask_b32 v9, s37, v9, vcc_lo
    0x7E14_021E, // v_mov_b32 v10, s30
    0x0214_1426, // v_cndmask_b32 v10, s38, v10, vcc_lo
    0x7E16_021F, // v_mov_b32 v11, s31
    0x0216_1627, // v_cndmask_b32 v11, s39, v11, vcc_lo
    0x887E_147E, // 0xb8: s_or_b32 exec_lo, exec_lo, s20
    0xBE97_037E, // s_mov_b32 s23, exec_lo
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0x7E18_0504, // 0xc4: v_readfirstlane_b32 s12, v4
    0x7E1A_0505, // v_readfirstlane_b32 s13, v5
    0x7E1C_0506, // v_readfirstlane_b32 s14, v6
    0x7E1E_0507, // v_readfirstlane_b32 s15, v7
    0x7E20_0508, // v_readfirstlane_b32 s16, v8
    0x7E22_0509, // v_readfirstlane_b32 s17, v9
    0x7E24_050A, // v_readfirstlane_b32 s18, v10
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
    0xBF80_0000, // s_nop 0
    0xF09C_0F08,
    0x0043_0C02, // image_sample_lz v[12:15], v[2:3], s[12:19], s[8:11] dmask:0xf dim:SQ_RSRC_IMG_2D
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0x897E_157E, // s_xor_b32 exec_lo, exec_lo, s21
    0xBF89_FFE6, // s_cbranch_execnz -> 0xc4
    0xBEFE_0317, // s_mov_b32 exec_lo, s23
    0xE070_1000,
    0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
    0xBF81_0000, // s_endpgm
];

/// A waterfall loop whose descriptor changes on each pass: v[4:11] holds
/// the first table entry on entry and the second after every pass, so
/// the first lane's dwords are phis of the loop header.
pub(crate) const WATERFALL_LOOPED_WAVE32: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xBE86_0301, // s_mov_b32 s6, s1
    0xBE87_0380, // s_mov_b32 s7, 0
    0xF40C_0602,
    0xFA00_0000, // s_load_dwordx8 s[24:31], s[4:5], 0x0
    0xF40C_0802,
    0xFA00_0020, // s_load_dwordx8 s[32:39], s[4:5], 0x20
    0xF408_0203,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
    0xF408_0A02,
    0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    0x7E04_0D00, // v_cvt_f32_u32 v2, v0
    0x7E06_0302, // v_mov_b32 v3, v2
    0x3420_0082, // v_lshlrev_b32 v16, 2, v0
    0x7E08_0218, // v_mov_b32 v4, s24
    0x7E0A_0219, // v_mov_b32 v5, s25
    0x7E0C_021A, // v_mov_b32 v6, s26
    0x7E0E_021B, // v_mov_b32 v7, s27
    0x7E10_021C, // v_mov_b32 v8, s28
    0x7E12_021D, // v_mov_b32 v9, s29
    0x7E14_021E, // v_mov_b32 v10, s30
    0x7E16_021F, // v_mov_b32 v11, s31
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xBE97_037E, // s_mov_b32 s23, exec_lo
    0x7E18_0504, // 0x64: v_readfirstlane_b32 s12, v4
    0x7E1A_0505, // v_readfirstlane_b32 s13, v5
    0x7E1C_0506, // v_readfirstlane_b32 s14, v6
    0x7E1E_0507, // v_readfirstlane_b32 s15, v7
    0x7E20_0508, // v_readfirstlane_b32 s16, v8
    0x7E22_0509, // v_readfirstlane_b32 s17, v9
    0x7E24_050A, // v_readfirstlane_b32 s18, v10
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
    0xBF80_0000, // s_nop 0
    0xF09C_0F08,
    0x0043_0C02, // image_sample_lz v[12:15], v[2:3], s[12:19], s[8:11] dmask:0xf dim:SQ_RSRC_IMG_2D
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0x897E_157E, // s_xor_b32 exec_lo, exec_lo, s21
    0x7E08_0220, // v_mov_b32 v4, s32
    0x7E0A_0221, // v_mov_b32 v5, s33
    0x7E0C_0222, // v_mov_b32 v6, s34
    0x7E0E_0223, // v_mov_b32 v7, s35
    0x7E10_0224, // v_mov_b32 v8, s36
    0x7E12_0225, // v_mov_b32 v9, s37
    0x7E14_0226, // v_mov_b32 v10, s38
    0x7E16_0227, // v_mov_b32 v11, s39
    0xBF89_FFDE, // s_cbranch_execnz -> 0x64
    0xBEFE_0317, // s_mov_b32 exec_lo, s23
    0xE070_1000,
    0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
    0xBF81_0000, // s_endpgm
];

/// A waterfall loop over one of two buffer descriptors (four dwords)
/// loaded from the table at s0, chosen by nested if/else: lanes below 16
/// move the first into v[4:7], lanes from 16 to 23 the second, and the
/// others keep what v[4:7] held. Only lanes below 24 (the same mask)
/// run the loop, which loads dword `lane` of the chosen buffer, and
/// store it to the buffer at dword 16 of the table.
pub(crate) const WATERFALL_JOINED_BUFFER_WAVE32: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0602,
    0xFA00_0000, // s_load_dwordx4 s[24:27], s[4:5], 0x0
    0xF408_0802,
    0xFA00_0020, // s_load_dwordx4 s[32:35], s[4:5], 0x20
    0xF408_0A02,
    0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    0x3420_0082, // v_lshlrev_b32 v16, 2, v0
    0xD4C4_001E,
    0x0002_0098, // v_cmp_gt_u32_e64 s30, 24, v0
    0x7D88_0090, // v_cmp_gt_u32 vcc_lo, 16, v0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xBE94_3C6A, // s_and_saveexec_b32 s20, vcc_lo
    0x8914_147E, // s_xor_b32 s20, exec_lo, s20
    0xBF88_0004, // s_cbranch_execz -> 0x50
    0x7E08_0218, // 0x40: v_mov_b32 v4, s24
    0x7E0A_0219, // v_mov_b32 v5, s25
    0x7E0C_021A, // v_mov_b32 v6, s26
    0x7E0E_021B, // v_mov_b32 v7, s27
    0xBE94_3D14, // 0x50: s_or_saveexec_b32 s20, s20
    0x897E_147E, // s_xor_b32 exec_lo, exec_lo, s20
    0xBF88_0007, // s_cbranch_execz -> 0x78
    0xBE9F_3C1E, // s_and_saveexec_b32 s31, s30
    0xBF88_0004, // s_cbranch_execz -> 0x74
    0x7E08_0220, // 0x64: v_mov_b32 v4, s32
    0x7E0A_0221, // v_mov_b32 v5, s33
    0x7E0C_0222, // v_mov_b32 v6, s34
    0x7E0E_0223, // v_mov_b32 v7, s35
    0x887E_1F7E, // 0x74: s_or_b32 exec_lo, exec_lo, s31
    0x887E_147E, // 0x78: s_or_b32 exec_lo, exec_lo, s20
    0xBE97_3C1E, // s_and_saveexec_b32 s23, s30
    0xBF88_0013, // s_cbranch_execz -> 0xd0
    0xBE9D_037E, // s_mov_b32 s29, exec_lo
    0x7E18_0504, // 0x88: v_readfirstlane_b32 s12, v4
    0x7E1A_0505, // v_readfirstlane_b32 s13, v5
    0x7E1C_0506, // v_readfirstlane_b32 s14, v6
    0x7E1E_0507, // v_readfirstlane_b32 s15, v7
    0xD4E2_0015,
    0x0002_080C, // v_cmp_eq_u64_e64 s21, s[12:13], v[4:5]
    0xD4E2_0016,
    0x0002_0C0E, // v_cmp_eq_u64_e64 s22, s[14:15], v[6:7]
    0x8715_1615, // s_and_b32 s21, s21, s22
    0xBE95_3C15, // s_and_saveexec_b32 s21, s21
    0xE030_1000,
    0x8003_0C10, // buffer_load_dword v12, v16, s[12:15], 0 offen
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0x897E_157E, // s_xor_b32 exec_lo, exec_lo, s21
    0xBF89_FFF1, // s_cbranch_execnz -> 0x88
    0xBEFE_031D, // s_mov_b32 exec_lo, s29
    0xE070_1000,
    0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
    0x887E_177E, // 0xd0: s_or_b32 exec_lo, exec_lo, s23
    0xBF81_0000, // s_endpgm
];

pub(super) fn waterfall(words: &[u32], wave64: bool) -> Result<Resources, ShaderError> {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let p = crate::ir::build(&code, wave64, 256).unwrap();
    let layout = [param(Table, 0, 1), param(Table, 1, 1)];
    track(&p, &layout, HwStage::Cs, lanes(&p).as_ref())
}

#[test]
fn descriptors_read_from_the_first_lane_of_per_lane_loads_are_traced_to_them() {
    for (words, wave64) in [
        (WATERFALL_FETCHED_WAVE32, false),
        (WATERFALL_FETCHED_WAVE64, true),
    ] {
        let r = waterfall(words, wave64).unwrap();
        let image = r
            .descriptors
            .iter()
            .find(|d| d.role == Role::Resource)
            .unwrap();
        let Source::Fetched { load, first: 0 } = image.source else {
            panic!("{:?}", image.source)
        };
        // The first of the two loads: entry v0 * 32 of the table at s0.
        let a = r.scalar.iter().find(|s| s.0 == load).unwrap().1;
        assert_eq!((a.param, a.offset), (0, 0));
        assert!(a.dynamic.is_some());
        // The sampler is the same for every lane.
        let sampler = r.descriptors.iter().find(|d| d.role == Role::Sampler);
        assert!(matches!(
            sampler.unwrap().source,
            Source::Loaded { first: 0, .. }
        ));
        // Four 64-bit compares of the first lane's dwords with the lane's
        // own; the eight reads become the lane's own dwords.
        assert_eq!(r.waterfall.compares.len(), 4);
        assert_eq!(r.waterfall.reads.len(), 8);
    }
}

#[test]
fn descriptors_read_from_the_first_lane_of_a_per_lane_choice_are_traced_per_lane() {
    let r = waterfall(WATERFALL_PICKED_WAVE32, false).unwrap();
    let image = &r.descriptors[0];
    let Source::Lanes { args, .. } = &image.source else {
        panic!("{:?}", image.source)
    };
    // Lanes with their bit set take the first load (offset 0), the
    // others the second (0x20).
    let offsets: Vec<u32> = args
        .iter()
        .map(|a| match *a {
            Source::Loaded { load, first: 0 } => {
                r.scalar.iter().find(|s| s.0 == load).unwrap().1.offset
            }
            ref a => panic!("{a:?}"),
        })
        .collect();
    assert_eq!(offsets, [0, 0x20]);
    assert_eq!(r.waterfall.compares.len(), 4);
}

#[test]
fn a_first_lane_read_outside_a_loop_is_no_waterfall() {
    // The waterfall without its back edge: the s_cbranch_execnz is an
    // s_nop, so lanes the first one does not match never get a pass.
    let mut words = WATERFALL_FETCHED_WAVE32.to_vec();
    let back = words.iter().position(|&w| w == 0xBF89_FFE6).unwrap();
    words[back] = 0xBF80_0000;
    let err = waterfall(&words, false).unwrap_err();
    assert!(
        matches!(
            err,
            ShaderError::Unsupported {
                what: "a descriptor read from the first lane outside a waterfall loop",
                ..
            }
        ),
        "{err:?}"
    );
}

/// Why resource tracking refuses `words`.
pub(super) fn refusal(words: &[u32], wave64: bool) -> &'static str {
    match waterfall(words, wave64) {
        Err(ShaderError::Unsupported { what, .. }) => what,
        other => panic!("not refused: {other:?}"),
    }
}

const NO_LOOP: &str = "a descriptor read from the first lane outside a waterfall loop";
const NOT_SELECTED: &str = "a waterfall loop whose compares do not select the access's lanes";
pub(super) const BAD_RETRY: &str = "a waterfall loop whose retry does not keep the lanes still waiting";
pub(super) const SPLIT: &str = "a waterfall loop nested in another or split over blocks";

/// The index of `word` in `words`.
pub(super) fn word_at(words: &[u32], word: u32) -> usize {
    words.iter().position(|&w| w == word).unwrap()
}

#[test]
fn a_first_lane_read_without_its_compare_is_no_waterfall() {
    // The loop without the compare of dwords 0 and 1: nothing makes
    // the first lane's dwords the lane's own there.
    let mut words = WATERFALL_FETCHED_WAVE32.to_vec();
    let cmp = word_at(&words, 0x0002_080C);
    words[cmp - 1..=cmp].copy_from_slice(&[0xBF80_0000, 0xBF80_0000]);
    assert_eq!(refusal(&words, false), NOT_SELECTED);
}

#[test]
fn a_compare_that_does_not_mask_the_access_is_no_waterfall() {
    // The compares are all there, but their result never narrows EXEC:
    // every lane samples with the first lane's descriptor (reported by
    // ifBars on upstream PR #566).
    let mut words = WATERFALL_JOINED_WAVE32.to_vec();
    let save = word_at(&words, 0xBE95_3C15);
    words[save] = 0xBE95_037E; // s_mov_b32 s21, exec_lo
    assert_eq!(refusal(&words, false), NOT_SELECTED);
}

#[test]
fn an_access_under_a_further_mask_is_no_waterfall() {
    // The access runs for the matching lanes that VCC also holds, yet
    // the retry removes every matching lane.
    let mut words = WATERFALL_JOINED_WAVE32.to_vec();
    let nop = word_at(&words, 0xBE95_3C15) + 1;
    assert_eq!(words[nop], 0xBF80_0000);
    words[nop] = 0x877E_6A7E; // s_and_b32 exec_lo, exec_lo, vcc_lo
    assert_eq!(refusal(&words, false), NOT_SELECTED);
}

#[test]
fn a_loop_that_drops_unmatched_lanes_is_no_waterfall() {
    let mut words = WATERFALL_JOINED_WAVE32.to_vec();
    let retry = word_at(&words, 0x897E_157E);
    words[retry] = 0xBEFE_0380; // s_mov_b32 exec_lo, 0
    assert_eq!(refusal(&words, false), BAD_RETRY);
}

#[test]
fn a_retry_that_keeps_other_lanes_than_those_waiting_is_no_waterfall() {
    // The retry keeps the lanes of VCC that did not just sample, not
    // the waiting ones: lanes 16 and up never sample on the console.
    let mut words = WATERFALL_JOINED_WAVE32.to_vec();
    let retry = word_at(&words, 0x897E_157E);
    words[retry] = 0x8A7E_7E6A; // s_andn2_b32 exec_lo, vcc_lo, exec_lo
    assert_eq!(refusal(&words, false), BAD_RETRY);
}

#[test]
fn a_wave64_retry_of_one_half_is_no_waterfall() {
    // Only the low half of EXEC loses the lanes just served.
    let mut words = WATERFALL_FETCHED_WAVE64.to_vec();
    let retry = word_at(&words, 0x89FE_167E);
    words[retry] = 0x897E_167E; // s_xor_b32 exec_lo, exec_lo, s22
    assert_eq!(refusal(&words, true), BAD_RETRY);
}

#[test]
fn a_descriptor_used_after_exec_is_restored_is_no_waterfall() {
    let mut words = WATERFALL_JOINED_WAVE32.to_vec();
    let sample = word_at(&words, 0xF09C_0F08);
    let restore = word_at(&words, 0xBEFE_0317);
    let image = [words[sample], words[sample + 1]];
    words[sample..sample + 2].fill(0xBF80_0000); // Keep the branch offsets.
    words.splice(restore + 1..restore + 1, image);
    assert_eq!(refusal(&words, false), NO_LOOP);
}

#[test]
fn a_wave64_access_with_exec_hi_overwritten_is_no_waterfall() {
    let mut words = WATERFALL_FETCHED_WAVE64.to_vec();
    let save = word_at(&words, 0xBE96_2416);
    words[save + 1] = 0xBEFF_03C1; // s_mov_b32 exec_hi, -1
    assert_eq!(refusal(&words, true), NOT_SELECTED);
}

