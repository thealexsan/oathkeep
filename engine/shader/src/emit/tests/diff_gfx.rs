//! Differential tests: HLSL compiled by the console's shader compiler and
//! translated, against the same HLSL compiled for PC, both drawn (or
//! dispatched) on the host GPU with the same inputs.
//!
//! Needs `OATHKEEP_CONSOLE_DXC` (the console SDK's `dxc.exe`) and a PC
//! `dxc.exe` (`OATHKEEP_PC_DXC`, else the newest Windows SDK's); skipped
//! without them. Set `OATHKEEP_WARP=1` to draw on WARP.

use std::path::{Path, PathBuf};

use windows::Win32::Graphics::Direct3D12::*;

use super::super::host::{Draw, Host, Root};
use super::super::{HostShader, RootType, translate};


use super::diff_compute_cases::{CS_COMMON, CS_RS};
use super::diff_mesh::MS_COMMON;

const RS: &str = "RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT), \
                  RootConstants(num32BitConstants=4, b0), \
                  DescriptorTable(SRV(t0)), DescriptorTable(Sampler(s0)), \
                  StaticSampler(s1, filter=FILTER_MIN_MAG_MIP_POINT, \
                      addressU=TEXTURE_ADDRESS_WRAP, addressV=TEXTURE_ADDRESS_MIRROR), \
                  StaticSampler(s2, filter=FILTER_MIN_MAG_LINEAR_MIP_POINT, \
                      addressU=TEXTURE_ADDRESS_BORDER, addressV=TEXTURE_ADDRESS_CLAMP, \
                      borderColor=STATIC_BORDER_COLOR_OPAQUE_WHITE, mipLODBias=0.5)";

const COMMON: &str = r#"
cbuffer K : register(b0) { float4 k; };
Texture2D tex : register(t0);
SamplerState smp : register(s0);
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; float4 col : COLOR0; };
[RootSignature(RS)]
V vs(float4 p : POSITION, float2 uv : TEXCOORD0) {
    V o;
    o.pos = p;
    o.uv = uv;
    o.col = float4(uv * k.xy, p.x * k.z, 1);
    return o;
}
"#;

/// Pixel shaders, each `ps`.
const CASES: &[(&str, &str)] = &[
    (
        "constant",
        "[RootSignature(RS)] float4 ps(V i) : SV_Target { return float4(0.25, 0.5, 1, 0.75); }",
    ),
    (
        "interpolated",
        "[RootSignature(RS)] float4 ps(V i) : SV_Target { return i.col * 2 - float4(i.uv, 0, 0); }",
    ),
    (
        "sample and branch",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            float4 t = tex.Sample(smp, i.uv);
            float4 r = t * i.col + k.w;
            if (i.uv.x > 0.5) r.xy = sin(r.yx * 3.0);
            else r.z = rcp(r.z + 2.0);
            return r;
        }"#,
    ),
    // A wave-wide range scalarises a per-pixel loop (as the title's light
    // lists do): the console's 64-lane sections become host wave operations;
    // the result is the same for any wave the pixel lands in.
    (
        "wave range",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            uint idx = uint(i.pos.x * 0.37 + i.pos.y * 0.21) & 15;
            uint lo = WaveActiveMin(idx), hi = WaveActiveMax(idx);
            float s = 0;
            for (uint n = lo; n <= hi; n++) if (n == idx) s += n * k.x;
            return float4(s, lo <= idx, hi >= idx, WaveActiveCountBits(true) > 0);
        }"#,
    ),
    (
        "uniform loop",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            float s = 0;
            for (uint n = 0; n < (asuint(k.x) & 7); n++) s += i.uv.y * n;
            return float4(s, 0, 0, 1);
        }"#,
    ),
    (
        "bit loop",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            uint2 p = uint2(i.pos.xy);
            uint s = p.y;
            for (uint n = 0; n < (p.x & 7); n++) s = (s ^ (s << 3)) + n;
            return float4(s & 255, 0, 0, 1);
        }"#,
    ),
    (
        "integer loop",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            uint2 p = uint2(i.pos.xy);
            uint s = 0;
            for (uint n = 0; n < (p.x & 7); n++) s += n;
            return float4(s, 0, 0, 1);
        }"#,
    ),
    (
        "loop",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            uint2 p = uint2(i.pos.xy);
            float s = 0;
            for (uint n = 0; n < (p.x & 7); n++) s += i.uv.y * n + k.x;
            return float4(s, 0, 0, 1);
        }"#,
    ),
    (
        "discard",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            uint2 p = uint2(i.pos.xy);
            if ((p.x ^ p.y) & 1) discard;
            return i.col;
        }"#,
    ),
    (
        "immediate samplers",
        r#"SamplerState point_wrap : register(s1);
        SamplerState linear_border : register(s2);
        [RootSignature(RS)] float4 ps(V i) : SV_Target {
            return tex.Sample(point_wrap, i.uv * 3 - 1) + 2 * tex.Sample(linear_border, i.uv * 2.5 - 0.7);
        }"#,
    ),
    (
        "loop, discard and loads",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            uint2 p = uint2(i.pos.xy);
            if ((p.x ^ p.y) & 1) discard;
            float s = 0;
            for (uint n = 0; n < (p.x & 7); n++) s += i.uv.y * n + k.x;
            float4 t = tex.Load(int3(p & 3, 0));
            return float4(s, frac(i.uv.x * 7.3), max(t.x, t.y) * k.y, sqrt(abs(i.col.x)) + exp2(i.uv.y));
        }"#,
    ),
    (
        "level of detail",
        r#"[RootSignature(RS)] float4 ps(V i) : SV_Target {
            return float4(tex.CalculateLevelOfDetail(smp, i.uv * 4),
                tex.CalculateLevelOfDetailUnclamped(smp, i.uv * 7), 0, 1);
        }"#,
    ),
    // 16-bit outputs and inputs (one value per dword with the 2020
    // compiler; the title's two per dword cannot be compiled
    // here) and the front face.
    (
        "16-bit varyings and front face",
        r#"struct V16 { float4 pos : SV_Position; half2 uv : TEXCOORD0; float4 col : COLOR0; half4 h : TEXCOORD1; };
        [RootSignature(RS)] V16 vs16(float4 p : POSITION, float2 uv : TEXCOORD0) {
            V16 o;
            o.pos = p;
            o.uv = half2(uv * k.xy);
            o.col = float4(uv, p.x * k.z, 1);
            o.h = half4(uv.yx * 3 - 1, k.w, p.y);
            return o;
        }
        [RootSignature(RS)] float4 ps(V16 i, bool f : SV_IsFrontFace) : SV_Target {
            float4 c = i.col * float4(i.uv, i.h.xy) + float4(i.h.zw, 0, 0);
            return f ? c : -c;
        }"#,
    ),
];

pub(super) fn tools() -> Option<(PathBuf, PathBuf)> {
    let console = PathBuf::from(std::env::var_os("OATHKEEP_CONSOLE_DXC")?);
    let pc = match std::env::var_os("OATHKEEP_PC_DXC") {
        Some(p) => PathBuf::from(p),
        None => super::super::validator::Validator::default_path()?.with_file_name("dxc.exe"),
    };
    (console.exists() && pc.exists()).then_some((console, pc))
}

pub(super) fn compile(dxc: &Path, dir: &Path, src: &str, entry: &str, profile: &str) -> Vec<u8> {
    let input = dir.join("t.hlsl");
    let output = dir.join(format!("{entry}.bin"));
    let text = if entry == "cs" {
        format!("#define RS \"{CS_RS}\"\n{CS_COMMON}\n{src}\n")
    } else if entry.starts_with('m') {
        format!("{MS_COMMON}\n{src}\n")
    } else {
        format!("#define RS \"{RS}\"\n{COMMON}\n{src}\n")
    };
    std::fs::write(&input, text).unwrap();
    let out = std::process::Command::new(dxc)
        .args(["-T", profile, "-E", entry, "-Fo"])
        .arg(&output)
        .arg(&input)
        .args(src.contains("half").then_some("-enable-16bit-types"))
        // `CONSOLE` marks the console compile (cases spell console-only
        // intrinsics there).
        .args(
            std::env::var_os("OATHKEEP_CONSOLE_DXC")
                .is_some_and(|c| Path::new(&c) == dxc)
                .then_some("-DCONSOLE"),
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read(output).unwrap()
}

/// Root arguments for a translated stage: user data from the root values.
pub(super) fn user_data(s: &HostShader, k: &[u32; 4]) -> Vec<u32> {
    let mut data = vec![0; (s.user_data_dwords() as usize).max(1)];
    for u in &s.user_data {
        data[u.index as usize] = match (u.root, u.slot) {
            (RootType::Constants, 0) => k[u.dword as usize],
            // The texture and sampler tables start at heap index 1, the UAV
            // table (compute) at 5, in the console's units.
            (RootType::Table, 1) => 32,
            (RootType::Table, 2) => 16,
            (RootType::Table, 3) => 5 * 32,
            // The typed views of `raw` (compute) from 9.
            (RootType::Table, 6) => 9 * 32,
            other => panic!("unexpected user data {other:?}"),
        };
    }
    data
}

#[test]
fn translated_shaders_draw_as_pc_ones() {
    let Some((console, pc)) = tools() else {
        eprintln!("no console or PC compiler; skipped");
        return;
    };
    let host = match Host::new(false) {
        Ok(host) => host,
        Err(e) => {
            eprintln!("no SM 6.6 device ({e}); skipped");
            return;
        }
    };
    let dir = std::env::temp_dir().join(format!("victory-diff-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let k = [0.75f32, -1.5, 2.0, 0.125].map(f32::to_bits);
    let texels: Vec<[f32; 4]> = (0..16)
        .map(|i| {
            [
                i as f32 / 16.0,
                (i % 4) as f32 * 0.3,
                1.0 - i as f32 / 20.0,
                0.5,
            ]
        })
        .collect();
    let dummy = [[9.0f32; 4]];
    let sampler = |filter, address| D3D12_SAMPLER_DESC {
        Filter: filter,
        AddressU: address,
        AddressV: address,
        AddressW: address,
        MaxAnisotropy: 1,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let samplers = [
        sampler(
            D3D12_FILTER_MIN_MAG_MIP_POINT,
            D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        ),
        sampler(
            D3D12_FILTER_MIN_MAG_MIP_LINEAR,
            D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        ),
    ];
    // A full-screen triangle: POSITION (4 floats), TEXCOORD0 (2 floats).
    let vertices = [
        -1.0, -1.0, 0.5, 1.0, 0.0, 1.0, //
        -1.0, 3.0, 0.5, 1.0, 0.0, -1.0, //
        3.0, -1.0, 0.5, 1.0, 2.0, 1.0,
    ];
    let textures: [(u32, u32, &[[f32; 4]]); 2] = [(1, 1, &dummy), (4, 4, &texels)];
    let draw = |roots| Draw {
        roots,
        textures: &textures,
        samplers: &samplers,
        vertices: &vertices,
        stride: 24,
        size: (16, 16),
        mesh_groups: 0,
    };
    let mut failures = Vec::new();
    let only = std::env::var("OATHKEEP_DIFF_ONLY").ok();
    for (name, src) in CASES
        .iter()
        .filter(|c| only.as_ref().is_none_or(|o| o == c.0))
    {
        // A case may bring its own vertex shader.
        let vs = if src.contains(" vs16(") { "vs16" } else { "vs" };
        let vs_c = translate(&compile(&console, &dir, src, vs, "vs_6_5")).unwrap();
        let ps_c = match translate(&compile(&console, &dir, src, "ps", "ps_6_5")) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{name}: {e}"));
                continue;
            }
        };
        if let Ok(out) = std::env::var("OATHKEEP_EMIT_DUMP") {
            let stem = name.replace([' ', ','], "_");
            std::fs::write(
                format!("{out}/{stem}.ps.cdxil"),
                compile(&console, &dir, src, "ps", "ps_6_5"),
            )
            .unwrap();
            std::fs::write(format!("{out}/{stem}.ps.dxil"), &ps_c.dxil).unwrap();
        }
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            for s in [&vs_c, &ps_c] {
                v.validate(&mut s.dxil.clone())
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
            }
        }
        let (root_t, pso_t) = host.pipeline(&vs_c, &ps_c).unwrap();
        let ours = host
            .draw(
                &root_t,
                &pso_t,
                &draw(vec![
                    (0, Root::Constants(user_data(&vs_c, &k))),
                    (1, Root::Constants(user_data(&ps_c, &k))),
                ]),
            )
            .unwrap();

        let (vs_p, ps_p) = (
            compile(&pc, &dir, src, vs, "vs_6_5"),
            compile(&pc, &dir, src, "ps", "ps_6_5"),
        );
        // SAFETY: the PC container carries its root signature.
        let root_p: ID3D12RootSignature =
            unsafe { host.device.CreateRootSignature(0, &ps_p) }.unwrap();
        let pso_p = host.pipeline_with(&root_p, &vs_p, &ps_p).unwrap();
        let theirs = host
            .draw(
                &root_p,
                &pso_p,
                &draw(vec![
                    (0, Root::Constants(k.to_vec())),
                    (
                        1,
                        Root::Table {
                            sampler: false,
                            index: 1,
                        },
                    ),
                    (
                        2,
                        Root::Table {
                            sampler: true,
                            index: 1,
                        },
                    ),
                ]),
            )
            .unwrap();

        let worst = ours
            .iter()
            .flatten()
            .zip(theirs.iter().flatten())
            .enumerate()
            .map(|(i, (a, b))| (((a - b).abs() / b.abs().max(1.0)), i, *a, *b))
            .max_by(|x, y| x.0.total_cmp(&y.0))
            .unwrap();
        eprintln!(
            "{name}: worst relative difference {:.2e} (at {}: {} vs {})",
            worst.0, worst.1, worst.2, worst.3
        );
        if worst.0.is_nan() || worst.0 > 1e-3 {
            failures.push(format!("{name}: {worst:?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{failures:#?}");
}

