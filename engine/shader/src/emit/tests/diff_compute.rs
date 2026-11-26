//! Differential tests: HLSL compiled by the console's shader compiler and
//! translated, against the same HLSL compiled for PC, both drawn (or
//! dispatched) on the host GPU with the same inputs.
//!
//! Needs `OATHKEEP_CONSOLE_DXC` (the console SDK's `dxc.exe`) and a PC
//! `dxc.exe` (`OATHKEEP_PC_DXC`, else the newest Windows SDK's); skipped
//! without them. Set `OATHKEEP_WARP=1` to draw on WARP.


use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::super::host::{Host, Root};
use super::super::translate;


use super::diff_compute_cases::CS_CASES;
use super::diff_gfx::{compile, tools, user_data};

/// Equal bits, both zero (the console's legacy multiply gives +0 where PC
/// code gives -0), or two normal floats within 1e-3 relative.
fn close(a: u32, b: u32) -> bool {
    let (x, y) = (f32::from_bits(a), f32::from_bits(b));
    a == b
        || x == 0.0 && y == 0.0
        || x.is_normal() && y.is_normal() && (x - y).abs() <= 1e-3 * y.abs().max(1.0)
}

#[test]
fn translated_compute_matches_pc() {
    use super::super::host::{Dispatch, Stride, View};
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
    let dir = std::env::temp_dir().join(format!("victory-diff-cs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let k = [0.75f32, -1.5, 2.0, 0.125].map(f32::to_bits);
    let texels: Vec<[f32; 4]> = (0..16)
        .map(|i| [i as f32 / 16.0, (i % 4) as f32 * 0.3, 0.5, 1.0])
        .collect();
    let dummy = [[9.0f32; 4]];
    let cube_texels: Vec<[f32; 4]> = (0..96)
        .map(|i| {
            [
                (i / 16) as f32,
                (i % 4) as f32 * 0.25,
                (i / 4 % 4) as f32 * 0.25,
                1.0,
            ]
        })
        .collect();
    // Buffers: 16 float4, 8 structures of 16 bytes, 32 raw dwords; the
    // root CBV's 16 float4 and the root SRV's 32 dwords.
    let floats = |n: usize, scale: f32| -> Vec<u32> {
        (0..n).map(|i| (i as f32 * scale - 3.0).to_bits()).collect()
    };
    let typed_data = floats(64, 0.25);
    let structured_data: Vec<u32> = (0..32u32)
        .map(|i| {
            if i % 4 == 1 {
                i * 0x0101_0101
            } else {
                (i as f32 * 0.5).to_bits()
            }
        })
        .collect();
    let raw_data: Vec<u32> = (0..32u32).map(|i| i.wrapping_mul(0x9E37_79B9)).collect();
    let views = [
        View::Texture(1, 1, &dummy),
        View::Texture(4, 4, &texels),
        View::Buffer {
            data: &typed_data,
            stride: Stride::Typed,
        },
        View::Buffer {
            data: &structured_data,
            stride: Stride::Structured(16),
        },
        View::Buffer {
            data: &raw_data,
            stride: Stride::Raw,
        },
        View::RawUav(1024),
        View::TextureUav(16, 16),
        View::UintTextureUav(16, 16),
        View::HalfUav(256),
    ];
    let typed_view = |format, bytes| View::Buffer {
        data: &raw_data,
        stride: Stride::Format(format, bytes),
    };
    let views: Vec<View> = views
        .into_iter()
        .chain([
            typed_view(DXGI_FORMAT_R8G8B8A8_UNORM, 4),
            typed_view(DXGI_FORMAT_R16G16_SNORM, 4),
            typed_view(DXGI_FORMAT_R8G8B8A8_UINT, 4),
            typed_view(DXGI_FORMAT_R10G10B10A2_UNORM, 4),
            typed_view(DXGI_FORMAT_R16G16B16A16_UINT, 8),
            typed_view(DXGI_FORMAT_R8_UNORM, 1),
            typed_view(DXGI_FORMAT_R16_FLOAT, 2),
            typed_view(DXGI_FORMAT_R8G8B8A8_SNORM, 4),
            View::Cube(4, 1, &cube_texels),
        ])
        .collect();
    let root_cbv = floats(64, -0.125);
    let root_srv: Vec<u32> = (0..32u32).map(|i| i * 0x0001_0003 + 7).collect();
    let samplers = [D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        MaxLOD: f32::MAX,
        ..Default::default()
    }; 2];
    let dispatch = |roots| Dispatch {
        roots,
        views: &views,
        samplers: &samplers,
        groups: [2, 2, 1],
    };
    let mut failures = Vec::new();
    let only = std::env::var("OATHKEEP_DIFF_ONLY").ok();
    let warp = std::env::var("OATHKEEP_WARP").is_ok_and(|v| v == "1");
    for (name, src) in CS_CASES
        .iter()
        .filter(|c| only.as_ref().is_none_or(|o| o == c.0))
        .filter(|c| warp || !c.0.starts_with("terminates"))
    {
        let console_cs = compile(&console, &dir, src, "cs", "cs_6_5");
        if let Ok(out) = std::env::var("OATHKEEP_EMIT_DUMP") {
            let stem = name.replace([' ', ','], "_");
            std::fs::write(format!("{out}/{stem}.cs.cdxil"), &console_cs).unwrap();
        }
        let cs_c = match translate(&console_cs) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{name}: {e}"));
                continue;
            }
        };
        if let Ok(out) = std::env::var("OATHKEEP_EMIT_DUMP") {
            let stem = name.replace([' ', ','], "_");
            std::fs::write(format!("{out}/{stem}.cs.dxil"), &cs_c.dxil).unwrap();
        }
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut cs_c.dxil.clone())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let (root_t, pso_t) = match host.compute_pipeline(&cs_c) {
            Ok(p) => p,
            // WARP runs no fixed wave size of 64.
            Err(e) if std::env::var("OATHKEEP_WARP").is_ok_and(|v| v == "1") => {
                eprintln!("{name}: skipped on WARP ({e})");
                continue;
            }
            Err(e) => panic!("{name}: {e}"),
        };
        // Our root views follow the user data in root parameter order.
        let mut roots = vec![(0, Root::Constants(user_data(&cs_c, &k)))];
        let mut root_views: Vec<(u8, bool)> = cs_c.root_cbvs.iter().map(|&s| (s, false)).collect();
        root_views.extend(cs_c.root_srvs.iter().map(|&s| (s, true)));
        root_views.sort_unstable();
        let view_strides: Vec<u32> = views
            .iter()
            .map(|v| match *v {
                View::RawUav(_) => 1,
                View::HalfUav(_) => 8,
                View::Buffer { stride, .. } => match stride {
                    Stride::Raw => 1,
                    Stride::Typed => 16,
                    Stride::Structured(s) => s,
                    Stride::Format(f, bytes) => {
                        let kind = match f.0 as u32 {
                            3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62 => {
                                super::super::VIEW_INTEGER
                            }
                            4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => {
                                super::super::VIEW_INTEGER | super::super::VIEW_SIGNED
                            }
                            _ => 0,
                        };
                        bytes | kind
                    }
                },
                _ => 0,
            })
            .collect();
        for (n, (slot, srv)) in root_views.into_iter().enumerate() {
            let data = if srv
                && (slot == super::super::SLOT_STRIDES || slot == super::super::SLOT_TABLE_STRIDES)
            {
                view_strides.clone()
            } else {
                assert_eq!(slot, 4 + u8::from(srv), "{name}: root view");
                if srv {
                    root_srv.clone()
                } else {
                    root_cbv.clone()
                }
            };
            roots.push((1 + n as u32, Root::View { srv, data }));
        }
        let ours = host.dispatch(&root_t, &pso_t, &dispatch(roots));
        if name.starts_with("terminates") {
            match ours {
                Ok(_) => eprintln!("{name}: finished"),
                Err(e) => failures.push(format!("{name}: {e}")),
            }
            continue;
        }
        let ours = ours.unwrap();
        let cs_p = compile(&pc, &dir, src, "cs", "cs_6_5");
        // SAFETY: the PC container carries its root signature.
        let root_p: ID3D12RootSignature =
            unsafe { host.device.CreateRootSignature(0, &cs_p) }.unwrap();
        let pso_p = host.compute_pipeline_with(&root_p, &cs_p).unwrap();
        let table = |sampler, index| Root::Table { sampler, index };
        let theirs = host
            .dispatch(
                &root_p,
                &pso_p,
                &dispatch(vec![
                    (0, Root::Constants(k.to_vec())),
                    (1, table(false, 1)),
                    (2, table(true, 1)),
                    (3, table(false, 5)),
                    (
                        4,
                        Root::View {
                            srv: false,
                            data: root_cbv.clone(),
                        },
                    ),
                    (
                        5,
                        Root::View {
                            srv: true,
                            data: root_srv.clone(),
                        },
                    ),
                    (6, table(false, 9)),
                ]),
            )
            .unwrap();
        let mismatch = ours
            .iter()
            .flatten()
            .zip(theirs.iter().flatten())
            .enumerate()
            .find(|(_, (a, b))| !close(**a, **b));
        match mismatch {
            None => eprintln!("{name}: equal"),
            Some((at, (a, b))) => failures.push(format!(
                "{name}: word {at}: {a:#x} ({}) vs {b:#x} ({})",
                f32::from_bits(*a),
                f32::from_bits(*b)
            )),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{failures:#?}");
}
