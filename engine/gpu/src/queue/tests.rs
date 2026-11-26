use super::*;
use crate::object::slot;

/// Where a queue's signal is: newer layouts have it two slots later.
/// Another test may switch layouts while this one runs, so the queue's
/// own table decides.
fn signal_slot(q: This) -> usize {
    // SAFETY: one of our queues; its first field is the table.
    let table = unsafe { *q.cast::<*const *const ()>() };
    if table == NEWER_FUNCTIONS.0.as_ptr() {
        0x80
    } else {
        0x70
    }
}

#[test]
fn queue_signals_complete_at_once() {
    let null = std::ptr::null();
    let desc = QueueDesc {
        kind: 0,
        priority: 0,
        flags: 0,
        nodes: 0,
    };
    let mut q = null;
    assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
    // SAFETY: the title reads the type at +0x10.
    assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
    let mut fence = null;
    let fence_iid = &crate::fence::FENCE_IIDS[0];
    assert_eq!(
        crate::fence::create(null, 0, 0, fence_iid, &mut fence),
        S_OK
    );
    let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
    assert_eq!(signal(q, fence, 7), S_OK);
    assert_eq!(
        slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
        7
    );
    assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
}

/// The title's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
#[test]
fn console_queues_are_created_as_on_pc() {
    let null = std::ptr::null();
    let desc = QueueDescX {
        kind: 2,
        flags: 0,
        engine_or_pipe: 1,
        queue: 0,
    };
    let mut q = null;
    assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
    assert_eq!(queue(q).desc.kind, 2);
    let none = std::ptr::null();
    assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
}

#[test]
fn newer_queues_have_their_methods_two_slots_later() {
    let (old, new) = (queue_table(false), queue_table(true));
    assert_eq!(old[10], new[10]); // submission stays at 0x50
    assert_eq!(old[14], new[16]); // signal 0x70 → 0x80
    assert_eq!(old[30], new[32]); // present 0xf0 → 0x100
    assert_eq!(new[35], execute_x as *const ()); // 0x118
}

#[test]
fn a_pair_of_timestamps_shows_the_load_it_is_given() {
    let (start, end) = (0xd0419_6698, 0xd0419_6690);
    assert_eq!(
        written::as_loaded(5_000_000, end, 4_000_000)
            - written::as_loaded(5_000_000, start, 4_000_000),
        4_000_000
    );
    assert_eq!(written::as_loaded(5_000_000, start, 0), 5_000_000);
    assert_eq!(written::as_loaded(5, start, 100), 1);
}

#[test]
fn timestamps_and_values_are_written() {
    let mut at = [0u64; 2];
    Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
    assert_eq!(at[0], 2);
    Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
    let first = at[1];
    std::thread::sleep(std::time::Duration::from_millis(2));
    Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
    assert!(at[1] > first);
}
