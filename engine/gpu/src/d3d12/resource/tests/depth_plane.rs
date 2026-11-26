use super::super::create::create_committed;
use super::super::host_tex::{host_copy, host_resource};
use super::super::twin::twin_plane_resource;
use super::{PROPERTIES, RESOURCE_IIDS, ResourceDesc, texture};
use crate::d3d12::object::S_OK;

#[test]
fn a_depth_planes_twin_is_drawn_into_the_plane() {
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
    // SAFETY: live resources.
    let (d, t) = unsafe { (host_depth.GetDesc(), plane.GetDesc()) };
    assert_eq!((d.Format.0, d.Flags.0), (44, 2));
    assert_eq!((t.Format.0, t.Flags.0), (53, 4));

    const CLASSES: [u16; 16] = [8, 65, 128, 129, 65, 8, 129, 128, 0, 255, 65, 64, 129, 130, 8, 65];
    let mut pattern = vec![0u8; 1024];
    for (i, class) in CLASSES.into_iter().enumerate() {
        let at = i / 4 * 256 + i % 4 * 2;
        pattern[at..at + 2].copy_from_slice(&class.to_le_bytes());
    }
    let (upload, data) = host.upload_buffer(1024).unwrap();
    // SAFETY: the buffer's mapping, 1024 bytes.
    unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024) };
    let buffer = |kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES| {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
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
        // SAFETY: plain creation of a buffer.
        unsafe {
            host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made).unwrap();
        }
        made.unwrap()
    };
    let readback = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
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
        // SAFETY: plain creation of a texture.
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
        // SAFETY: a borrowed pointer the location never releases.
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
    let (written, source, dest, tested, drawn) = (
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
    let moved = crate::backend::transition;

    let fill = host.new_list().unwrap();
    // SAFETY: an open list and live resources.
    unsafe {
        let l = &fill.list;
        l.ResourceBarrier(&[moved(plane, written, dest)]);
        l.CopyTextureRegion(&whole(plane), 0, 0, 0, &rows(&upload, rows_of(53)), None);
        l.ResourceBarrier(&[moved(plane, dest, written)]);
    }
    fill.close();
    host.execute(&[&fill]);

    let list = host.new_list().unwrap();
    assert!(list.twin_use(depth, true, true));
    list.twins_written();
    list.close();
    host.execute(&[&list]);

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
    // SAFETY: plain creation; the description points at live data.
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
    let state = host_copy(depth).unwrap().state.load(Relaxed);
    let state = D3D12_RESOURCE_STATES(state as i32);
    let colour = host.rtv(Some(&target), None);
    let depth_view = host.dsv(
        host_depth,
        Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
            Format: DXGI_FORMAT(45),
            ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
            ..Default::default()
        }),
    );
    let back = host.new_list().unwrap();
    // SAFETY: an open list and live resources and views.
    unsafe {
        let l = &back.list;
        if state != tested {
            l.ResourceBarrier(&[moved(host_depth, state, tested)]);
        }
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
        l.ResourceBarrier(&[moved(&target, drawn, source)]);
        let (to, from) = (rows(&readback, rows_of(61)), whole(&target));
        l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
    }
    back.close();
    host.execute(&[&back]);
    let mut mapped = std::ptr::null_mut();
    // SAFETY: the copy finished (execute waits); the buffer is 1024 bytes.
    let read = unsafe {
        readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 1024 }), Some(&mut mapped)).unwrap();
        std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
    };
    for (i, class) in CLASSES.into_iter().enumerate() {
        let covered = read[i / 4 * 256 + i % 4] != 0;
        let drawn = matches!(class, 65 | 129);
        assert_eq!(covered, drawn, "texel {i}, class {class}");
    }
}
