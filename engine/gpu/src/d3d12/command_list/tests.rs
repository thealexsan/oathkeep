use super::*;
use crate::d3d12::object::slot;

#[test]
fn every_inline_command_falls_back_to_the_table() {
    let null = std::ptr::null();
    let mut allocator = null;
    assert_eq!(
        create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
        S_OK
    );
    let mut list = null;
    let hr = create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list);
    assert_eq!(hr, S_OK);
    // What the title's inline draw checks: put < draw limit.
    // SAFETY: the list is 0x560 bytes.
    let (put, draw_limit, api_limit) = unsafe {
        let at = |o: usize| list.cast::<u8>().add(o).cast::<u64>().read();
        (at(0x18), at(0x28), at(0x30))
    };
    assert!(put >= draw_limit && put >= api_limit);
    // The console's ID3D12GraphicsCommandList6, as the title asks.
    let mut same = null;
    assert_eq!(list_query(list, &LIST_IIDS[7], &mut same), S_OK);
    assert_eq!(same, list);
    assert_eq!(slot::<extern "system" fn(This) -> u32>(list, 0x10)(list), 1);
    assert_eq!(slot::<extern "system" fn(This) -> u32>(list, 0x40)(list), 0);
    assert_eq!(
        slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list),
        S_OK
    );
    // List-owned upload memory: aligned, writable, gone at reset.
    let p = allocate_upload(list, 40, 0x100);
    assert!(!p.is_null() && (p as usize).is_multiple_of(0x100));
    // SAFETY: 40 bytes were handed out.
    unsafe { p.write_bytes(0xab, 40) };
    assert!(allocate_upload(list, 8, 0x30).is_null(), "not a power of two");
    assert_eq!(reset(list, allocator, null), S_OK);
    // SAFETY: the list is ours.
    assert!(unsafe { &(*list.cast::<List>()).uploads }.lock().unwrap().is_empty());
    let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
    assert_eq!(release(list), 0);
    assert_eq!(release(allocator), 0);
}

/// An offset near `u64::MAX` must not overflow and write outside the
/// query destination buffer.
#[test]
fn resolve_queries_rejects_overflowing_offset() {
    let null = std::ptr::null();
    let mut allocator = null;
    assert_eq!(
        create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
        S_OK
    );
    let mut list = null;
    assert_eq!(
        create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
        S_OK
    );
    let buffer = crate::d3d12::resource::test_buffer(64);
    // Used to overflow `offset + count * 8` (panic in debug, wrap in release).
    resolve_queries(list, null, 0, 0, 2, buffer, u64::MAX - 8);
    let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
    for object in [buffer, list, allocator] {
        assert_eq!(release(object), 0);
    }
}

#[test]
fn clear_uav_slots_are_wired_and_tolerate_null_values() {
    type ClearUint =
        extern "system" fn(This, u64, u64, This, *const [u32; 4], u32, *const Rect);
    type ClearFloat =
        extern "system" fn(This, u64, u64, This, *const [f32; 4], u32, *const Rect);
    let null = std::ptr::null();
    let mut allocator = null;
    assert_eq!(
        create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
        S_OK
    );
    let mut list = null;
    assert_eq!(
        create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
        S_OK
    );
    let clear_u = slot::<ClearUint>(list, 0x1a8);
    let clear_f = slot::<ClearFloat>(list, 0x1b0);
    // Null value pointers and empty handles must return safely without crashing.
    clear_u(list, 0, 0, null, std::ptr::null(), 0, std::ptr::null());
    clear_f(list, 0, 0, null, std::ptr::null(), 0, std::ptr::null());
    let buffer = crate::d3d12::resource::test_buffer(64);
    let uvals = [0x1234_5678u32; 4];
    let fvals = [1.0f32, 2.0, 3.0, 4.0];
    clear_u(list, 0, 0, buffer, &uvals, 0, std::ptr::null());
    clear_f(list, 0, 0, buffer, &fvals, 0, std::ptr::null());
    let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
    for object in [buffer, list, allocator] {
        assert_eq!(release(object), 0);
    }
}
