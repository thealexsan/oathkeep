use super::super::create::create_committed;
use super::super::host_tex::host_resource;
use super::super::twin::{PLANES, twin_plane_resource};
use super::{PROPERTIES, RESOURCE_IIDS, ResourceDesc, texture};
use crate::d3d12::object::S_OK;

#[test]
fn depth_plane_copies_back_to_twin_inline_and_in_prelude() {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
    let Some(host) = crate::backend::host() else {
        return;
    };
    let null = std::ptr::null();
    const DEPTH_WRITE: u32 = 0x10;
    let moved = crate::backend::transition;

    let make_readback = || {
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
    let read_plane = |plane: &ID3D12Resource, format: i32| -> Vec<u8> {
        let readback = make_readback();
        let whole = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed COM pointer not released by the struct.
            pResource: unsafe { std::mem::transmute_copy(plane) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        let rows = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&readback) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT(format), Width: 4, Height: 4, Depth: 1, RowPitch: 256 },
                },
            },
        };
        let back = host.new_list().unwrap();
        // SAFETY: open list and live resources; `plane` rests in `UNORDERED_ACCESS`.
        unsafe {
            let l = &back.list;
            l.ResourceBarrier(&[moved(plane, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            l.CopyTextureRegion(&rows, 0, 0, 0, &whole, None);
            l.ResourceBarrier(&[moved(plane, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: executed list finished; buffer holds 1024 bytes.
        unsafe {
            readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 1024 }), Some(&mut mapped)).unwrap();
            let bytes = std::slice::from_raw_parts(mapped.cast::<u8>(), 1024).to_vec();
            readback.Unmap(0, None);
            bytes
        }
    };

    // Case 1: 16-bit depth-stencil (format 118 -> R16 step twin).
    let desc16 = ResourceDesc { flags: 6, ..texture(4, 4, 118, 1) };
    let mut depth16 = null;
    assert_eq!(create_committed(null, &PROPERTIES, 0, &desc16, DEPTH_WRITE, null.cast(), &RESOURCE_IIDS[0], &mut depth16), S_OK);
    let host_depth16 = host_resource(depth16).unwrap();
    let Some(plane16) = twin_plane_resource(depth16) else {
        return;
    };
    let dsv16 = host.dsv(
        host_depth16,
        Some(&D3D12_DEPTH_STENCIL_VIEW_DESC { Format: DXGI_FORMAT(45), ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D, ..Default::default() }),
    );
    let missed_before = PLANES[1].load(Relaxed);

    let inline16 = host.new_list().unwrap();
    let _ = inline16.transition(depth16, DEPTH_WRITE);
    // SAFETY: open list and live DSV.
    unsafe {
        inline16.list.ClearDepthStencilView(dsv16, D3D12_CLEAR_FLAG_DEPTH, 129.0 / 65535.0, 0, None);
    }
    assert!(inline16.twin_use(depth16, false, true));
    assert!(inline16.twin_use(depth16, true, false));
    assert!(inline16.take_state_clobbered(), "inline depth-to-plane copy-back must flag clobbered state");
    inline16.close();
    host.execute(&[&inline16]);
    let bytes = read_plane(plane16, 53);
    for row in 0..4 {
        for col in 0..4 {
            let at = row * 256 + col * 2;
            let step = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            assert_eq!(step, 129, "inline 16-bit step at ({col},{row})");
        }
    }

    let writer16 = host.new_list().unwrap();
    let _ = writer16.transition(depth16, DEPTH_WRITE);
    // SAFETY: open list and live DSV.
    unsafe {
        writer16.list.ClearDepthStencilView(dsv16, D3D12_CLEAR_FLAG_DEPTH, 200.0 / 65535.0, 0, None);
    }
    assert!(writer16.twin_use(depth16, false, true));
    writer16.close();
    host.execute(&[&writer16]);

    let reader16 = host.new_list().unwrap();
    assert!(reader16.twin_use(depth16, true, false));
    assert!(!reader16.take_state_clobbered(), "cross-list sync runs in prelude");
    reader16.close();
    host.execute(&[&reader16]);
    let bytes = read_plane(plane16, 53);
    for row in 0..4 {
        for col in 0..4 {
            let at = row * 256 + col * 2;
            let step = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            assert_eq!(step, 200, "prelude 16-bit step at ({col},{row})");
        }
    }

    // Case 2: 24-bit depth-stencil (format 45 -> R32 float twin).
    let desc32 = ResourceDesc { flags: 6, ..texture(4, 4, 45, 1) };
    let mut depth32 = null;
    assert_eq!(create_committed(null, &PROPERTIES, 0, &desc32, DEPTH_WRITE, null.cast(), &RESOURCE_IIDS[0], &mut depth32), S_OK);
    let host_depth32 = host_resource(depth32).unwrap();
    let Some(plane32) = twin_plane_resource(depth32) else {
        return;
    };
    let dsv32 = host.dsv(
        host_depth32,
        Some(&D3D12_DEPTH_STENCIL_VIEW_DESC { Format: DXGI_FORMAT(45), ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D, ..Default::default() }),
    );

    let writer32 = host.new_list().unwrap();
    let _ = writer32.transition(depth32, DEPTH_WRITE);
    // SAFETY: open list and live DSV.
    unsafe {
        writer32.list.ClearDepthStencilView(dsv32, D3D12_CLEAR_FLAG_DEPTH, 0.375, 0, None);
    }
    assert!(writer32.twin_use(depth32, false, true));
    writer32.close();
    host.execute(&[&writer32]);

    let reader32 = host.new_list().unwrap();
    assert!(reader32.twin_use(depth32, true, false));
    reader32.close();
    host.execute(&[&reader32]);
    let bytes = read_plane(plane32, 39);
    for row in 0..4 {
        for col in 0..4 {
            let at = row * 256 + col * 4;
            let val = f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            assert!((val - 0.375).abs() < 1e-5, "prelude 32-bit float at ({col},{row}): got {val}");
        }
    }
    assert_eq!(PLANES[1].load(Relaxed), missed_before, "depth-to-plane copy-back must not drop writes back to plane twins");
}
