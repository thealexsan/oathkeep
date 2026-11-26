use super::super::create::create_committed;
use super::super::host_tex::host_resource;
use super::super::twin::{PLANES, twin_plane_resource};
use super::{PROPERTIES, RESOURCE_IIDS, ResourceDesc, texture};
use crate::d3d12::object::S_OK;

#[test]
fn inline_depth_plane_twin_sync_runs_before_list_close() {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
    use windows::core::s;
    let Some(host) = crate::backend::host() else {
        return;
    };
    let null = std::ptr::null();
    const DEPTH_WRITE: u32 = 0x10;
    let desc = ResourceDesc { flags: 6, ..texture(4, 4, 118, 1) };
    let mut depth = null;
    assert_eq!(create_committed(null, &PROPERTIES, 0, &desc, DEPTH_WRITE, null.cast(), &RESOURCE_IIDS[0], &mut depth), S_OK);
    let host_depth = host_resource(depth).unwrap();
    let Some(plane) = twin_plane_resource(depth) else {
        return;
    };

    const CLASSES: [u16; 16] = [8, 65, 128, 129, 65, 8, 129, 128, 0, 255, 65, 64, 129, 130, 8, 65];
    let mut pattern = vec![0u8; 1024];
    for (i, class) in CLASSES.into_iter().enumerate() {
        let at = i / 4 * 256 + i % 4 * 2;
        pattern[at..at + 2].copy_from_slice(&class.to_le_bytes());
    }
    let (upload, data) = host.upload_buffer(1024).unwrap();
    // SAFETY: mapped upload buffer holds 1024 bytes.
    unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024) };

    let readback = {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain readback buffer creation.
        unsafe {
            host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut made).unwrap();
        }
        made.unwrap()
    };
    let target = {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT(61),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain texture creation.
        unsafe {
            host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut made).unwrap();
        }
        made.unwrap()
    };
    let rows_of = |format: i32| D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT(format), Width: 4, Height: 4, Depth: 1, RowPitch: 256 },
    };
    let whole = |texture: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: borrowed COM pointer not released by the struct.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    };
    let rows = |buffer: &ID3D12Resource, placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: placed },
    };
    let moved = crate::backend::transition;

    let fill = host.new_list().unwrap();
    // SAFETY: open list and live resources.
    unsafe {
        let l = &fill.list;
        l.ResourceBarrier(&[moved(plane, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_DEST)]);
        l.CopyTextureRegion(&whole(plane), 0, 0, 0, &rows(&upload, rows_of(53)), None);
        l.ResourceBarrier(&[moved(plane, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
    }
    fill.close();
    host.execute(&[&fill]);

    const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants = 1, b0)"
cbuffer Drawn : register(b0) { uint number; };
[RootSignature(RS)]
float4 corner(uint i : SV_VertexID) : SV_Position {
    float z = float(number) * asfloat(0x37800080);
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, z, 1.0);
}
float4 lit() : SV_Target { return 1.0; }
"#;
    let vs = crate::backend::compile(SOURCE, s!("corner"), s!("vs_5_1")).unwrap();
    let ps = crate::backend::compile(SOURCE, s!("lit"), s!("ps_5_1")).unwrap();
    let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() };
    let mut blend = D3D12_BLEND_DESC::default();
    blend.RenderTarget[0].RenderTargetWriteMask = 0xf;
    let mut formats = [DXGI_FORMAT(0); 8];
    formats[0] = DXGI_FORMAT(61);
    // SAFETY: plain PSO creation.
    let (signature, pipeline) = unsafe {
        let signature: ID3D12RootSignature = host.device.CreateRootSignature(0, &vs[..]).unwrap();
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&signature),
            VS: code(&vs[..]),
            PS: code(&ps[..]),
            BlendState: blend,
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                DepthFunc: D3D12_COMPARISON_FUNC_EQUAL,
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: formats,
            DSVFormat: DXGI_FORMAT(45),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let pipeline: ID3D12PipelineState = host.device.CreateGraphicsPipelineState(&desc).unwrap();
        (signature, pipeline)
    };

    let late_before = PLANES[2].load(Relaxed);
    let list = host.new_list().unwrap();
    assert!(list.twin_use(depth, true, true));
    list.twins_written();
    assert!(!list.take_state_clobbered());
    assert!(list.twin_use(depth, false, false));
    assert!(list.take_state_clobbered(), "inline depth-plane pass must report clobbered command-list state");
    assert!(!list.take_state_clobbered(), "clobbered flag must clear on read");
    assert_eq!(PLANES[2].load(Relaxed), late_before, "inline pass must not defer to list close");

    let colour = host.rtv(Some(&target), None);
    let depth_view = host.dsv(
        host_depth,
        Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
            Format: DXGI_FORMAT(45),
            ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
            ..Default::default()
        }),
    );
    // SAFETY: open list, live resources; `pass_plane` left `host_depth` in `DEPTH_WRITE`.
    unsafe {
        let l = &list.list;
        l.ClearRenderTargetView(colour, &[0.0; 4], None);
        l.SetGraphicsRootSignature(&signature);
        l.SetPipelineState(&pipeline);
        l.OMSetRenderTargets(1, Some(&colour), false, Some(&depth_view));
        l.RSSetViewports(&[D3D12_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: 4.0, Height: 4.0, MinDepth: 0.0, MaxDepth: 1.0 }]);
        l.RSSetScissorRects(&[RECT { left: 0, top: 0, right: 4, bottom: 4 }]);
        l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(4));
        for class in [65u32, 129] {
            l.SetGraphicsRoot32BitConstant(0, class, 0);
            l.DrawInstanced(3, 1, 0, 0);
        }
        l.ResourceBarrier(&[moved(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        l.CopyTextureRegion(&rows(&readback, rows_of(61)), 0, 0, 0, &whole(&target), None);
    }
    list.close();
    host.execute(&[&list]);

    let mut mapped = std::ptr::null_mut();
    // SAFETY: executed list finished; buffer holds 1024 bytes.
    let read = unsafe {
        readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 1024 }), Some(&mut mapped)).unwrap();
        std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
    };
    for (i, class) in CLASSES.into_iter().enumerate() {
        let covered = read[i / 4 * 256 + i % 4] != 0;
        let drawn = matches!(class, 65 | 129);
        assert_eq!(covered, drawn, "inline pass texel {i}, class {class}");
    }
}
