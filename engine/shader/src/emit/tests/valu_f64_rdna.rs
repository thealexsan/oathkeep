//! End-to-end tests of synthetic GFX10 VALU shaders lowered to DXIL, validated
//! by `dxil.dll` when present, and executed on the host GPU and WARP.
//!
//! Covers RDNA2 hardware semantics that differ from naive DXIL lowering:
//! `v_med3_f32`/`v_med3_f16` with NaN operands, `v_fract_f32`/`v_fract_f16`
//! near `1.0`, turn-range reduction for `v_sin_*`/`v_cos_*`, and the LDS
//! float atomics `ds_min_f32`/`ds_max_f32`.

use super::super::host::{Dispatch, Host, Root, View};
use super::super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};


use super::valu_helpers::*;

#[test]
fn whole_wave_f64_sum_and_cvt_f32_f64() {
    let s_or_saveexec_b32 =
        |sdst: u8, src0: u8| 0xBE80_0000 | (sdst as u32) << 16 | 0x3D << 8 | src0 as u32;
    let v_mov_dpp8 = |vdst: u8, vsrc: u8, sel: u32| [vop1(0x001, vdst, 0xE9), vsrc as u32 | (sel << 8)];
    let v_mov_row_mirror = |vdst: u8, vsrc: u8| {
        [
            vop1(0x001, vdst, 0xFA),
            vsrc as u32 | (0x140 << 8) | (1 << 19) | (0xF << 24) | (0xF << 28),
        ]
    };

    let mut words = Vec::new();
    words.push(s_mov_b32(5, 0x80));
    words.extend_from_slice(&s_load_dwordx4(8, 4));
    // Per-lane f64 input in v[6:7] = (s0, s1).
    words.push(vop1(0x001, 6, sgpr(0)));
    words.push(vop1(0x001, 7, sgpr(1)));
    // Whole-wave f64 sum over Wave32 with s_mov_b32 copy before EXEC restore.
    words.push(s_or_saveexec_b32(12, 0xC1));
    words.extend_from_slice(&vop3(0x101, 2, 0x80, vgpr(6), sgpr(12)));
    words.extend_from_slice(&vop3(0x101, 3, 0x80, vgpr(7), sgpr(12)));
    for sel in [14570689u32, 9658386, 2340] {
        words.extend_from_slice(&v_mov_dpp8(4, 2, sel));
        words.extend_from_slice(&v_mov_dpp8(5, 3, sel));
        words.extend_from_slice(&vop3(0x164, 2, vgpr(4), vgpr(2), 0)); // v_add_f64
    }
    words.extend_from_slice(&v_mov_row_mirror(4, 2));
    words.extend_from_slice(&v_mov_row_mirror(5, 3));
    words.extend_from_slice(&vop3(0x164, 2, vgpr(4), vgpr(2), 0));
    words.extend_from_slice(&vop3(0x378, 4, vgpr(2), 0x80, 0x80)); // v_permlanex16_b32
    words.extend_from_slice(&vop3(0x378, 5, vgpr(3), 0x80, 0x80));
    words.extend_from_slice(&vop3(0x164, 2, vgpr(4), vgpr(2), 0));
    words.extend_from_slice(&vop3(0x360, 14, vgpr(2), 0x80, 0)); // v_readlane_b32 s14, v2, 0
    words.extend_from_slice(&vop3(0x360, 15, vgpr(3), 0x80, 0)); // v_readlane_b32 s15, v3, 0
    words.push(s_mov_b32(16, 14));
    words.push(s_mov_b32(17, 15));
    words.push(s_mov_b32(126, 12)); // restore exec_lo
    words.push(vop1(0x00f, 0, sgpr(16))); // v_cvt_f32_f64 v0, s[16:17]
    words.extend_from_slice(&buffer_store_dword(0, 8, 0));
    words.push(S_ENDPGM);

    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 12,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [4, 1, 1],
        }),
        code,
    };

    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
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
    .expect("lower");

    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: Vec::new(),
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }

    let cases: &[(f64, f32)] = &[(1.25, 5.0), (-0.75, -3.0)];
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            continue;
        };
        let (root, pso) = match host.compute_pipeline(&shader) {
            Ok(p) => p,
            // WARP does not support fixed WaveSize(32) on SM 6.6.
            Err(e) if warp => {
                eprintln!("{label}: compute_pipeline skipped ({e})");
                continue;
            }
            Err(e) => panic!("{label}: compute_pipeline failed: {e}"),
        };
        for &(x, want) in cases {
            let b = x.to_bits();
            let inputs = [b as u32, (b >> 32) as u32, 0, 0];
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot) {
                    (RootType::Constants, 0) => inputs[u.dword as usize],
                    (RootType::Table, 1) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &[View::RawUav(1)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            assert_eq!(
                out[0][0],
                want.to_bits(),
                "{label}: wave f64 sum({x}) = {:#010x}, want {:#010x}",
                out[0][0],
                want.to_bits(),
            );
        }
    }
}

/// Whether the hardware adapter is an AMD RDNA card's: what the translator
/// writes for one ([`super::super::rdna`]) holds there.
fn rdna_card() -> bool {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::*;
    let Ok(host) = Host::new(false) else { return false };
    let mut waves = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: plain queries.
    unsafe {
        let amd = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter1>(host.device.GetAdapterLuid()))
            .and_then(|a| a.GetDesc1())
            .is_ok_and(|d| d.VendorId == 0x1002);
        amd && host
            .device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS1,
                (&raw mut waves).cast(),
                size_of_val(&waves) as u32,
            )
            .is_ok()
            && waves.WaveLaneCountMin == 32
            && waves.WaveLaneCountMax == 64
    }
}

/// The forms written for the console's own kind of hardware give, on such a
/// card, what the ones spelled out for every host give; a legacy product's
/// zero may have the other sign.
#[test]
fn rdna_forms_give_what_the_spelled_out_ones_give() {
    if !rdna_card() {
        eprintln!("no RDNA card; skipped");
        return;
    }
    const ONE: u16 = 242;
    const MINUS_ONE: u16 = 243;
    let (a, b, c) = (vgpr(1), vgpr(2), vgpr(3));
    let saturated = vop3_mod(0x110, 5, vgpr(1), vgpr(1), 0, 0, 0, 0, true);
    let two = |x: [u32; 2], y: [u32; 2]| [x, y].concat();
    let bodies: [(&str, Vec<u32>, bool); 14] = [
        ("max", vop3(0x110, 0, a, b, 0).to_vec(), true),
        ("min", vop3(0x10F, 0, a, b, 0).to_vec(), true),
        ("max3", vop3(0x154, 0, a, b, c).to_vec(), true),
        ("min3", vop3(0x151, 0, a, b, c).to_vec(), true),
        ("med3 0 1", vop3(0x157, 0, a, 128, ONE).to_vec(), true),
        ("med3 1 0", vop3(0x157, 0, ONE, a, 128).to_vec(), true),
        ("med3 1 -1", vop3(0x157, 0, ONE, MINUS_ONE, a).to_vec(), true),
        ("fract", vec![vop1(0x020, 0, a)], true),
        ("mul_legacy", vop3(0x107, 0, a, b, 0).to_vec(), false),
        ("mul_legacy by itself", vop3(0x107, 0, a, a, 0).to_vec(), false),
        ("mul_legacy by a constant", vop3(0x107, 0, a, MINUS_ONE, 0).to_vec(), false),
        ("mul_legacy of a saturated", two(saturated, vop3(0x107, 0, vgpr(5), b, 0)), false),
        ("mad_legacy", vop3(0x140, 0, a, b, c).to_vec(), false),
        ("max of a mul_legacy", two(vop3(0x107, 5, a, b, 0), vop3(0x110, 0, vgpr(5), c, 0)), false),
    ];
    let numbers = [0.0f32, -0.0, 1.0, -1.0, 0.3, -2.5e-7, 3e38, f32::INFINITY, f32::NEG_INFINITY, f32::NAN];
    let mut cases = Vec::new();
    for (i, x) in numbers.iter().enumerate() {
        for (j, y) in numbers.iter().enumerate() {
            let z = numbers[(3 * i + j) % numbers.len()];
            cases.push([x.to_bits(), y.to_bits(), z.to_bits(), 0]);
        }
    }
    for (name, body, exact) in bodies {
        let mut got: Vec<Vec<u32>> = Vec::new();
        let mut written = Vec::new();
        for bits in [0, super::super::rdna::ALL] {
            lower::TEST_RDNA.set(Some(bits));
            let shader = build_valu_cs(&body);
            lower::TEST_RDNA.set(None);
            written.push(shader.dxil.clone());
            let mut all = Vec::new();
            run_cases_on(&shader, &cases, &[("hardware", false)], |_, _, _, r| all.push(r));
            got.push(all);
        }
        assert_ne!(written[0], written[1], "{name}: written alike");
        for (case, (&was, &is)) in cases.iter().zip(got[0].iter().zip(&got[1])) {
            let (x, y) = (f32::from_bits(was), f32::from_bits(is));
            let same = was == is || x.is_nan() && y.is_nan() || !exact && x == 0.0 && y == 0.0;
            assert!(same, "{name} of {:?}: {x} ({was:#x}), now {y} ({is:#x})", case.map(f32::from_bits));
        }
    }
}

