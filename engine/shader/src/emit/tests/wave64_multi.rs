//! A console wave64 compute program on a host of 32-lane waves: a wave reduction inside a
//! per-lane branch, with that branch taken wave-wide ([`crate::structure::structure_wide`])
//! so that the two host waves can exchange their halves inside it. Lowered, validated by
//! `dxil.dll` when present, and run on the host GPU when it takes waves of 32.

use std::collections::HashSet;

use super::super::host::{Dispatch, Host, Root, View};
use super::super::{HostShader, lower, validator};
use crate::native::{
    HwStage, StageIo,
};
use crate::{ShaderError, io, ir, resource, structure};



use super::wave64_lds::native_with_lds;

/// Negative safety check: a 128-thread shader whose widened branch can be entered
/// by BOTH console waves (`v0 < s0`) must be rejected by `multi_wave_wide_safe`.
#[test]
fn multi_wave_wide_safe_rejects_two_active_console_waves() {
    let body: &[u32] = &[
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0  (both waves can satisfy v0 < s0)
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0002, // s_cbranch_execz +2
        0xBE86_107E, // s_bcnt1_i32_b64 s6, exec
        0x7E00_0206, // v_mov_b32 v0, s6
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ];
    let native = native_with_lds(body, 128, 0);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure_wide(&p, &HashSet::from([0usize])).expect("structure");
    let io = io::raise(&p, &native).expect("io");
    assert!(
        !lower::multi_wave_wide_safe(&p, &st, &io, [128, 1, 1]),
        "branch where both console waves can be active must not be marked single-wave safe"
    );
}

/// Multi-wave (`group = 128`) shader combining:
/// 1) a single-console-wave (`v0 < 64`) divergent non-atomic LDS read+write region
///    with `s_bcnt1_i32_b64` (must be widened), and
/// 2) a separate two-console-wave (`v0 < s0`) divergent LDS region whose only write
///    is a disjoint atomic `ds_add_rtn_u32` (already safe under `lds_meetings`, must
///    NOT be pulled into `st.wide`).
/// Verifies targeted LDS region widening keeps `multi_wave_wide_safe` happy, validates
/// with `dxil.dll`, and computes exact results on the host GPU.
#[test]
fn multi_wave_widening_keeps_safe_atomic_lds_region_unwidened() {
    let body: &[u32] = &[
        // Phase 0 (tid == 0): lds[0] = 0, lds[4] = 5, lds[8] = 0
        0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0008, // s_cbranch_execz +8
        0x7E02_0280, // v_mov_b32 v1, 0
        0x7E04_0285, // v_mov_b32 v2, 5
        0xD834_0000, 0x0000_0101, // ds_write_b32 v1, v1 offset:0
        0xD834_0004, 0x0000_0201, // ds_write_b32 v1, v2 offset:4
        0xD834_0008, 0x0000_0101, // ds_write_b32 v1, v1 offset:8
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0xBF8A_0000, // s_barrier
        // Phase 1 (Console Wave 0 only: v0 < 64): non-atomic LDS read+write + s_bcnt1
        0xBE8C_047E, // s_mov_b64 s[12:13], exec
        0x7DA8_00C0, // v_cmpx_gt_u32 exec, 64, v0
        0xBF88_0007, // s_cbranch_execz +7
        0x7E02_0280, // v_mov_b32 v1, 0
        0xD8D8_0004, 0x0200_0001, // ds_read_b32 v2, v1 offset:4
        0xBE86_107E, // s_bcnt1_i32_b64 s6, exec
        0x4A04_0406, // v_add_nc_u32 v2, s6, v2
        0xD834_0008, 0x0000_0201, // ds_write_b32 v1, v2 offset:8
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0xBF8A_0000, // s_barrier
        // Phase 2 (BOTH console waves: v0 < s0): read lds[4], atomic add disjoint lds[0]
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0009, // s_cbranch_execz +9
        0x7E02_0280, // v_mov_b32 v1, 0
        0xD8D8_0004, 0x0200_0001, // ds_read_b32 v2, v1 offset:4
        0x7E06_0281, // v_mov_b32 v3, 1
        0xD880_0000, 0x0400_0301, // ds_add_rtn_u32 v4, v1, v3 offset:0
        0x7E0A_0288, // v_mov_b32 v5, 8
        0xE070_1000, 0x8002_0205, // buffer_store_dword v2, v5, s[8:11], 0 offen
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0xBF8A_0000, // s_barrier
        // Phase 3 (tid == 0): store lds[0] -> uav[0], lds[8] -> uav[1]
        0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_000A, // s_cbranch_execz +10
        0x7E02_0280, // v_mov_b32 v1, 0
        0xD8D8_0000, 0x0200_0001, // ds_read_b32 v2, v1 offset:0
        0xE070_1000, 0x8002_0201, // buffer_store_dword v2, v1, s[8:11], 0 offen
        0xD8D8_0008, 0x0200_0001, // ds_read_b32 v2, v1 offset:8
        0x7E02_0284, // v_mov_b32 v1, 4
        0xE070_1000, 0x8002_0201, // buffer_store_dword v2, v1, s[8:11], 0 offen
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ];
    let native = native_with_lds(body, 128, 1);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let mut wide = HashSet::new();
    let shader = loop {
        let st = structure::structure_wide(&p, &wide).expect("structure");
        let io = io::raise(&p, &native).expect("io");
        lower::take_needs();
        let was = lower::TEST_HOST_WAVE.replace(Some(32));
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource");
        let out = lower::lower(&lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        });
        lower::TEST_HOST_WAVE.set(was);
        let needs = lower::take_needs();
        match out {
            Ok(out) if out.wave_size == Some(64) && !needs.is_empty() => {
                let more = lower::branches_around(&st, &needs).expect("branches");
                assert!(!more.is_empty(), "stuck on wave64 LDS");
                wide.extend(more);
            }
            Ok(out) => {
                assert_eq!(out.wave_size, Some(32), "must lower to wave32");
                assert_eq!(wide.len(), 1, "only Phase 1 branch should be widened, got {wide:?}");
                assert!(
                    lower::multi_wave_wide_safe(&p, &st, &io, [128, 1, 1]),
                    "two-wave atomic LDS region must stay unwidened so multi_wave_wide_safe passes"
                );
                break HostShader {
                    stage: HwStage::Cs,
                    dxil: out.dxil,
                    user_data: out.user_data,
                    samplers: out.samplers,
                    static_samplers: Vec::new(),
                    root_cbvs: out.root_cbvs,
                    root_srvs: out.root_srvs,
                    root_uavs: out.root_uavs,
                    root_indexed_views: res.root_indexed_views,
                };
            }
            Err(ShaderError::Unsupported { what, .. }) if lower::needs_every_lane(what) => {
                let more = lower::branches_around(&st, &needs).expect("branches");
                assert!(!more.is_empty(), "stuck on {what}");
                wide.extend(more);
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_helpers::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for count in [1u32, 37, 95, 128] {
        let mut constants = vec![0u32; shader.user_data_dwords() as usize];
        constants[0] = count;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(4)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        assert_eq!(result[0][0], count, "atomic LDS counter for count={count}");
        assert_eq!(result[0][1], 69, "wave 0 widened LDS result for count={count}");
        assert_eq!(result[0][2], 5, "disjoint LDS read for count={count}");
    }
}

/// Single-console-wave (`8x8x1 = 64` threads) compute program with LDS whose
/// lane-divergent branch performs both an LDS write and an LDS read plus a
/// cross-half wave reduction (`WAVE_OR_V7_TO_S7`). Initial lowering returns
/// `Ok((_, Some(64)))` because `lds_meetings` rejects the divergent read+write
/// region; `emit::emit` must automatically widen the branch (`single_wave_lds`)
/// without requiring a hardcoded container digest, pass `dxil.dll` validation,
/// and produce exact results across both 32-lane host waves.
#[test]
fn single_wave_lds_branch_lowers_through_emit() {
    let mut body = vec![
        // Flatten 2D `[8, 8, 1]` thread ID (`v0 = x`, `v1 = y`) into `v0 = y * 8 + x` (`0..63`).
        0x3402_0283, // v_lshlrev_b32 v1, 3, v1
        0x4A00_0101, // v_add_nc_u32 v0, v1, v0
        // Inside `if (v0 < s0)`: write `(v0 + 10)` to `lds[v0 * 4]`, read `lds[0]`,
        // compute wave-wide OR of `1 << (v0 >> 1)`, and store `(wave_or + lds[0])`.
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0021, // s_cbranch_execz +33 (to restore of exec)
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x4A04_008A, // v_add_nc_u32 v2, 10, v0
        0xD834_0000, 0x0000_0201, // ds_write_b32 v1, v2 offset:0
        0x7E06_0280, // v_mov_b32 v3, 0
        0xD8D8_0000, 0x0400_0003, // ds_read_b32 v4, v3 offset:0
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
    ];
    body.extend_from_slice(&super::super::wave::WAVE_OR_V7_TO_S7);
    body.extend_from_slice(&[
        0x4A00_0807, // v_add_nc_u32 v0, s7, v4
        0xE070_1000, 0x8002_0003, // buffer_store_dword v0, v3, s[8:11], 0 offen
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    let mut native = native_with_lds(&body, 64, 1);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.threads = [8, 8, 1];
        cs.rsrc2 |= 1 << 11; // TIDIG_COMP_CNT = 1 (v0 = x, v1 = y)
    }
    let container = crate::dxbc::Container {
        digest: [0xaa; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource");
    let io = io::raise(&p, &native).expect("io");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let emitted = super::super::emit(&container, &native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    let shader = emitted.expect("single-wave LDS branch must lower to wave32 via emit()");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_helpers::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false).or_else(|_| Host::new(true)) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for count in [1u32, 19, 32, 33, 50, 64] {
        let mut constants = vec![0u32; shader.user_data_dwords() as usize];
        constants[0] = count;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(4)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        let expected_or = (0..count).fold(0u32, |acc, t| acc | (1u32 << (t >> 1)));
        assert_eq!(
            result[0][0],
            expected_or.wrapping_add(10),
            "widened 8x8x1 LDS + wave OR mismatch for count={count}"
        );
    }
}

