use crate::native::{HwStage, RootType};
use crate::ShaderError;

use super::super::*;
use RootType::*;
use super::S_ENDPGM;


use super::{lanes, param};

#[test]
fn untraceable_addresses_fail_with_a_reason() {
    let code: Vec<u8> = [0xF408_0001, 0xFA00_0000, S_ENDPGM] // s_load_dwordx4 s[0:3], s[2:3]
        .iter()
        .flat_map(|w: &u32| w.to_le_bytes())
        .collect();
    let p = crate::ir::build(&code, false, 256).unwrap();
    let err = track(&p, &[param(Table, 9, 1)], HwStage::Ps, lanes(&p).as_ref()).unwrap_err();
    assert_eq!(
        err,
        ShaderError::Unsupported {
            offset: 0,
            what: "an address in an SGPR that is no root parameter"
        }
    );
}

/// A waterfall loop over an image descriptor each lane loads from the
/// table at s0 (wave32, assembled with llvm-mc for gfx1030): lane i
/// reads entry i with two `global_load_dwordx4`, the loop reads the
/// first active lane's descriptor into s[12:19], samples with the lanes
/// whose descriptor it is, and repeats for the others. The sampler comes
/// from the table at s1; the result goes to LDS so the sample is needed.
pub(crate) const WATERFALL_FETCHED_WAVE32: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xBE86_0301, // s_mov_b32 s6, s1
    0xBE87_0380, // s_mov_b32 s7, 0
    0x3402_0085, // v_lshlrev_b32 v1, 5, v0
    0xDC38_8000,
    0x0404_0001, // global_load_dwordx4 v[4:7], v1, s[4:5]
    0xDC38_8010,
    0x0804_0001, // global_load_dwordx4 v[8:11], v1, s[4:5] offset:16
    0xF408_0203,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
    0x7E04_0D00, // v_cvt_f32_u32 v2, v0
    0x7E06_0302, // v_mov_b32 v3, v2
    0xBF8C_0070, // s_waitcnt vmcnt(0) lgkmcnt(0)
    0xBE94_037E, // s_mov_b32 s20, exec_lo
    0x7E18_0504, // 0x3c: v_readfirstlane_b32 s12, v4
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
    0xBF89_FFE6, // s_cbranch_execnz -> 0x3c
    0xBEFE_0314, // s_mov_b32 exec_lo, s20
    0xD834_0000,
    0x0000_0C00, // ds_write_b32 v0, v12
    0xBF81_0000, // s_endpgm
];

/// [`WATERFALL_FETCHED_WAVE32`] in wave64: 64-bit lane masks.
pub(crate) const WATERFALL_FETCHED_WAVE64: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xBE86_0301, // s_mov_b32 s6, s1
    0xBE87_0380, // s_mov_b32 s7, 0
    0x3402_0085, // v_lshlrev_b32 v1, 5, v0
    0xDC38_8000,
    0x0404_0001, // global_load_dwordx4 v[4:7], v1, s[4:5]
    0xDC38_8010,
    0x0804_0001, // global_load_dwordx4 v[8:11], v1, s[4:5] offset:16
    0xF408_0203,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
    0x7E04_0D00, // v_cvt_f32_u32 v2, v0
    0x7E06_0302, // v_mov_b32 v3, v2
    0xBF8C_0070, // s_waitcnt vmcnt(0) lgkmcnt(0)
    0xBE94_047E, // s_mov_b64 s[20:21], exec
    0x7E18_0504, // 0x3c: v_readfirstlane_b32 s12, v4
    0x7E1A_0505, // v_readfirstlane_b32 s13, v5
    0x7E1C_0506, // v_readfirstlane_b32 s14, v6
    0x7E1E_0507, // v_readfirstlane_b32 s15, v7
    0x7E20_0508, // v_readfirstlane_b32 s16, v8
    0x7E22_0509, // v_readfirstlane_b32 s17, v9
    0x7E24_050A, // v_readfirstlane_b32 s18, v10
    0x7E26_050B, // v_readfirstlane_b32 s19, v11
    0xD4E2_0016,
    0x0002_080C, // v_cmp_eq_u64_e64 s[22:23], s[12:13], v[4:5]
    0xD4E2_0018,
    0x0002_0C0E, // v_cmp_eq_u64_e64 s[24:25], s[14:15], v[6:7]
    0x8796_1816, // s_and_b64 s[22:23], s[22:23], s[24:25]
    0xD4E2_0018,
    0x0002_1010, // v_cmp_eq_u64_e64 s[24:25], s[16:17], v[8:9]
    0x8796_1816, // s_and_b64 s[22:23], s[22:23], s[24:25]
    0xD4E2_0018,
    0x0002_1412, // v_cmp_eq_u64_e64 s[24:25], s[18:19], v[10:11]
    0x8796_1816, // s_and_b64 s[22:23], s[22:23], s[24:25]
    0xBE96_2416, // s_and_saveexec_b64 s[22:23], s[22:23]
    0xBF80_0000, // s_nop 0
    0xF09C_0F08,
    0x0043_0C02, // image_sample_lz v[12:15], v[2:3], s[12:19], s[8:11] dmask:0xf dim:SQ_RSRC_IMG_2D
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0x89FE_167E, // s_xor_b64 exec, exec, s[22:23]
    0xBF89_FFE6, // s_cbranch_execnz -> 0x3c
    0xBEFE_0414, // s_mov_b64 exec, s[20:21]
    0xD834_0000,
    0x0000_0C00, // ds_write_b32 v0, v12
    0xBF81_0000, // s_endpgm
];

/// The same waterfall over one of two image descriptors loaded from the
/// table at s0, picked per lane by `v_cndmask_b32` (lanes below 16 take
/// the first).
pub(crate) const WATERFALL_PICKED_WAVE32: &[u32] = &[
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
    0x7E04_0D00, // v_cvt_f32_u32 v2, v0
    0x7E06_0302, // v_mov_b32 v3, v2
    0x7D88_0090, // v_cmp_gt_u32 vcc_lo, 16, v0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
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
    0xBE94_037E, // s_mov_b32 s20, exec_lo
    0x7E18_0504, // 0x7c: v_readfirstlane_b32 s12, v4
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
    0xBF89_FFE6, // s_cbranch_execnz -> 0x7c
    0xBEFE_0314, // s_mov_b32 exec_lo, s20
    0xD834_0000,
    0x0000_0C00, // ds_write_b32 v0, v12
    0xBF81_0000, // s_endpgm
];

/// The waterfall loop of [`WATERFALL_PICKED_WAVE32`] over one of two
/// image descriptors loaded from the table at s0, chosen by control flow
/// as the compiler lowers an if/else on a per-lane condition: lanes below
/// 16 move the first into v[4:11], the others the second, each arm
/// skipped when no lane takes it. The sample goes to the buffer at dword
/// 16 of the table, at lane * 4.
pub(crate) const WATERFALL_JOINED_WAVE32: &[u32] = &[
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
    0x7D88_0090, // v_cmp_gt_u32 vcc_lo, 16, v0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xBE94_3C6A, // s_and_saveexec_b32 s20, vcc_lo
    0x8914_147E, // s_xor_b32 s20, exec_lo, s20
    0xBF88_0008, // s_cbranch_execz -> 0x70
    0x7E08_0218, // v_mov_b32 v4, s24
    0x7E0A_0219, // v_mov_b32 v5, s25
    0x7E0C_021A, // v_mov_b32 v6, s26
    0x7E0E_021B, // v_mov_b32 v7, s27
    0x7E10_021C, // v_mov_b32 v8, s28
    0x7E12_021D, // v_mov_b32 v9, s29
    0x7E14_021E, // v_mov_b32 v10, s30
    0x7E16_021F, // v_mov_b32 v11, s31
    0xBE94_3D14, // 0x70: s_or_saveexec_b32 s20, s20
    0x897E_147E, // s_xor_b32 exec_lo, exec_lo, s20
    0xBF88_0008, // s_cbranch_execz -> 0x9c
    0x7E08_0220, // v_mov_b32 v4, s32
    0x7E0A_0221, // v_mov_b32 v5, s33
    0x7E0C_0222, // v_mov_b32 v6, s34
    0x7E0E_0223, // v_mov_b32 v7, s35
    0x7E10_0224, // v_mov_b32 v8, s36
    0x7E12_0225, // v_mov_b32 v9, s37
    0x7E14_0226, // v_mov_b32 v10, s38
    0x7E16_0227, // v_mov_b32 v11, s39
    0x887E_147E, // 0x9c: s_or_b32 exec_lo, exec_lo, s20
    0xBE97_037E, // s_mov_b32 s23, exec_lo
    0x7E18_0504, // 0xa4: v_readfirstlane_b32 s12, v4
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
    0xBF89_FFE6, // s_cbranch_execnz -> 0xa4
    0xBEFE_0317, // s_mov_b32 exec_lo, s23
    0xE070_1000,
    0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
    0xBF81_0000, // s_endpgm
];

/// The console compiler's waterfall (the title's ray-tracing
/// resolve and others): each lane holds its entry's byte offset in the
/// table at s0 (v7 = (lane & 1) * 32); the loop reads the first active
/// lane's offset, narrows EXEC to the lanes with the same one
/// (`v_cmpx_eq_u32`), loads that entry with `s_load_dwordx8` and samples,
/// then drops those lanes (`s_andn2_wrexec_b32`). The sample goes to the
/// buffer at dword 16 of the table, at lane * 4.
pub(crate) const WATERFALL_INDEXED_WAVE32: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xBE86_0301, // s_mov_b32 s6, s1
    0xBE87_0380, // s_mov_b32 s7, 0
    0xF408_0203,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
    0xF408_0A02,
    0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    0x7E04_0D00, // v_cvt_f32_u32 v2, v0
    0x7E06_0302, // v_mov_b32 v3, v2
    0x3420_0082, // v_lshlrev_b32 v16, 2, v0
    0x360E_0081, // v_and_b32 v7, 1, v0
    0x340E_0E85, // v_lshlrev_b32 v7, 5, v7
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xBE97_037E, // s_mov_b32 s23, exec_lo
    0xBE95_037E, // s_mov_b32 s21, exec_lo
    0x7E28_0507, // 0x40: v_readfirstlane_b32 s20, v7
    0x7DA4_0E14, // v_cmpx_eq_u32 exec_lo, s20, v7
    0xF40C_0302,
    0x2800_0000, // s_load_dwordx8 s[12:19], s[4:5], s20
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xF09C_0F08,
    0x0043_0C02, // image_sample_lz v[12:15], v[2:3], s[12:19], s[8:11] dmask:0xf dim:SQ_RSRC_IMG_2D
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xBE95_4715, // s_andn2_wrexec_b32 s21, s21
    0xBF89_FFF6, // s_cbranch_execnz -> 0x40
    0xBEFE_0317, // s_mov_b32 exec_lo, s23
    0xE070_1000,
    0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
    0xBF81_0000, // s_endpgm
];

