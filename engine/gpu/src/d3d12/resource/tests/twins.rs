use super::super::create::create_committed;
use super::super::host_tex::{host_copy, host_resource};
use super::super::twin::twin_resource;
use super::{PROPERTIES, RESOURCE_IIDS, ResourceDesc, texture};
use crate::d3d12::object::S_OK;

#[test]
fn a_depth_texture_and_its_twin_copy_into_each_other() {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
    let Some(host) = crate::backend::host() else {
        return;
    };
    let null = std::ptr::null();
    const DEPTH_WRITE: u32 = 0x10;
    let desc = ResourceDesc { flags: 6, ..texture(4, 4, 55, 1) };
    let mut depth = null;
    assert_eq!(create_committed(null, &PROPERTIES, 0, &desc, DEPTH_WRITE, null.cast(), &RESOURCE_IIDS[0], &mut depth), S_OK);
    let host_depth = host_resource(depth).unwrap();
    let Some(twin) = twin_resource(depth) else {
        return;
    };
    // SAFETY: live resources.
    let (d, t) = unsafe { (host_depth.GetDesc(), twin.GetDesc()) };
    assert_eq!((d.Format.0, d.Flags.0), (53, 2));
    assert_eq!((t.Format.0, t.Flags.0), (53, 4));

    let mut pattern = vec![0u8; 1024];
    for row in 0..4 {
        for b in 0..8 {
            pattern[row * 256 + b] = (1 + row * 8 + b) as u8;
        }
    }
    let (upload, data) = host.upload_buffer(2048).unwrap();
    // SAFETY: the buffer's mapping, 2048 bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024);
        std::ptr::write_bytes(data.add(1024), 0, 1024);
    }
    let readback_heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let readback_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: 1024,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut readback: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a readback buffer.
    unsafe {
        host.device
            .CreateCommittedResource(&readback_heap, D3D12_HEAP_FLAG_NONE, &readback_desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut readback)
            .unwrap();
    }
    let readback = readback.unwrap();

    let whole = |texture: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    };
    let rows = |buffer: &ID3D12Resource, offset: u64| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: offset,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT(53), Width: 4, Height: 4, Depth: 1, RowPitch: 256 },
            },
        },
    };
    let (written, tested, source, dest) = (
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    let moved = crate::backend::transition;
    let list = host.new_list().unwrap();
    // SAFETY: an open list and live resources.
    unsafe {
        let l = &list.list;
        l.ResourceBarrier(&[moved(twin, written, dest), moved(host_depth, tested, dest)]);
        l.CopyTextureRegion(&whole(twin), 0, 0, 0, &rows(&upload, 0), None);
        l.ResourceBarrier(&[moved(twin, dest, source)]);
        l.CopyResource(host_depth, twin);
        l.ResourceBarrier(&[moved(twin, source, dest)]);
        l.CopyTextureRegion(&whole(twin), 0, 0, 0, &rows(&upload, 1024), None);
        l.ResourceBarrier(&[moved(host_depth, dest, source)]);
        l.CopyResource(twin, host_depth);
        l.ResourceBarrier(&[moved(twin, dest, source)]);
        l.CopyTextureRegion(&rows(&readback, 0), 0, 0, 0, &whole(twin), None);
        l.ResourceBarrier(&[moved(twin, source, written), moved(host_depth, source, tested)]);
    }
    list.close();
    host.execute(&[&list]);
    let mut mapped = std::ptr::null_mut();
    // SAFETY: the copies finished (execute waits); the buffer is 1024 bytes.
    let read = unsafe {
        readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 1024 }), Some(&mut mapped)).unwrap();
        std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
    };
    for row in 0..4 {
        let at = row * 256;
        assert_eq!(read[at..at + 8], pattern[at..at + 8], "row {row}");
    }
}

#[test]
fn a_stencil_planes_twin_reaches_the_plane() {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
    let Some(host) = crate::backend::host() else {
        return;
    };
    let null = std::ptr::null();
    const DEPTH_WRITE: u32 = 0x10;
    let desc = ResourceDesc { flags: 6, ..texture(4, 4, 45, 1) };
    let mut depth = null;
    assert_eq!(create_committed(null, &PROPERTIES, 0, &desc, DEPTH_WRITE, null.cast(), &RESOURCE_IIDS[0], &mut depth), S_OK);
    let host_depth = host_resource(depth).unwrap();
    let Some(twin) = twin_resource(depth) else {
        return;
    };
    // SAFETY: live resources.
    let (d, t) = unsafe { (host_depth.GetDesc(), twin.GetDesc()) };
    assert_eq!((d.Format.0, d.Flags.0), (44, 2));
    assert_eq!((t.Format.0, t.Flags.0), (60, 4));

    let mut pattern = vec![0u8; 1024];
    for row in 0..4 {
        for b in 0..4 {
            pattern[row * 256 + b] = (1 + row * 4 + b) as u8;
        }
    }
    let (upload, data) = host.upload_buffer(1024).unwrap();
    // SAFETY: the buffer's mapping, 1024 bytes.
    unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024) };
    let readback_heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let readback_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: 4096,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut readback: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a readback buffer.
    unsafe {
        host.device
            .CreateCommittedResource(&readback_heap, D3D12_HEAP_FLAG_NONE, &readback_desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut readback)
            .unwrap();
    }
    let readback = readback.unwrap();
    let mut plane = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    // SAFETY: a plain query writing one footprint.
    unsafe {
        host.device.GetCopyableFootprints(&d, 1, 1, 0, Some(&mut plane), None, None, None);
    }
    let pattern_rows = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT(60), Width: 4, Height: 4, Depth: 1, RowPitch: 256 },
    };

    let whole = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: index },
    };
    let rows = |buffer: &ID3D12Resource, placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: placed },
    };
    let (written, source, dest) = (
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    let moved = crate::backend::transition;

    let fill = host.new_list().unwrap();
    // SAFETY: an open list and live resources.
    unsafe {
        let l = &fill.list;
        l.ResourceBarrier(&[moved(twin, written, dest)]);
        l.CopyTextureRegion(&whole(twin, 0), 0, 0, 0, &rows(&upload, pattern_rows), None);
        l.ResourceBarrier(&[moved(twin, dest, written)]);
    }
    fill.close();
    host.execute(&[&fill]);

    let list = host.new_list().unwrap();
    assert!(list.twin_use(depth, true, true));
    list.twins_written();
    list.close();
    host.execute(&[&list]);

    let state = host_copy(depth).unwrap().state.load(Relaxed);
    let state = D3D12_RESOURCE_STATES(state as i32);
    let back = host.new_list().unwrap();
    // SAFETY: an open list and live resources; a whole subresource.
    unsafe {
        let l = &back.list;
        l.ResourceBarrier(&[moved(host_depth, state, source)]);
        let (to, from) = (rows(&readback, plane), whole(host_depth, 1));
        l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        l.ResourceBarrier(&[moved(host_depth, source, state)]);
    }
    back.close();
    host.execute(&[&back]);
    let mut mapped = std::ptr::null_mut();
    // SAFETY: the copies finished (execute waits); the buffer is 4096 bytes.
    let read = unsafe {
        readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 4096 }), Some(&mut mapped)).unwrap();
        std::slice::from_raw_parts(mapped.cast::<u8>(), 4096)
    };
    let pitch = plane.Footprint.RowPitch as usize;
    for row in 0..4 {
        let (at, from) = (row * pitch, row * 256);
        assert_eq!(read[at..at + 4], pattern[from..from + 4], "row {row}");
    }
}
