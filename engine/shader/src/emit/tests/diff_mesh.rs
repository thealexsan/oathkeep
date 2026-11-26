//! Differential tests: HLSL compiled by the console's shader compiler and
//! translated, against the same HLSL compiled for PC, both drawn (or
//! dispatched) on the host GPU with the same inputs.
//!
//! Needs `OATHKEEP_CONSOLE_DXC` (the console SDK's `dxc.exe`) and a PC
//! `dxc.exe` (`OATHKEEP_PC_DXC`, else the newest Windows SDK's); skipped
//! without them. Set `OATHKEEP_WARP=1` to draw on WARP.


use windows::Win32::Graphics::Direct3D12::*;

use super::super::host::{Draw, Host, Root};
use super::super::translate;


use super::diff_gfx::{compile, tools, user_data};

pub(super) const MS_COMMON: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0)"
cbuffer K : register(b0) { float4 k; };
struct V { float4 pos : SV_Position; float4 col : COLOR0; float2 uv : TEXCOORD0; };
[RootSignature(RS)] float4 mps(V i) : SV_Target { return i.col + float4(i.uv, 0, 0); }
#define MS_ATTR [RootSignature(RS)] [outputtopology("triangle")]
"#;

/// Mesh shaders, each `ms` (the console runs one thread per output lane, so
/// groups of 32 run as 64 lanes there), with their group counts. Drawn
/// with `mps` into 16×16 targets. The 2020 compiler's containers do not
/// record the thread count, so cases reading the group id keep
/// max(threads, vertices, primitives) at the lane count (see `emit`).
const MS_CASES: &[(&str, u32, &str)] = &[
    (
        "one triangle",
        1,
        r#"MS_ATTR [numthreads(32, 1, 1)]
        void ms(uint i : SV_GroupIndex, out vertices V verts[3], out indices uint3 tris[1]) {
            SetMeshOutputCounts(3, 1);
            if (i < 3) {
                verts[i].pos = float4(i == 1 ? 3 : -1, i == 2 ? 3 : -1, 0.5, 1);
                verts[i].col = float4(k.x * i, k.y, k.z * (i == 2), 1);
                verts[i].uv = float2(i * 0.5, k.w);
            }
            if (i == 0) tris[0] = uint3(0, 1, 2);
        }"#,
    ),
    // A quad per group in its quadrant; a run-time vertex count, and a
    // primitive count per group whose third triangle is culled.
    (
        "groups, counts and culling",
        4,
        r#"struct P { bool cull : SV_CullPrimitive; };
        MS_ATTR [numthreads(64, 1, 1)]
        void ms(uint i : SV_GroupIndex, uint3 g : SV_GroupID, out vertices V verts[8],
                out indices uint3 tris[4], out primitives P prims[4]) {
            uint nv = 4 + (k.z > 1);
            uint np = 2 + (g.x & 1);
            SetMeshOutputCounts(nv, np);
            float2 o = float2(g.x & 1, g.x >> 1) - 1;
            if (i < nv) {
                float2 p = i < 4 ? float2(i & 1, i >> 1) : float2(-0.75, 0.5);
                verts[i].pos = float4(o + p, 0.5, 1);
                verts[i].col = float4(k.x * i, g.x * 0.25, k.z, 1);
                verts[i].uv = float2(i, g.x);
            }
            if (i < np) {
                tris[i] = i == 0 ? uint3(0, 1, 2) : i == 1 ? uint3(1, 3, 2) : uint3(1, 4, 3);
                prims[i].cull = i == 2;
            }
        }"#,
    ),
    // Two waves per group, as the title's mesh programs run: the
    // second wave's lanes are threads 64-127.
    (
        "two waves",
        1,
        r#"MS_ATTR [numthreads(128, 1, 1)]
        void ms(uint i : SV_GroupIndex, out vertices V verts[81], out indices uint3 tris[128]) {
            SetMeshOutputCounts(81, 128);
            if (i < 81) {
                float2 p = float2(i % 9, i / 9) / 8.0;
                verts[i].pos = float4(p * 2 - 1, 0.5, 1);
                verts[i].col = float4(p.x * k.x, p.y, (i & 7) * 0.125, 1);
                verts[i].uv = float2(i, k.y);
            }
            uint c = i / 2 % 8 + i / 16 * 9;
            tris[i] = i & 1 ? uint3(c + 1, c + 10, c + 9) : uint3(c, c + 1, c + 9);
        }"#,
    ),
    // Quad reads (DPP quad permutations on the console; the title's
    // tessellating mesh programs swizzle within quads too).
    (
        "quad reads",
        1,
        r#"MS_ATTR [numthreads(128, 1, 1)]
        void ms(uint i : SV_GroupIndex, out vertices V verts[81], out indices uint3 tris[128]) {
            SetMeshOutputCounts(81, 128);
            float w = i * k.x + k.y;
            float a = QuadReadLaneAt(w, 0);
            float b = QuadReadLaneAt(w, 2);
            float c = QuadReadAcrossX(w);
            if (i < 81) {
                float2 p = float2(i % 9, i / 9) / 8.0;
                verts[i].pos = float4(p * 2 - 1, 0.5, 1);
                verts[i].col = float4(a, b, c, 1);
                verts[i].uv = float2(i, k.y);
            }
            uint cc = i / 2 % 8 + i / 16 * 9;
            tris[i] = i & 1 ? uint3(cc + 1, cc + 10, cc + 9) : uint3(cc, cc + 1, cc + 9);
        }"#,
    ),
    // Vertices and indices written by other threads than their lanes: the
    // console passes them through shared memory, with a loop and barriers.
    (
        "grid through shared memory",
        2,
        r#"MS_ATTR [numthreads(32, 1, 1)]
        void ms(uint i : SV_GroupIndex, uint3 g : SV_GroupID, out vertices V verts[36],
                out indices uint3 tris[64]) {
            SetMeshOutputCounts(36, 50);
            for (uint v = i; v < 36; v += 32) {
                float2 p = float2(v % 6, v / 6) / 5.0;
                verts[v].pos = float4(p.x - g.x, p.y * 2 - 1, 0.5, 1);
                verts[v].col = float4(p * k.x, k.z * g.x, 1);
                verts[v].uv = float2(v, k.y);
            }
            if (i < 25) {
                uint c = i % 5 + i / 5 * 6;
                tris[2 * i] = uint3(c, c + 1, c + 6);
                tris[2 * i + 1] = uint3(c + 1, c + 7, c + 6);
            }
        }"#,
    ),
];

#[test]
fn translated_mesh_shaders_draw_as_pc_ones() {
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
    let dir = std::env::temp_dir().join(format!("victory-diff-ms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let k = [0.75f32, -1.5, 2.0, 0.125].map(f32::to_bits);
    let draw = |roots, mesh_groups| Draw {
        roots,
        textures: &[],
        samplers: &[],
        vertices: &[],
        stride: 0,
        size: (16, 16),
        mesh_groups,
    };
    let mut failures = Vec::new();
    let only = std::env::var("OATHKEEP_DIFF_ONLY").ok();
    for &(name, groups, src) in MS_CASES
        .iter()
        .filter(|c| only.as_ref().is_none_or(|o| o == c.0))
    {
        let console_ms = compile(&console, &dir, src, "ms", "ms_6_5");
        let ms_c = match translate(&console_ms) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{name}: {e}"));
                continue;
            }
        };
        let ps_c = translate(&compile(&console, &dir, src, "mps", "ps_6_5")).unwrap();
        if let Ok(out) = std::env::var("OATHKEEP_EMIT_DUMP") {
            let stem = name.replace([' ', ','], "_");
            std::fs::write(format!("{out}/{stem}.ms.cdxil"), &console_ms).unwrap();
            std::fs::write(format!("{out}/{stem}.ms.dxil"), &ms_c.dxil).unwrap();
        }
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut ms_c.dxil.clone())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let (root_t, pso_t) = host.mesh_pipeline(&ms_c, &ps_c).unwrap();
        let ours = host
            .draw(
                &root_t,
                &pso_t,
                &draw(
                    vec![
                        (0, Root::Constants(user_data(&ms_c, &k))),
                        (1, Root::Constants(user_data(&ps_c, &k))),
                    ],
                    groups,
                ),
            )
            .unwrap();
        let (ms_p, ps_p) = (
            compile(&pc, &dir, src, "ms", "ms_6_5"),
            compile(&pc, &dir, src, "mps", "ps_6_5"),
        );
        // SAFETY: the PC container carries its root signature.
        let root_p: ID3D12RootSignature =
            unsafe { host.device.CreateRootSignature(0, &ms_p) }.unwrap();
        let pso_p = host.mesh_pipeline_with(&root_p, &ms_p, &ps_p).unwrap();
        let theirs = host
            .draw(
                &root_p,
                &pso_p,
                &draw(vec![(0, Root::Constants(k.to_vec()))], groups),
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
        let drawn = theirs.iter().filter(|t| t[3] != 0.0).count();
        eprintln!(
            "{name}: {drawn} texels drawn; worst relative difference {:.2e} (at {}: {} vs {})",
            worst.0, worst.1, worst.2, worst.3
        );
        if worst.0.is_nan() || worst.0 > 1e-3 || drawn == 0 {
            // Which texels each drew (alpha set), row by row.
            for (o, t) in ours.chunks(16).zip(theirs.chunks(16)) {
                let row = |r: &[[f32; 4]]| -> String {
                    r.iter()
                        .map(|x| if x[3] != 0.0 { '#' } else { '.' })
                        .collect()
                };
                eprintln!("  {}  {}", row(o), row(t));
            }
            failures.push(format!("{name}: {worst:?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Where PC code and the console differ by design, the translation must
/// give the console's results: float-to-int conversions saturate and give
/// 0 for NaN (`v_cvt_i32/u32_f32`), the legacy multiply the compiler
/// uses for `*` gives 0 for 0 × NaN, `frac` never reaches 1.0 (it gives
/// the largest value below; some hosts' own `frac` gives 1.0), and sine
/// and cosine take turns: the sine of a half turn and the cosine of a
/// quarter turn are exactly +0 (the host's functions of the rounded angle
/// give about 1e-7). The compiler takes whole turns off itself, so large
/// angles cannot be tried from here.
#[test]
fn float_semantics_are_the_consoles() {
    use super::super::host::{Dispatch, View};
    let Some((console, _)) = tools() else {
        eprintln!("no console compiler; skipped");
        return;
    };
    let host = match Host::new(false) {
        Ok(host) => host,
        Err(e) => {
            eprintln!("no SM 6.6 device ({e}); skipped");
            return;
        }
    };
    let dir = std::env::temp_dir().join(format!("victory-float-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = r#"[RootSignature(RS)] [numthreads(4, 1, 1)]
        void cs(uint i : SV_GroupIndex) {
            float x = i == 0 ? k.x : i == 1 ? -k.x : i == 2 ? k.y : k.z;
            outb.Store(4 * i, asuint(int(x)));
            outb.Store(16 + 4 * i, uint(x));
            outb.Store(32 + 4 * i, asuint(x * k.w));
            float y = i == 0 ? k.w - 1e-10 : i == 1 ? k.z : i == 2 ? k.w - 1e-8 : k.z + 0.25;
            outb.Store(48 + 4 * i, asuint(frac(y)));
            float r = i == 0 ? k.w + 3.14159274 : i == 1 ? k.w + 1.0
                    : i == 2 ? k.w - 3.14159274 : k.w + 1.57079637;
            outb.Store(64 + 4 * i, asuint(i == 3 ? cos(r) : sin(r)));
        }"#;
    let cs = translate(&compile(&console, &dir, src, "cs", "cs_6_5")).unwrap();
    let (root, pso) = host.compute_pipeline(&cs).unwrap();
    // 1e30, NaN, 3.5, 0.
    let k = [1e30f32.to_bits(), 0x7FC0_0000, 3.5f32.to_bits(), 0];
    let views = [
        View::Texture(1, 1, &[[0.0; 4]]),
        View::Texture(1, 1, &[[0.0; 4]]),
        View::Texture(1, 1, &[[0.0; 4]]),
        View::Texture(1, 1, &[[0.0; 4]]),
        View::Texture(1, 1, &[[0.0; 4]]),
        View::RawUav(20),
        View::TextureUav(1, 1),
    ];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(user_data(&cs, &k)))],
                views: &views,
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        out[0][..16],
        [
            0x7FFF_FFFF,
            0x8000_0000,
            0,
            3, // int
            u32::MAX,
            0,
            0,
            3, // uint
            0,
            0,
            0,
            0, // x * 0, NaN included
            0x3F7F_FFFF,
            0.5f32.to_bits(),
            0x3F7F_FFFF,
            0.75f32.to_bits(), // frac
        ]
    );
    // sin(pi), sin(-pi) and cos(pi/2): the compiler turns radians into
    // exactly 0.5, -0.5 and 0.25 turns.
    assert_eq!([out[0][16], out[0][18], out[0][19]], [0, 0, 0]);
    // An ordinary angle is untouched: sin(1 rad).
    let one = f32::from_bits(out[0][17]);
    assert!((one - 0.841_470_96).abs() < 1e-4, "{one}");
}

