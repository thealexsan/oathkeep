use super::*;
use crate::object::slot;

fn create_as(iid: &Iid) -> (HRESULT, This) {
    let version = CHECKED_VERSION;
    let mut device = std::ptr::null();
    let hr = create(std::ptr::null(), &version, iid, &mut device);
    (hr, device)
}

#[test]
fn creation_hands_out_the_device() {
    let (hr, device) = create_as(&IID);
    assert_eq!(hr, S_OK);
    assert_eq!(device, super::device());
    // The title tests byte 0x18 before inlining a device call.
    // SAFETY: the device is 0x58 bytes.
    assert_ne!(unsafe { device.cast::<u8>().add(0x18).read() }, 0);
    assert_eq!(create_as(&[0; 16]).0, E_NOINTERFACE);
    assert_eq!(
        slot::<extern "system" fn(This) -> u32>(device, 0x38)(device),
        1
    );
    let missing = slot::<extern "system" fn() -> HRESULT>(device, 0xf0);
    assert_eq!(missing(), E_NOTIMPL);
}

#[test]
fn newer_device_levels_are_the_same_device() {
    // ID3D12Device11, as the title asks for it.
    let (hr, device) = create_as(&LEVEL_IIDS[8]);
    assert_eq!((hr, device), (S_OK, super::device()));
    let mut same = std::ptr::null();
    assert_eq!(query_interface(device, &LEVEL_IIDS[4], &mut same), S_OK);
    assert_eq!(same, device);
    // A Device8 method we have not written fails without crashing.
    let missing = slot::<extern "system" fn() -> HRESULT>(device, 0x3a0);
    assert_eq!(missing(), E_NOTIMPL);
}

/// A device child, and the DXGI device, hand out the device as any
/// interface the device itself answers to.
#[test]
fn the_device_is_found_at_every_level_through_others() {
    type Get = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
    let fence_iid = &crate::fence::FENCE_IIDS[0];
    let mut fence = std::ptr::null();
    assert_eq!(
        crate::fence::create(std::ptr::null(), 0, 0, fence_iid, &mut fence),
        S_OK
    );
    let get_device = slot::<Get>(fence, 0x38);
    let dxgi = dxgi::device();
    let query = slot::<Get>(dxgi, 0);
    for iid in std::iter::once(&IID).chain(&LEVEL_IIDS) {
        let mut found = std::ptr::null();
        assert_eq!(get_device(fence, iid, &mut found), S_OK, "{iid:02x?}");
        assert_eq!(found, device());
        let mut found = std::ptr::null();
        assert_eq!(query(dxgi, iid, &mut found), S_OK, "{iid:02x?}");
        assert_eq!(found, device());
    }
    // What the device is not, it is not through them either.
    let mut found = device();
    assert_eq!(get_device(fence, fence_iid, &mut found), E_NOINTERFACE);
    assert!(found.is_null());
    slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence);
}

/// The host's: a fence's `GetDevice` for `ID3D12Device5`.
#[cfg(windows)]
#[test]
fn the_hosts_children_hand_out_newer_device_levels() {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FENCE_FLAG_NONE, ID3D12Device5, ID3D12Fence,
    };
    let Some(host) = crate::host::host() else {
        return; // no host GPU here
    };
    // SAFETY: plain calls on the host device and its fence.
    unsafe {
        let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let mut device: Option<ID3D12Device5> = None;
        assert!(fence.GetDevice(&mut device).is_ok());
        assert!(device.is_some());
    }
}

#[test]
fn virtual_memory_configuration_reports_user_space() {
    let mut config = VirtualMemory {
        flags: 0,
        page_table_pages: 7,
        preserved_bytes: 0,
        lowest: 0,
        highest: 0,
    };
    assert_eq!(configure_virtual_memory(&mut config), S_OK);
    assert!(0 < config.lowest && config.lowest < config.highest);
    let heap = Box::new(0u8);
    assert!((config.lowest..=config.highest).contains(&(&raw const *heap as u64)));
    assert_eq!(configure_virtual_memory(std::ptr::null_mut()), E_INVALIDARG);
}

#[test]
fn feature_levels_pick_the_highest_supported() {
    type Check = extern "system" fn(This, u32, *mut c_void, u32) -> HRESULT;
    let device = device();
    let check = slot::<Check>(device, 0x68);
    let query = |levels: &[u32], size| {
        let mut query = FeatureLevels {
            count: levels.len() as u32,
            requested: levels.as_ptr(),
            highest: 0,
        };
        let hr = check(device, 2, (&raw mut query).cast(), size);
        (hr, query.highest)
    };
    let levels = [0xb000, 0xb100, 0xc000, 0xc100, 0xd000];
    assert_eq!(query(&levels, 0x18), (S_OK, 0xc100));
    assert_eq!(query(&levels, 0x10).0, E_INVALIDARG);
    assert_eq!(query(&levels[4..], 0x18).0, UNSUPPORTED);
}

#[test]
fn offline_pipeline_deserializers_validate_and_fallback() {
    type Deserialize = extern "system" fn(This, *const u8, *const Iid, *mut This) -> HRESULT;
    let device = device();
    for offset in [0x1c8usize, 0x218usize] {
        let f = slot::<Deserialize>(device, offset);
        let mut out = device;
        assert_eq!(f(device, std::ptr::null(), &IID, &mut out), E_INVALIDARG);
        assert!(out.is_null(), "slot {offset:#x} must clear *out on E_INVALIDARG");
        let dummy = [0u8; 64];
        let mut out = device;
        assert_eq!(f(device, dummy.as_ptr(), &IID, &mut out), E_NOTIMPL);
        assert!(out.is_null(), "slot {offset:#x} must clear *out on E_NOTIMPL");
    }
}
