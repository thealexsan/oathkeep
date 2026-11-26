use super::*;

const TAG: Iid = [1; 16];
const OTHER_TAG: Iid = [2; 16];

extern "system" fn release_test_object(this: This) -> u32 {
    release_boxed::<Object>(this)
}

fn new_test_object() -> This {
    static TABLE: Table<7> = {
        let mut functions = d3d12_object(missing_table::<DEVICE, 7>());
        functions[2] = release_test_object as *const ();
        Table(functions)
    };
    let mut this = std::ptr::null();
    assert_eq!(
        hand_out(
            DEVICE,
            Box::new(Object::new(&TABLE)),
            &TAG,
            &[TAG],
            &mut this
        ),
        S_OK
    );
    this
}

#[test]
fn private_data_is_scoped_to_object_and_id() {
    let first = new_test_object();
    let second = new_test_object();
    let set =
        slot::<extern "system" fn(This, *const Iid, u32, *const u8) -> HRESULT>(first, 0x20);
    let get =
        slot::<extern "system" fn(This, *const Iid, *mut u32, *mut u8) -> HRESULT>(first, 0x18);
    let release = slot::<extern "system" fn(This) -> u32>(first, 0x10);

    assert_eq!(set(first, &TAG, 1, b"a".as_ptr()), S_OK);
    assert_eq!(set(first, &OTHER_TAG, 0, b"".as_ptr()), S_OK);
    assert_eq!(set(second, &TAG, 2, b"bc".as_ptr()), S_OK);
    assert_eq!(set(first, &TAG, 3, b"def".as_ptr()), S_OK);
    assert_eq!(add_ref(first), 2);
    assert_eq!(release(first), 1);

    let mut size = 0;
    let mut data = [0; 3];
    assert_eq!(get(first, &TAG, &mut size, std::ptr::null_mut()), S_OK);
    assert_eq!(size, 3);
    size = 1;
    assert_eq!(get(first, &TAG, &mut size, data.as_mut_ptr()), MORE_DATA);
    assert_eq!(size, 3);
    assert_eq!(data, [0; 3]);
    assert_eq!(get(first, &TAG, &mut size, data.as_mut_ptr()), S_OK);
    assert_eq!(&data, b"def");

    assert_eq!(set(first, &TAG, 99, std::ptr::null()), S_OK);
    assert_eq!(get(first, &TAG, &mut size, data.as_mut_ptr()), NOT_FOUND);
    assert_eq!(size, 0);
    assert_eq!(get(first, &OTHER_TAG, &mut size, data.as_mut_ptr()), S_OK);
    assert_eq!(size, 0);
    assert_eq!(release(first), 0);
    size = 3;
    assert_eq!(get(second, &TAG, &mut size, data.as_mut_ptr()), S_OK);
    assert_eq!(&data[..size as usize], b"bc");
    assert_eq!(release(second), 0);
}

#[test]
#[ignore = "synthetic release benchmark; run with --release --ignored --nocapture"]
fn benchmark_private_data_release() {
    use std::hint::black_box;
    use std::time::Instant;

    const RELEASES: usize = 50_000;
    for count in [128, 512, 1024] {
        let live: Vec<_> = (0..count).map(|_| new_test_object()).collect();
        let set = slot::<extern "system" fn(This, *const Iid, u32, *const u8) -> HRESULT>(
            live[0], 0x20,
        );
        let release = slot::<extern "system" fn(This) -> u32>(live[0], 0x10);
        for &object in &live {
            assert_eq!(set(object, &TAG, 1, b"a".as_ptr()), S_OK);
        }
        let mut samples = [0.0; 5];
        for sample in &mut samples {
            let mut retiring: Vec<_> = (0..256).map(|_| new_test_object()).collect();
            let retiring_count = retiring.len();
            let start = Instant::now();
            for index in 0..RELEASES {
                let object = new_test_object();
                if index % 4 == 0 {
                    assert_eq!(set(object, &TAG, 1, b"b".as_ptr()), S_OK);
                }
                let old = std::mem::replace(&mut retiring[index % retiring_count], object);
                black_box(release(black_box(old)));
            }
            *sample = start.elapsed().as_secs_f64() * 1e9 / RELEASES as f64;
            for object in retiring {
                assert_eq!(release(object), 0);
            }
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "{count:4} live entries: {:8.1} ns/object (median)",
            samples[2]
        );
        for object in live {
            assert_eq!(release(object), 0);
        }
    }
}

#[test]
fn final_release_clears_private_data_before_drop() {
    #[repr(C)]
    struct OwnedObject(Object);

    impl Drop for OwnedObject {
        fn drop(&mut self) {
            let address = std::ptr::from_ref(&self.0) as usize;
            let data = private_data();
            assert!(!data.contains_key(&(address, [u8::MIN; 16])));
            assert!(!data.contains_key(&(address, TAG)));
            assert!(!data.contains_key(&(address, [u8::MAX; 16])));
        }
    }

    static TABLE: Table<0> = Table([]);
    let this: This = Box::into_raw(Box::new(OwnedObject(Object::new(&TABLE)))).cast();
    assert_eq!(add_ref(this), 1);
    for id in [[u8::MIN; 16], TAG, [u8::MAX; 16]] {
        assert_eq!(set_private_data(this, &id, 1, b"a".as_ptr()), S_OK);
    }
    assert_eq!(release_boxed::<OwnedObject>(this), 0);
}

#[test]
fn guids_round_trip_through_text() {
    let g = guid(
        0x189819f1,
        0x1db6,
        0x4b57,
        [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
    );
    assert_eq!(g[..4], [0xf1, 0x19, 0x98, 0x18]);
    assert_eq!(guid_text(&g), "189819f1-1db6-4b57-be54-1821339b85f7");
}

/// Every COM object answers for `IUnknown`, and with one pointer
/// whichever of its interfaces is asked: the object's identity.
#[test]
fn every_object_answers_for_iunknown() {
    type Query = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
    let unknown = |object: This| {
        let mut found = std::ptr::null();
        let hr = slot::<Query>(object, 0)(object, &IUNKNOWN, &mut found);
        (hr, found)
    };
    let device = crate::device::device();
    let mut fence = std::ptr::null();
    let fence_iid = &crate::fence::FENCE_IIDS[0];
    assert_eq!(
        crate::fence::create(std::ptr::null(), 0, 0, fence_iid, &mut fence),
        S_OK
    );
    assert_eq!(unknown(device), (S_OK, device));
    assert_eq!(unknown(fence), (S_OK, fence));
    // The DXGI device is the same object as the device it answers for.
    assert_eq!(unknown(crate::dxgi::device()), (S_OK, device));
    // An id nobody has is still nobody's.
    let mut found = fence;
    let nobodys = guid(0x189819f1, 0x1db6, 0x4b57, [0xbe; 8]);
    assert_eq!(
        slot::<Query>(fence, 0)(fence, &nobodys, &mut found),
        E_NOINTERFACE
    );
    assert!(found.is_null());
    slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence);
}
