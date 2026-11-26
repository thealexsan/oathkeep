use std::sync::Mutex;

use super::acp::{COMMAND_COMPLETED, Message, REGISTER_MESSAGES};
use super::heap::{HEAP, heap_unmap};
use super::*;
use crate::modules::xasync::E_INVALIDARG;
use crate::modules::xgameruntime::S_OK;

mod xma_tests;

/// The context arrays and the heap exist once per process: tests that
/// allocate from them run one at a time (a concurrent allocation could
/// take a freed range a test expects back).
pub(super) static CONTEXT_TESTS: Mutex<()> = Mutex::new(());

#[test]
fn heap_maps_every_byte_of_a_block() {
    let _serial = lock(&CONTEXT_TESTS);
    let (mut ptr, mut apu) = (std::ptr::null_mut(), 0);
    assert_eq!(ApuAlloc(&mut ptr, &mut apu, 0x100, 128, 1), S_OK);
    assert!(apu != 0 && apu.is_multiple_of(128));
    let inner = ptr.cast::<u8>().wrapping_add(0x40).cast_const().cast();
    assert_eq!(ApuMapVirtualAddress(inner), apu + 0x40);
    assert!(ApuIsVirtualAddressValid(ptr, 128));
    assert_eq!(heap_unmap(apu + 4), Some(ptr.cast::<u8>().wrapping_add(4)));
    ApuFree(ptr);
    assert_eq!(ApuMapVirtualAddress(ptr), 0);
    assert_eq!(ApuAlloc(&mut ptr, &mut apu, 0x100, 6, 1), E_INVALIDARG);
}

#[test]
fn heap_reuses_freed_addresses() {
    let _serial = lock(&CONTEXT_TESTS);
    let (mut a, mut b, mut c) = (
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    let (mut apu_a, mut apu_b, mut apu_c) = (0, 0, 0);
    assert_eq!(ApuAlloc(&mut a, &mut apu_a, 0x1000, 0x1000, 1), S_OK);
    assert_eq!(ApuAlloc(&mut b, &mut apu_b, 0x1000, 0x1000, 1), S_OK);
    ApuFree(a);
    assert_eq!(ApuAlloc(&mut c, &mut apu_c, 0x800, 0x800, 1), S_OK);
    // The freed range is free again, whichever part `c` took.
    assert!(
        lock(&HEAP)
            .free
            .iter()
            .any(|(&s, &e)| s <= apu_a + 0x800 && apu_a + 0x1000 <= e)
            || apu_c == apu_a
    );
    assert_eq!(
        ApuMapApuAddress(apu_b + 8),
        b.cast::<u8>().wrapping_add(8).cast()
    );
    assert!(ApuMapApuAddress(0).is_null());
    assert_eq!(
        ApuCreateHeap(64 << 20, 0),
        0x8ACA_0001_u32 as i32,
        "too late"
    );
    ApuFree(b);
    ApuFree(c);
}

#[test]
fn full_message_queues_count_drops() {
    let mut acp = std::ptr::null_mut();
    assert_eq!(AcpHalCreate(&mut acp), S_OK);
    // SAFETY: just created.
    let m = unsafe { (*acp).vtable };
    assert_eq!((m.connect)(acp, 16, 2), S_OK);
    let mask = COMMAND_COMPLETED;
    assert_eq!(
        (m.submit)(
            acp,
            REGISTER_MESSAGES,
            0,
            u32::MAX,
            (&raw const mask).cast(),
            0
        ),
        S_OK
    );
    for id in 1..=4 {
        assert_eq!((m.submit)(acp, 12, id, u32::MAX, std::ptr::null(), 0), S_OK);
    }
    assert_eq!((m.message_count)(acp), 2);
    let mut message = Message {
        kind: 0,
        dropped: 0,
        usec: 0,
        data: [0; 16],
    };
    assert!((m.pop_message)(acp, &mut message) && (m.pop_message)(acp, &mut message));
    assert_eq!((m.submit)(acp, 12, 5, u32::MAX, std::ptr::null(), 0), S_OK);
    assert!((m.pop_message)(acp, &mut message));
    let dropped = message.dropped;
    assert_eq!(dropped, 2);
    (m.release)(acp);
}

#[test]
fn startup_as_fmod_does_it() {
    let _serial = lock(&CONTEXT_TESTS);
    // Context arrays: rate conversion, DMA and XMA.
    let mut request = ShapeContexts {
        counts: [0x200, 0, 0, 0x400, 0x200, 0],
        arrays: [std::ptr::null_mut(); 6],
        apu: [0; 6],
    };
    assert_eq!(size_of::<ShapeContexts>(), 0x60);
    assert_eq!(AcpHalAllocateShapeContexts(&mut request), S_OK);
    let arrays = request.arrays;
    assert!(!arrays[0].is_null() && !arrays[3].is_null() && !arrays[4].is_null());
    assert!(arrays[1].is_null());
    // SAFETY: 0x200 rate-conversion contexts of 32 bytes were allocated.
    unsafe { arrays[0].cast::<u8>().write_bytes(0, 0x200 << 5) };

    // Connect, register every message but frame starts, then wait for
    // the registration to be acknowledged.
    let mut acp = std::ptr::null_mut();
    assert_eq!(AcpHalCreate(&mut acp), S_OK);
    // SAFETY: just created.
    let m = unsafe { (*acp).vtable };
    assert_eq!((m.connect)(acp, 0x804, 0x804), S_OK);
    let mask = 0xfe_u32;
    let data = (&raw const mask).cast();
    assert_eq!(
        (m.submit)(acp, REGISTER_MESSAGES, 1, u32::MAX, data, 0),
        S_OK
    );
    let mut message = Message {
        kind: 0,
        dropped: 0,
        usec: 0,
        data: [0; 16],
    };
    assert!((m.pop_message)(acp, &mut message));
    let (kind, dropped) = (message.kind, message.dropped);
    assert_eq!((kind, dropped), (COMMAND_COMPLETED, 0));
    assert_eq!(
        u64::from_le_bytes(message.data[4..12].try_into().unwrap()),
        1
    );
    assert!(!(m.pop_message)(acp, &mut message));
    assert_eq!((m.disconnect)(acp), S_OK);
    assert_eq!((m.message_count)(acp), 1, "disconnected");
    (m.release)(acp);
    assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
}
