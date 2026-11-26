use super::*;
use crate::object::{E_NOINTERFACE, E_POINTER, S_FALSE, S_OK, slot};

/// An interface we do not implement.
const OTHER_IID: Iid = guid(0x5a5a_5a5a, 0x5a5a, 0x5a5a, [0x5a; 8]);

/// The shim hands the blob to the host unchanged; `create` keeps it, so
/// any bytes serve.
const BLOB: [u8; 4] = [0x01, 0x02, 0x03, 0x04];

fn created(iid: &Iid) -> This {
    let mut out = std::ptr::null();
    let hr = create(
        std::ptr::null(),
        0,
        BLOB.as_ptr(),
        BLOB.len(),
        iid,
        &mut out,
    );
    assert_eq!(hr, S_OK);
    assert!(!out.is_null());
    out
}

/// The host's structures, field for field (64-bit layout).
#[cfg(target_pointer_width = "64")]
#[test]
fn descriptions_keep_the_pc_layout() {
    assert_eq!(std::mem::size_of::<Desc>(), 40);
    assert_eq!(std::mem::offset_of!(Desc, parameter_count), 0);
    assert_eq!(std::mem::offset_of!(Desc, parameters), 8);
    assert_eq!(std::mem::offset_of!(Desc, sampler_count), 16);
    assert_eq!(std::mem::offset_of!(Desc, samplers), 24);
    assert_eq!(std::mem::offset_of!(Desc, flags), 32);
    assert_eq!(std::mem::size_of::<VersionedDesc>(), 48);
    assert_eq!(std::mem::offset_of!(VersionedDesc, version), 0);
    assert_eq!(std::mem::offset_of!(VersionedDesc, desc), 8);
}

#[test]
fn console_flags_are_dropped_and_the_rest_kept() {
    assert_eq!(CONSOLE_FLAGS, 0xff00_0000);
    let desc = Desc {
        parameter_count: 2,
        parameters: BLOB.as_ptr().cast(),
        sampler_count: 1,
        samplers: BLOB.as_ptr().cast(),
        flags: 0xffff_ffff,
    };
    let plain = without_console_flags(desc);
    assert_eq!(plain.flags, 0x00ff_ffff);
    assert_eq!((plain.parameter_count, plain.sampler_count), (2, 1));
    assert!(std::ptr::eq(plain.parameters, desc.parameters));
    assert!(std::ptr::eq(plain.samplers, desc.samplers));

    let low = Desc {
        flags: 0x00ab_cdef,
        ..desc
    };
    assert_eq!(without_console_flags(low).flags, 0x00ab_cdef);
    let none = Desc { flags: 0, ..desc };
    assert_eq!(without_console_flags(none).flags, 0);
}

#[test]
fn serializers_reject_a_null_description() {
    let mut blob = std::ptr::null_mut();
    let mut error = std::ptr::null_mut();
    assert_eq!(
        serialize(std::ptr::null(), 1, &mut blob, &mut error),
        E_INVALIDARG
    );
    assert_eq!(
        serialize_versioned(std::ptr::null(), &mut blob, &mut error),
        E_INVALIDARG
    );
    assert!(blob.is_null() && error.is_null());
}

/// A null `riid` used to be forwarded to the host, which dereferences it
/// (an access violation in `d3d12.dll`); an empty blob is malformed.
#[test]
fn deserializer_creation_rejects_missing_arguments() {
    let iid = ROOT_SIGNATURE_IIDS[0];
    let mut out = std::ptr::null_mut();
    assert_eq!(
        create_deserializer(std::ptr::null(), BLOB.len(), &iid, &mut out),
        E_INVALIDARG
    );
    assert_eq!(
        create_deserializer(BLOB.as_ptr(), BLOB.len(), std::ptr::null(), &mut out),
        E_INVALIDARG
    );
    assert_eq!(
        create_deserializer(BLOB.as_ptr(), 0, &iid, &mut out),
        E_INVALIDARG
    );
    assert_eq!(
        create_deserializer(BLOB.as_ptr(), BLOB.len(), &iid, std::ptr::null_mut()),
        E_INVALIDARG
    );
    assert!(out.is_null());
}

#[test]
fn creating_a_root_signature_checks_its_arguments() {
    let iid = ROOT_SIGNATURE_IIDS[0];
    let mut out = std::ptr::null();
    assert_eq!(
        create(
            std::ptr::null(),
            0,
            std::ptr::null(),
            BLOB.len(),
            &iid,
            &mut out
        ),
        E_INVALIDARG
    );
    assert_eq!(
        create(std::ptr::null(), 0, BLOB.as_ptr(), 0, &iid, &mut out),
        E_INVALIDARG
    );
    assert!(out.is_null());

    // An unknown interface is dropped, and the answer is null.
    assert_eq!(
        create(
            std::ptr::null(),
            0,
            BLOB.as_ptr(),
            BLOB.len(),
            &OTHER_IID,
            &mut out
        ),
        E_NOINTERFACE
    );
    assert!(out.is_null());
    assert_eq!(
        create(
            std::ptr::null(),
            0,
            BLOB.as_ptr(),
            BLOB.len(),
            std::ptr::null(),
            &mut out
        ),
        E_NOINTERFACE
    );
    assert!(out.is_null());

    // Nowhere to write means the title only asked whether creation works.
    assert_eq!(
        create(
            std::ptr::null(),
            0,
            BLOB.as_ptr(),
            BLOB.len(),
            &iid,
            std::ptr::null_mut()
        ),
        S_FALSE
    );
}

#[test]
fn every_known_interface_creates_a_working_object() {
    let root = created(&ROOT_SIGNATURE_IIDS[0]);
    let query = slot::<extern "system" fn(This, *const Iid, *mut This) -> HRESULT>(root, 0);
    for iid in &ROOT_SIGNATURE_IIDS {
        let mut out = std::ptr::null();
        assert_eq!(query(root, iid, &mut out), S_OK);
        assert!(std::ptr::eq(out, root));
        assert_eq!(object::release(out), 1, "the original reference remains");
    }
    let mut out = std::ptr::null();
    assert_eq!(query(root, &OTHER_IID, &mut out), E_NOINTERFACE);
    assert!(out.is_null());
    assert_eq!(query(root, std::ptr::null(), &mut out), E_NOINTERFACE);
    assert_eq!(
        query(root, &ROOT_SIGNATURE_IIDS[0], std::ptr::null_mut()),
        E_POINTER
    );
    assert_eq!(object::release(root), 0);
}

#[test]
fn the_table_has_the_object_and_device_child_slots() {
    let root = created(&ROOT_SIGNATURE_IIDS[1]);
    // SetPrivateDataInterface is the one object method left out.
    assert_eq!(
        slot::<extern "system" fn() -> HRESULT>(root, 0x28)(),
        E_NOTIMPL
    );
    assert_eq!(
        slot::<extern "system" fn(This, *const u16) -> HRESULT>(root, 0x30)(
            root,
            std::ptr::null()
        ),
        S_OK
    );
    // GetDevice: an unknown interface is answered, not dereferenced.
    let mut device = std::ptr::null();
    let get_device =
        slot::<extern "system" fn(This, *const Iid, *mut This) -> HRESULT>(root, 0x38);
    assert_eq!(
        get_device(root, std::ptr::null(), &mut device),
        E_NOINTERFACE
    );
    assert!(device.is_null());
    assert_eq!(object::release(root), 0);
}

/// One 32-bit constant at b0, serialized by the host, then created.
#[cfg(windows)]
#[test]
fn host_serialized_blobs_create_root_signatures() {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_ROOT_CONSTANTS, D3D12_ROOT_PARAMETER, D3D12_ROOT_PARAMETER_0,
        D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS, D3D12_SHADER_VISIBILITY_ALL,
    };
    let parameter = D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Constants: D3D12_ROOT_CONSTANTS {
                ShaderRegister: 0,
                RegisterSpace: 0,
                Num32BitValues: 1,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    let desc = Desc {
        parameter_count: 1,
        parameters: (&raw const parameter).cast(),
        sampler_count: 0,
        samplers: std::ptr::null(),
        flags: 0x0100_0001,
    };
    let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(serialize(&desc, 1, &mut blob, &mut error), S_OK);
    let blob = blob.cast_const().cast::<Object>();
    let data = slot::<extern "system" fn(This) -> *const u8>(blob, 0x18)(blob);
    let size = slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob);
    assert!(size > 0);
    let null = std::ptr::null();
    let mut root = null;
    assert_eq!(
        create(null, 0, data, size, &ROOT_SIGNATURE_IIDS[0], &mut root),
        S_OK
    );
    assert_eq!(slot::<extern "system" fn(This) -> u32>(root, 0x10)(root), 0);
    slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
}

/// The host's deserializer reads our blobs back (the upscaler's call).
#[cfg(windows)]
#[test]
fn our_blobs_deserialize_on_the_host() {
    use windows::Win32::Graphics::Direct3D12::ID3D12RootSignatureDeserializer;
    use windows::core::Interface;
    let desc = VersionedDesc {
        version: 1,
        desc: Desc {
            parameter_count: 0,
            parameters: std::ptr::null(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags: 1,
        },
    };
    let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(serialize_versioned(&desc, &mut blob, &mut error), S_OK);
    let blob = blob.cast_const().cast::<Object>();
    let data = slot::<extern "system" fn(This) -> *const u8>(blob, 0x18)(blob);
    let size = slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob);
    let iid: Iid = ID3D12RootSignatureDeserializer::IID.to_u128().to_be_bytes();
    let iid = guid(
        u32::from_be_bytes(iid[0..4].try_into().unwrap()),
        u16::from_be_bytes(iid[4..6].try_into().unwrap()),
        u16::from_be_bytes(iid[6..8].try_into().unwrap()),
        iid[8..16].try_into().unwrap(),
    );
    let mut out = std::ptr::null_mut();
    assert_eq!(create_deserializer(data, size, &iid, &mut out), S_OK);
    // SAFETY: the host handed out this interface.
    let deserializer = unsafe { ID3D12RootSignatureDeserializer::from_raw(out) };
    // SAFETY: valid while the deserializer lives.
    let read = unsafe { &*deserializer.GetRootSignatureDesc() };
    assert_eq!((read.NumParameters, read.Flags.0), (0, 1));
    slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
}

/// Version 1.0 through the versioned entry point, with a console flag.
#[cfg(windows)]
#[test]
fn versioned_descriptions_serialize_on_the_host() {
    let desc = VersionedDesc {
        version: 1,
        desc: Desc {
            parameter_count: 0,
            parameters: std::ptr::null(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags: 0x0200_0001,
        },
    };
    let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(serialize_versioned(&desc, &mut blob, &mut error), S_OK);
    let blob = blob.cast_const().cast::<Object>();
    assert!(slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob) > 0);
    slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
    assert_eq!(
        serialize_versioned(std::ptr::null(), &mut blob.cast_mut().cast(), &mut error),
        E_INVALIDARG
    );
}

/// Truncated and nonsense blobs reach the host, which rejects them
/// without writing an object.
#[cfg(windows)]
#[test]
fn malformed_blobs_are_rejected_by_the_host() {
    use windows::Win32::Graphics::Direct3D12::ID3D12RootSignatureDeserializer;
    use windows::core::Interface;
    let desc = VersionedDesc {
        version: 1,
        desc: Desc {
            parameter_count: 0,
            parameters: std::ptr::null(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags: 1,
        },
    };
    let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(serialize_versioned(&desc, &mut blob, &mut error), S_OK);
    let blob = blob.cast_const().cast::<Object>();
    let data = slot::<extern "system" fn(This) -> *const u8>(blob, 0x18)(blob);
    let size = slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob);
    let iid: Iid = ID3D12RootSignatureDeserializer::IID.to_u128().to_be_bytes();
    let iid = guid(
        u32::from_be_bytes(iid[0..4].try_into().unwrap()),
        u16::from_be_bytes(iid[4..6].try_into().unwrap()),
        u16::from_be_bytes(iid[6..8].try_into().unwrap()),
        iid[8..16].try_into().unwrap(),
    );
    let mut out = std::ptr::null_mut();
    assert_eq!(
        create_deserializer(data, size - 1, &iid, &mut out),
        E_INVALIDARG
    );
    assert!(out.is_null());
    let garbage = [0xffu8; 64];
    assert_eq!(
        create_deserializer(garbage.as_ptr(), garbage.len(), &iid, &mut out),
        E_INVALIDARG
    );
    assert!(out.is_null());
    slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
}
