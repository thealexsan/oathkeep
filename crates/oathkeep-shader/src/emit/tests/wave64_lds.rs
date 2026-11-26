//! A console wave64 compute program on a host of 32-lane waves: a wave reduction inside a
//! per-lane branch, with that branch taken wave-wide ([`crate::structure::structure_wide`])
//! so that the two host waves can exchange their halves inside it. Lowered, validated by
//! `dxil.dll` when present, and run on the host GPU when it takes waves of 32.

use std::collections::HashSet;

use super::super::host::{Dispatch, Host, Root, View};
use super::super::{HostShader, lower, validator};
use crate::native::{
    HwStage, NativeProgram, StageIo,
};
use crate::{ShaderError, io, ir, resource, structure};


use super::wave64_core::{lowered, native_of};

pub(super) fn native_with_lds(body: &[u32], threads: u32, lds_blocks: u32) -> NativeProgram {
    let mut p = native_of(body, true, threads);
    if let StageIo::Cs(cs) = &mut p.io {
        // COMPUTE_PGM_RSRC2: lds_size sits in bits 15..23 (units of 128 dwords = 512 B).
        cs.rsrc2 |= (lds_blocks & 0x1FF) << 15;
    }
    p
}

/// Wave64 compute program with 128 threads and full 32 KiB LDS using:
/// 1) a single-lane `if (v0 == 0)` block that reads and writes LDS,
/// 2) a divergent branch `if (v0 < s0)` that reads `lds[8]` while reserving
///    per-wave slots via `ds_add_rtn_u32` at disjoint `lds[4]` (wave-local
///    allocation idiom).
/// Verifies `wave_local` accepts LDS atomics, `lds_meetings` accepts single-half
/// and disjoint-offset regions, local lowering adds zero extra LDS dwords (fits
/// within the 32 KiB DXIL TGSM limit), validates with `dxil.dll`, and runs
/// accurately on a Wave32 GPU.
#[test]
fn lds_atomic_wave_local_idiom_and_full_32k_lds_lower_to_wave32() {
    // s0 = active thread count, s[8:11] = raw UAV
    // Single-lane init (v0 == 0): lds[4] = 0, lds[8] = 7
    // s_barrier
    // Single-lane update (v0 == 0): lds[8] = lds[8] + 2 (= 9)
    // s_barrier
    // Divergent (v0 < s0):
    //   read v1 = lds[8] (offset:8)
    //   s_ff1_i32_b64 s16, exec
    //   v_mbcnt_lo/hi(-1, 0) -> v4 (console lane)
    //   v_mbcnt_lo/hi(exec, 0) -> v2 (prefix)
    //   save exec -> s[14:15]; v_cmpx_eq_i32 exec, s16, v4
    //   s_bcnt1_i32_b64 s17, s[14:15]
    //   v_mov_b32 v3, 0; v_mov_b32 v5, s17
    //   ds_add_rtn_u32 v3, v3, v5 offset:4
    //   s_mov_b64 exec, s[14:15]
    //   v_readfirstlane_b32 s18, v3
    //   v_add_nc_u32 v2, s18, v2   (unique slot index 0..s0)
    //   buffer_store_dword v1, (v2 << 2), s[8:11], 0 offen
    let body: &[u32] = &[
        // 0: v_cmp_eq_u32 vcc, 0, v0
        0x7D84_0080,
        // 1: s_and_saveexec_b64 s[12:13], vcc
        0xBE8C_246A,
        // 2: s_cbranch_execz +6 -> word 9
        0xBF88_0006,
        // 3: v_mov_b32 v1, 0
        0x7E02_0280,
        // 4: v_mov_b32 v2, 7
        0x7E04_0287,
        // 5-6: ds_write_b32 v1, v1 offset:4
        0xD834_0004,
        0x0000_0101,
        // 7-8: ds_write_b32 v1, v2 offset:8
        0xD834_0008,
        0x0000_0201,
        // 9: s_mov_b64 exec, s[12:13]
        0xBEFE_040C,
        // 10: s_barrier
        0xBF8A_0000,
        // 11: s_and_saveexec_b64 s[12:13], vcc  (single-lane read+write region)
        0xBE8C_246A,
        // 12: s_cbranch_execz +6 -> word 19
        0xBF88_0006,
        // 13: v_mov_b32 v1, 0
        0x7E02_0280,
        // 14-15: ds_read_b32 v2, v1 offset:8
        0xD8D8_0008,
        0x0200_0001,
        // 16: v_add_nc_u32 v2, 2, v2
        0x4A04_0482,
        // 17-18: ds_write_b32 v1, v2 offset:8
        0xD834_0008,
        0x0000_0201,
        // 19: s_mov_b64 exec, s[12:13]
        0xBEFE_040C,
        // 20: s_barrier
        0xBF8A_0000,
        // 21: v_cmp_gt_u32 vcc, s0, v0  (v0 < s0)
        0x7D88_0000,
        // 22: s_and_saveexec_b64 s[12:13], vcc
        0xBE8C_246A,
        // 23: s_cbranch_execz +25 -> word 49
        0xBF88_0019,
        // 24: v_mov_b32 v1, 0
        0x7E02_0280,
        // 25-26: ds_read_b32 v1, v1 offset:8  (disjoint from offset:4)
        0xD8D8_0008,
        0x0100_0001,
        // 27: s_ff1_i32_b64 s16, exec
        0xBE90_147E,
        // 28: s_mov_b64 s[14:15], exec
        0xBE8E_047E,
        // 29-30: v_mbcnt_lo_u32_b32 v2, -1, 0
        0xD765_0002,
        0x0001_00C1,
        // 31-32: v_mbcnt_hi_u32_b32 v4, -1, v2
        0xD766_0004,
        0x0002_04C1,
        // 33-34: v_mbcnt_lo_u32_b32 v3, s14, 0
        0xD765_0003,
        0x0001_000E,
        // 35-36: v_mbcnt_hi_u32_b32 v2, s15, v3
        0xD766_0002,
        0x0002_060F,
        // 37: v_cmpx_eq_i32 exec, s16, v4
        0x7D24_0810,
        // 38: s_bcnt1_i32_b64 s17, s[14:15]
        0xBE91_100E,
        // 39: v_mov_b32 v3, 0
        0x7E06_0280,
        // 40: v_mov_b32 v5, s17
        0x7E0A_0211,
        // 41-42: ds_add_rtn_u32 v3, v3, v5 offset:4
        0xD880_0004,
        0x0300_0503,
        // 43: s_mov_b64 exec, s[14:15]
        0xBEFE_040E,
        // 44: v_readfirstlane_b32 s18, v3
        0x7E24_0503,
        // 45: v_add_nc_u32 v2, s18, v2
        0x4A04_0412,
        // 46: v_lshlrev_b32 v2, 2, v2
        0x3404_0482,
        // 47-48: buffer_store_dword v1, v2, s[8:11], 0 offen
        0xE070_1000,
        0x8002_0102,
        // 49: s_mov_b64 exec, s[12:13]
        0xBEFE_040C,
    ];
    // 64 LDS blocks = 32768 bytes (full DXIL TGSM limit: zero extra LDS allowed!).
    let native = native_with_lds(body, 128, 64);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::wave_local(&p, &st), Ok(()), "LDS atomic reservation must match wave_local idiom");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource");
    let io = io::raise(&p, &native).expect("io");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let out = lower::with_local_waves(|| {
        lower::lower(&lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        })
    });
    lower::TEST_HOST_WAVE.set(was);
    let out = out.expect("local wave32 lowering with full 32 KiB LDS must succeed");
    assert_eq!(out.wave_size, Some(32));
    let shader = HostShader {
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
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed (TGSM must stay <= 32768 B)");
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
                views: &[View::RawUav(128)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        for i in 0..128usize {
            let expected = if (i as u32) < count { 9 } else { 0 };
            assert_eq!(result[0][i], expected, "count={count} slot={i}");
        }
    }
}

/// Multi-wave (`group = 128`, two console waves) Wave64 compute shader where:
/// - Console Wave 0 (`v0 < 64`) runs a scalar loop (`s0` iterations) with
///   `v_readlane_b32(..., 63)` and `s_bcnt1_i32_b64`, accumulating into `uav[0]`.
/// - Console Wave 1 (`!(v0 < 64)`) runs an independent scalar loop (`s1` iterations)
///   with `v_readlane_b32(..., 63)` and `s_bcnt1_i32_b64`, accumulating into `uav[1]`.
/// Verifies `multi_wave_wide_safe` accepts it, `group_wide` lowers both widened
/// and guarded scalar branches in lockstep across all 4 Wave32 host waves,
/// passes `dxil.dll` validation, and executes deadlock-free on the host GPU.
#[test]
fn multi_wave_single_wave_regions_with_scalar_loops_run_deadlock_free() {
    // s0 = wave 0 loop iterations (>= 1), s1 = wave 1 loop iterations (>= 1)
    // s[12:13] = initial exec
    // v_cmpx_gt_u32 exec, 64, v0     ; v0 < 64 (Console Wave 0)
    // s_cbranch_execz -> wave1
    //   v_mov_b32 v5, s0
    //   v_mov_b32 v6, 0
    // loop0:
    //   s_bcnt1_i32_b64 s6, exec     ; 64 (crosses halves -> forces widening!)
    //   v_add_nc_u32 v6, s6, v6      ; v6 += 64
    //   v_sub_nc_u32 v5, v5, 1       ; remaining -= 1
    //   v_readlane_b32 s7, v5, 63    ; lane 63 broadcasts remaining
    //   s_cmp_eq_u32 s7, 0
    //   s_cbranch_scc0 loop0         ; guarded scalar back-edge!
    //   v_mov_b32 v1, 0
    //   buffer_store_dword v6, v1, s[8:11], 0 offen  ; uav[0] = 64 * s0
    // wave1:
    //   s_andn2_b64 exec, s[12:13], exec ; !(v0 < 64) (Console Wave 1)
    //   s_cbranch_execz -> done
    //   v_mov_b32 v5, s1
    //   v_mov_b32 v6, 0
    // loop1:
    //   s_bcnt1_i32_b64 s6, exec     ; 64
    //   v_add_nc_u32 v6, s6, v6
    //   v_sub_nc_u32 v5, v5, 1
    //   v_readlane_b32 s7, v5, 63    ; lane 63 of wave 1 (thread 127)
    //   s_cmp_eq_u32 s7, 0
    //   s_cbranch_scc0 loop1         ; guarded scalar back-edge!
    //   v_mov_b32 v1, 4
    //   buffer_store_dword v6, v1, s[8:11], 0 offen  ; uav[1] = 64 * s1
    // done:
    //   s_mov_b64 exec, s[12:13]
    //   s_barrier
    let body: &[u32] = &[
        // 0: s_mov_b64 s[12:13], exec
        0xBE8C_047E,
        // 1: v_cmpx_gt_u32 exec, 64, v0
        0x7DA8_00C0,
        // 2: s_cbranch_execz +12 -> word 15 (wave1)
        0xBF88_000C,
        // 3: v_mov_b32 v5, s0
        0x7E0A_0200,
        // 4: v_mov_b32 v6, 0
        0x7E0C_0280,
        // 5 (loop0): s_bcnt1_i32_b64 s6, exec
        0xBE86_107E,
        // 6: v_add_nc_u32 v6, s6, v6
        0x4A0C_0C06,
        // 7: v_sub_nc_u32 v5, v5, 1   (v_subrev_nc_u32 v5, 1, v5)
        0x4E0A_0A81,
        // 8-9: v_readlane_b32 s7, v5, 63
        0xD760_0007,
        0x0001_7F05,
        // 10: s_cmp_eq_u32 s7, 0
        0xBF06_8007,
        // 11: s_cbranch_scc0 -7 -> word 5 (loop0)
        0xBF84_FFF9,
        // 12: v_mov_b32 v1, 0
        0x7E02_0280,
        // 13-14: buffer_store_dword v6, v1, s[8:11], 0 offen
        0xE070_1000,
        0x8002_0601,
        // 15 (wave1): s_andn2_b64 exec, s[12:13], exec
        0x8AFE_7E0C,
        // 16: s_cbranch_execz +12 -> word 29 (done)
        0xBF88_000C,
        // 17: v_mov_b32 v5, s1
        0x7E0A_0201,
        // 18: v_mov_b32 v6, 0
        0x7E0C_0280,
        // 19 (loop1): s_bcnt1_i32_b64 s6, exec
        0xBE86_107E,
        // 20: v_add_nc_u32 v6, s6, v6
        0x4A0C_0C06,
        // 21: v_subrev_nc_u32 v5, 1, v5
        0x4E0A_0A81,
        // 22-23: v_readlane_b32 s7, v5, 63
        0xD760_0007,
        0x0001_7F05,
        // 24: s_cmp_eq_u32 s7, 0
        0xBF06_8007,
        // 25: s_cbranch_scc0 -7 -> word 19 (loop1)
        0xBF84_FFF9,
        // 26: v_mov_b32 v1, 4
        0x7E02_0284,
        // 27-28: buffer_store_dword v6, v1, s[8:11], 0 offen
        0xE070_1000,
        0x8002_0601,
        // 29 (done): s_mov_b64 exec, s[12:13]
        0xBEFE_040C,
        // 30: s_barrier
        0xBF8A_0000,
    ];
    let native = native_with_lds(body, 128, 1);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    // Discover branches to widen iteratively just like emit::widen().
    let mut wide = HashSet::new();
    let shader = loop {
        let st = structure::structure_wide(&p, &wide).expect("structure");
        let io = io::raise(&p, &native).expect("io");
        lower::take_needs();
        match lowered(&native, &p, &wide) {
            Ok(s) => {
                assert!(
                    lower::multi_wave_wide_safe(&p, &st, &io, [128, 1, 1]),
                    "single-console-wave widened regions must pass safety check"
                );
                break s;
            }
            Err(ShaderError::Unsupported { what, .. }) if lower::needs_every_lane(what) => {
                let more = lower::branches_around(&st, &lower::take_needs()).expect("branches");
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
    for (iters0, iters1) in [(1u32, 1u32), (1, 5), (4, 1), (3, 7), (6, 2)] {
        let mut constants = vec![0u32; shader.user_data_dwords() as usize];
        constants[0] = iters0;
        constants[1] = iters1;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(4)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        assert_eq!(result[0][0], 64 * iters0, "wave 0 result for ({iters0}, {iters1})");
        assert_eq!(result[0][1], 64 * iters1, "wave 1 result for ({iters0}, {iters1})");
    }
}

