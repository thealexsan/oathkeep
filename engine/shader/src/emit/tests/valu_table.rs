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


/// MUBUF `op vdata, vaddr, s[srsrc:srsrc+3], 0 idxen`.
const fn mubuf_idxen(op: u8, vdata: u8, vaddr: u8, srsrc: u8) -> [u32; 2] {
    [
        0x38 << 26 | (op as u32) << 18 | 1 << 13,
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// SMEM `s_load_dwordx4 s[dst:dst+3], s[base:base+1], byte_offset` (soffset = null).
const fn s_load_dwordx4_off(dst: u8, base: u8, byte_offset: u32) -> [u32; 2] {
    [
        0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        (125 << 25) | (byte_offset & 0x1F_FFFF),
    ]
}

/// Builds a compute shader whose descriptors live in a `RootType::Table` at
/// `s4` (like `compute_graph_ops` and `ped_standard_head_cs`).
fn build_table_typed_cs(body: &[u32], used: Vec<UsedResource>) -> HostShader {
    let mut words = Vec::new();
    words.push(s_mov_b32(5, 0x80)); // s5 = 0
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 4,
            slot: 0,
        }],
        used,
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
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
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// `compute_graph_ops` (facial lip-sync & wrapped submesh solver) binds
/// `R16G16B16A16_UINT` / `R16_UINT` UAVs via `RootType::Table` (which on NVIDIA
/// are snapshotted into dynamic descriptor slots) and reads/writes FP16 bit
/// patterns with `buffer_load_format_d16_xyzw` and `buffer_store_format_d16_xyzw`.
/// Verify bit-exact round-trips for UINT, SINT, and FLOAT views (including
/// non-zero table offsets matching dynamic descriptor snapshots) on hardware
/// GPU and WARP.
#[test]
fn typed_d16_table_uav_roundtrip_uint_sint_and_float() {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16B16A16_SINT,
        DXGI_FORMAT_R16G16B16A16_UINT,
    };
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut body = Vec::new();
    // s[8:11] = table[0] (src typed UAV), s[12:15] = table[1] (dst typed UAV),
    // s[16:19] = table[2] (raw UAV dump of loaded v[2:3]).
    body.extend_from_slice(&s_load_dwordx4_off(8, 4, 0));
    body.extend_from_slice(&s_load_dwordx4_off(12, 4, 32));
    body.extend_from_slice(&s_load_dwordx4_off(16, 4, 64));
    // buffer_load_format_d16_xyzw v[2:3], v0, s[8:11], 0 idxen (op 0x83)
    body.extend_from_slice(&mubuf_idxen(0x83, 2, 0, 8));
    // buffer_store_format_d16_xyzw v[2:3], v0, s[12:15], 0 idxen (op 0x87)
    body.extend_from_slice(&mubuf_idxen(0x87, 2, 0, 12));
    // Also dump thread 0's loaded v[2:3] into raw UAV (table[2]) to verify the load itself.
    // v4 = v0 << 3 (byte offset = thread * 8)
    body.push(0x3408_0083); // v_lshlrev_b32 v4, 3, v0
    // buffer_store_dwordx2 v[2:3], v4, s[16:19], 0 offen (op 0x1D, offen bit 12)
    body.extend_from_slice(&[
        0x38 << 26 | 0x1D << 18 | 1 << 12,
        0x80 << 24 | (16u32 >> 2) << 16 | (2u32 << 8) | 4u32,
    ]);
    let used = (0..3u32)
        .map(|k| UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: k * 8,
            lower_bound: k,
            stride: 4,
            dwords: 4,
            user_data: 4,
            slot: 0,
        })
        .collect();
    let shader = build_table_typed_cs(&body, used);
    assert!(
        shader.root_srvs.contains(&super::super::SLOT_TABLE_STRIDES),
        "table typed d16 shader should request SLOT_TABLE_STRIDES"
    );
    assert!(
        !shader.root_srvs.contains(&super::super::SLOT_STRIDES),
        "table-only shader must not trigger the bindless SLOT_STRIDES sweep"
    );

    // 4 elements x 4 u16 channels = 8 u32 dwords.
    // Includes FP16 subnormal (0x0001), -0.0 (0x8000), 1.0 (0x3C00), -1.0 (0xBC00),
    // NaN (0x7E00), max u16 (0xFFFF), and arbitrary patterns.
    let uint_bits: [u32; 8] = [
        0x3C00_0001, 0x7E00_BC00,
        0xFFFF_8000, 0xABCD_1234,
        0x0002_0000, 0x7BFF_FBFF,
        0xDEAD_BEEF, 0xCAFE_BABE,
    ];
    let sint_bits: [u32; 8] = [
        0xFFFF_8000, 0x7FFF_0001,
        0xC000_0000, 0x8001_7FFE,
        0xFFFF_FFFF, 0x0000_0000,
        0xFEDC_BA98, 0x7654_3210,
    ];
    // Normal finite FP16 values that round-trip cleanly through f32.
    let float_bits: [u32; 8] = [
        0x4000_3C00, 0x3800_BC00, // 1.0, 2.0, -1.0, 0.5
        0xC200_3400, 0x4500_0000, // 0.25, -3.0, 0.0, 5.0
        0x3E00_BE00, 0x4400_C400, // -1.5, 1.5, -4.0, 4.0
        0x3C00_0000, 0x4200_C000, // 0.0, 1.0, -2.0, 3.0
    ];
    let zeros = [0u32; 8];

    let cases = [
        ("R16G16B16A16_UINT @0", DXGI_FORMAT_R16G16B16A16_UINT, 8 | super::super::VIEW_INTEGER, &uint_bits, 0u32),
        ("R16G16B16A16_UINT @3 (dynamic offset)", DXGI_FORMAT_R16G16B16A16_UINT, 8 | super::super::VIEW_INTEGER, &uint_bits, 3u32),
        ("R16G16B16A16_SINT @0", DXGI_FORMAT_R16G16B16A16_SINT, 8 | super::super::VIEW_INTEGER | super::super::VIEW_SIGNED, &sint_bits, 0u32),
        ("R16G16B16A16_FLOAT @0", DXGI_FORMAT_R16G16B16A16_FLOAT, 8, &float_bits, 0u32),
    ];

    for (backend, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else { continue };
        let (root, pso) = host
            .compute_pipeline(&shader)
            .unwrap_or_else(|e| panic!("{backend}: compute_pipeline failed: {e}"));
        for &(label, format, stride_word, input, base_slot) in &cases {
            let mut views = Vec::new();
            let mut strides = Vec::new();
            for _ in 0..base_slot {
                views.push(View::RawUav(1));
                strides.push(0u32);
            }
            let uav_base = base_slot as usize;
            views.push(View::TypedUav { data: input, format, elem_bytes: 8 });
            strides.push(stride_word);
            views.push(View::TypedUav { data: &zeros, format, elem_bytes: 8 });
            strides.push(stride_word);
            views.push(View::RawUav(8));
            strides.push(1);

            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                if u.root == RootType::Table && u.slot == 0 {
                    ud[u.index as usize] = base_slot * 32;
                }
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![
                            (0, Root::Constants(ud)),
                            (1, Root::View { srv: true, data: strides }),
                        ],
                        views: &views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{backend} {label}: dispatch failed: {e}"));
            assert_eq!(
                out[uav_base + 2],
                input,
                "{backend} {label}: buffer_load_format_d16_xyzw mismatch"
            );
            assert_eq!(
                out[uav_base + 1],
                input,
                "{backend} {label}: buffer_store_format_d16_xyzw mismatch"
            );
        }
    }
}

/// Non-`d16` typed loads (`buffer_load_format_x`) are used by
/// `CS_BlendshapeTranslationsHSSGraphOutputExtract` (reading `HSSGraphOut`
/// `R16_UINT`) and `CS_PedStandardSkinning_*` (reading bone indices). Verify
/// exact integer bits for `R16_UINT`, `R16_SINT`, and `R32_UINT`.
#[test]
fn typed_non_d16_table_load_preserves_integer_bits() {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_R16_SINT, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R32_UINT,
    };
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut body = Vec::new();
    // s[8:11] = table[0] (src typed SRV), s[12:15] = table[1] (raw UAV out)
    body.extend_from_slice(&s_load_dwordx4_off(8, 4, 0));
    body.extend_from_slice(&s_load_dwordx4_off(12, 4, 32));
    // buffer_load_format_x v2, v0, s[8:11], 0 idxen (op 0x00)
    body.extend_from_slice(&mubuf_idxen(0x00, 2, 0, 8));
    // v4 = v0 << 2 (byte offset = thread * 4)
    body.push(0x3408_0082); // v_lshlrev_b32 v4, 2, v0
    // buffer_store_dword v2, v4, s[12:15], 0 offen (op 0x1C, offen bit 12)
    body.extend_from_slice(&[
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | (12u32 >> 2) << 16 | (2u32 << 8) | 4u32,
    ]);
    let used = vec![
        UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 4,
            dwords: 4,
            user_data: 4,
            slot: 0,
        },
        UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 8,
            lower_bound: 1,
            stride: 4,
            dwords: 4,
            user_data: 4,
            slot: 0,
        },
    ];
    let shader = build_table_typed_cs(&body, used);
    // Non-d16 typed loads stay untouched (no SLOT_TABLE_STRIDES root SRV, no
    // CFG branches) so non-d16 pixel/mesh/particle/skinning shaders keep
    // identical DXIL and pipeline cache keys while still preserving raw bits.
    assert!(
        !shader.root_srvs.contains(&super::super::SLOT_TABLE_STRIDES),
        "non-d16 typed load should not add SLOT_TABLE_STRIDES"
    );

    let r16_data = [0x3C00_0001u32, 0xFFFF_BC00u32];
    let r32_data = [0xDEAD_BEEFu32, 0x0000_0001, 0x8000_0000, 0x7FFF_FFFF];
    let cases: [(&str, _, u32, &[u32], [u32; 4]); 3] = [
        (
            "R16_UINT",
            DXGI_FORMAT_R16_UINT,
            2,
            &r16_data,
            [0x0001, 0x3C00, 0xBC00, 0xFFFF],
        ),
        (
            "R16_SINT",
            DXGI_FORMAT_R16_SINT,
            2,
            &r16_data,
            [0x0001, 0x3C00, 0xFFFF_BC00, 0xFFFF_FFFF],
        ),
        (
            "R32_UINT",
            DXGI_FORMAT_R32_UINT,
            4,
            &r32_data,
            r32_data,
        ),
    ];

    for (backend, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else { continue };
        let (root, pso) = host
            .compute_pipeline(&shader)
            .unwrap_or_else(|e| panic!("{backend}: compute_pipeline failed: {e}"));
        for &(label, format, elem_bytes, input, want) in &cases {
            let views = [
                View::Buffer {
                    data: input,
                    stride: super::super::host::Stride::Format(format, elem_bytes),
                },
                View::RawUav(4),
            ];
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                if u.root == RootType::Table && u.slot == 0 {
                    ud[u.index as usize] = 0;
                }
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{backend} {label}: dispatch failed: {e}"));
            assert_eq!(out[0], want, "{backend} {label}: buffer_load_format_x mismatch");
        }
    }
}
