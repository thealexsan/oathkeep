use std::sync::atomic::Ordering;

use windows::Win32::Graphics::Dxgi::Common::*;

use super::tests::{PS, code, compile, dxc};
use super::*;

const VS: &str = "float4 main(uint i : SV_VertexID) : SV_Position {
    float2 p = float2((i << 1) & 2, i & 2);
    return float4(p * 2.0 - 1.0, 0.5, 1.0);
}";

/// A graphics pipeline with `DepthBoundsTestEnable` loads back from the
/// pipeline library and culls or passes fragments according to
/// `OMSetDepthBounds` (including across a command-list cut).
#[test]
fn depth_bounds_pipeline_loads_and_filters_draws() {
    let Some(host) = host() else {
        return;
    };
    if !host.depth_bounds() {
        eprintln!("no host depth bounds support; skipped");
        return;
    }
    let Some(dxc) = dxc() else {
        eprintln!("no PC dxc.exe; skipped");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let vs = compile(&dxc, dir.path(), VS, "vs_6_5");
    let ps = compile(&dxc, dir.path(), PS, "ps_6_5");
    let (root, root_blob) = root_signature(&host.device, &[], &[], false).unwrap();

    let mut g = GraphicsState::default();
    // Subobject 21 (`DEPTH_STENCIL1`) with `DepthBoundsTestEnable = true`,
    // depth test itself off so only depth bounds gates the draw.
    #[repr(C, align(8))]
    struct Sub21(u32, D3D12_DEPTH_STENCIL_DESC1);
    let sub = Sub21(
        21,
        D3D12_DEPTH_STENCIL_DESC1 {
            DepthEnable: false.into(),
            DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
            DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            DepthBoundsTestEnable: true.into(),
            ..Default::default()
        },
    );
    // SAFETY: `sub` is a complete subobject 21.
    unsafe { g.read(21, (&raw const sub).cast()) };
    assert!(g.depth_bounds);
    assert!(g.describe().contains("depth-bounds"));
    g.rasterizer.CullMode = D3D12_CULL_MODE_NONE;
    g.targets.NumRenderTargets = 1;
    g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    g.depth_format = DXGI_FORMAT_D32_FLOAT;

    let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        // SAFETY: borrowed pointer, never released by the descriptor.
        pRootSignature: unsafe { std::mem::transmute_copy(&root) },
        VS: code(&vs),
        PS: code(&ps),
        BlendState: g.blend,
        SampleMask: g.sample_mask,
        RasterizerState: g.rasterizer,
        DepthStencilState: g.depth,
        IBStripCutValue: g.strip_cut,
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        RTVFormats: g.targets.RTFormats,
        DSVFormat: g.depth_format,
        SampleDesc: g.samples,
        ..Default::default()
    };
    let name = super::super::library::name(
        "graphics depth-bounds test",
        &root_blob,
        &[&vs, &ps],
        &g.describe(),
    );
    let pso = graphics_depth_bounds_pipeline(&host.device, &root, &desc, g.depth1(), &name)
        .expect("depth-bounds PSO made");
    let loaded = super::super::library::LOADED.load(Ordering::Relaxed);
    graphics_depth_bounds_pipeline(&host.device, &root, &desc, g.depth1(), &name)
        .expect("depth-bounds PSO made again");
    assert!(
        super::super::library::LOADED.load(Ordering::Relaxed) > loaded,
        "the depth-bounds pipeline was compiled again instead of loaded from the library"
    );

    // Execute two draws against a depth buffer cleared to 0.5:
    // 1) bounds [0.6, 1.0] -> rejected (target stays 0)
    // 2) bounds [0.4, 0.6] replayed onto a fresh HostList -> passes (target becomes 255)
    let make_tex = |format, flags, state, clear: D3D12_CLEAR_VALUE| {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let rd = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: flags,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: valid heap and resource descriptions.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &rd, state, Some(&clear), &mut out)
                .unwrap();
        }
        out.unwrap()
    };
    let color = make_tex(
        DXGI_FORMAT_R8G8B8A8_UNORM,
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_CLEAR_VALUE {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            Anonymous: D3D12_CLEAR_VALUE_0 { Color: [0.0; 4] },
        },
    );
    let depth = make_tex(
        DXGI_FORMAT_D32_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_CLEAR_VALUE {
            Format: DXGI_FORMAT_D32_FLOAT,
            Anonymous: D3D12_CLEAR_VALUE_0 {
                DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                    Depth: 0.5,
                    Stencil: 0,
                },
            },
        },
    );
    let readback = {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let rd = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: valid readback buffer description.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &rd,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut out,
                )
                .unwrap();
        }
        out.unwrap()
    };

    let run_draw = |min: f32, max: f32| -> u8 {
        let mut state = crate::draw::State::default();
        let first_list = host.new_list().unwrap();
        state.set_depth_bounds(&first_list, min, max);
        first_list.close();
        // Replay onto a second host list, exercising `State::replay`.
        let list = host.new_list().unwrap();
        state.replay(&list);
        let rtv = host.rtv(Some(&color), None);
        let dsv = host.dsv(&depth, None);
        let vp = D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: 4.0,
            Height: 4.0,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let rect = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        };
        // SAFETY: open command list, live resources and views.
        unsafe {
            list.list.ClearRenderTargetView(rtv, &[0.0; 4], None);
            list.list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 0.5, 0, None);
            list.list.OMSetRenderTargets(1, Some(&rtv), false, Some(&dsv));
            list.list.RSSetViewports(&[vp]);
            list.list.RSSetScissorRects(&[rect]);
            list.list.SetGraphicsRootSignature(&root);
            list.list.SetPipelineState(&pso);
            list.list
                .IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.list.DrawInstanced(3, 1, 0, 0);
            list.list.ResourceBarrier(&[crate::host::transition(
                &color,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: 4,
                            Height: 4,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&color),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            list.list.ResourceBarrier(&[crate::host::transition(
                &color,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            )]);
        }
        list.close();
        host.execute(&[&list]);
        let mut ptr = std::ptr::null_mut();
        // SAFETY: `execute` waited for the GPU; readback holds 1024 bytes.
        unsafe {
            readback.Map(0, None, Some(&mut ptr)).unwrap();
            let pixel = *ptr.cast::<u8>();
            readback.Unmap(0, None);
            pixel
        }
    };

    assert_eq!(run_draw(0.6, 1.0), 0, "depth 0.5 outside [0.6, 1.0] must be rejected");
    assert_eq!(run_draw(0.4, 0.6), 255, "depth 0.5 inside [0.4, 0.6] must pass");
}
