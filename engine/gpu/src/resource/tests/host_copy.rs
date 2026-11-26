use super::super::buffer_bytes;
use super::super::create::create_committed;
use super::super::footprint::PlacedFootprint;
use super::super::host_tex::{host_copy, host_resource, host_transition};
use super::{PROPERTIES, buffer, texture};
use crate::object::{HRESULT, S_OK, This, guid, slot};

#[test]
fn buffer_data_reaches_the_host_texture() {
    use windows::Win32::Graphics::Direct3D12::*;
    let Some(host) = crate::host::host() else {
        return;
    };
    let null = std::ptr::null();
    let iid = &super::RESOURCE_IIDS[0];
    const COPY_DEST: u32 = 0x400;
    let (mut image, mut source) = (null, null);
    let rgba = texture(4, 4, 28, 1);
    assert_eq!(create_committed(null, &PROPERTIES, 0, &rgba, COPY_DEST, null.cast(), iid, &mut image), S_OK);
    assert_eq!(create_committed(null, &PROPERTIES, 0, &buffer(1024), 0, null.cast(), iid, &mut source), S_OK);
    let (data, _) = buffer_bytes(source).unwrap();
    // SAFETY: the buffer holds 1024 bytes.
    let bytes = unsafe { std::slice::from_raw_parts_mut(data, 1024) };
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }

    let queue_iid = guid(0x0ec870a6, 0x5d7e, 0x4c22, [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed]);
    let allocator_iid = guid(0x6102dee4, 0xaf59, 0x4b09, [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24]);
    let list_iid = guid(0x5b160d0f, 0xac1b, 0x4185, [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55]);
    let (mut queue, mut allocator, mut list) = (null, null, null);
    let queue_desc = [0u32; 4];
    assert_eq!(crate::queue::create(null, queue_desc.as_ptr().cast(), &queue_iid, &mut queue), S_OK);
    assert_eq!(crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator), S_OK);
    assert_eq!(crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list), S_OK);

    #[repr(C)]
    struct Location {
        resource: This,
        kind: u32,
        footprint: PlacedFootprint,
    }
    let rows = PlacedFootprint { offset: 0, format: 28, width: 4, height: 4, depth: 1, row_pitch: 256 };
    let to = Location { resource: image, kind: 0, footprint: PlacedFootprint { offset: 0, ..rows } };
    let from = Location { resource: source, kind: 1, footprint: rows };
    type Copy = extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const u8);
    slot::<Copy>(list, 0x80)(list, &to, 0, 0, 0, &from, null.cast());
    slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list);
    slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &list);

    let readback_heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let readback_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: 1024,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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
    let host_image = host_resource(image).unwrap();
    let back = host.new_list().unwrap();
    let (_, before) = host_transition(image, 0x800).unwrap();
    assert!(std::sync::Arc::strong_count(&host_copy(image).unwrap()) > 2);
    use windows::core::Interface;
    let out_to = Location { resource: readback.as_raw().cast_const().cast(), kind: 1, footprint: rows };
    let out_from = Location { resource: host_image.as_raw().cast_const().cast(), kind: 0, footprint: to.footprint };
    // SAFETY: an open list, live resources, locations in the host layout.
    unsafe {
        back.list.ResourceBarrier(&[crate::host::transition(
            host_image,
            D3D12_RESOURCE_STATES(before as i32),
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        back.list.CopyTextureRegion((&raw const out_to).cast(), 0, 0, 0, (&raw const out_from).cast(), None);
    }
    back.close();
    host.execute(&[&back]);
    let mut mapped = std::ptr::null_mut();
    // SAFETY: the copy finished (execute waits); the buffer is 1024 bytes.
    let read = unsafe {
        readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 1024 }), Some(&mut mapped)).unwrap();
        std::slice::from_raw_parts(mapped.cast::<u8>(), 1024)
    };
    for row in 0..4 {
        let at = row * 256;
        assert_eq!(read[at..at + 16], bytes[at..at + 16], "row {row}");
    }
}
