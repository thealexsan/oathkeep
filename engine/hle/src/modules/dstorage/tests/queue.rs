use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::modules::xasync::E_PENDING;
use crate::modules::xgameruntime::{Guid, HRESULT, S_OK};

use super::super::factory::QueueDesc;
use super::super::queue::{Status, StatusArray};
use super::super::scheduler::{Command, Job, Lane, STAGING};
use super::super::{
    END_OF_FILE, INVALID_DATA, Instance, MEMORY_SOURCE, Out, PAGE_DESTINATION,
    ZLIB, add_ref_instance,
};
use super::{
    call, enqueue, enqueue_status, factory, file_with, lanes, new_queue, pattern, read, request,
    result, status_array, void, wait, with_test_tables,
};

#[test]
fn statuses_complete_in_order_with_the_first_failure() {
    let contents = pattern(4096);
    let (_dir, file) = file_with(&contents);
    let (queue, array) = (new_queue(0), status_array(3));
    // Requests in flight together run in any order, so none of them
    // shares a destination (failed ones leave theirs undefined).
    let mut buffers = [[0u8; 16]; 5];
    let [failed, failed_too, out, other, bcpack] = buffers.each_mut().map(|b| b.as_mut_ptr().cast());

    // Segment 0: a read past the end, then a bad stream; the first wins.
    enqueue(queue, &request(0, failed, 16, file, 4090, 16));
    enqueue(queue, &request(ZLIB, failed_too, 16, file, 0, 16));
    enqueue_status(queue, array, 0);
    // Segment 1: two reads.
    enqueue(queue, &request(0, other, 16, file, 0, 16));
    enqueue(queue, &request(0, out, 16, file, 100, 16));
    enqueue_status(queue, array, 1);
    void(queue, 0x30);
    // Segment 2: a BCPack request (BC7) whose stream is empty fails.
    with_test_tables();
    enqueue(queue, &request(7 << 1, bcpack, 16, file, 0, 0));
    enqueue_status(queue, array, 2);
    void(queue, 0x30);

    assert_eq!(wait(array, 2), INVALID_DATA);
    assert_eq!(result(array, 0), END_OF_FILE);
    assert_eq!(result(array, 1), S_OK);
    assert_eq!(buffers[2], contents[100..116]);
    assert_eq!(buffers[3], contents[..16]);
}

#[test]
fn half_a_queue_waits_for_the_submit() {
    use crate::modules::xmem;
    // Two pages the title owns: the one its list still names from
    // earlier, and the one it maps for this batch when it submits.
    let mut pages = [0usize; 2];
    let mut n = 2;
    assert_eq!(xmem::XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
    let [stale, right] = pages;
    let plain = pattern(64);
    let mut list = [stale];
    let (queue, array) = (new_queue(1), status_array(1));
    // More than half of the queue's 128.
    for i in 0..100 {
        let mut r = request(
            PAGE_DESTINATION | MEMORY_SOURCE,
            list.as_mut_ptr().cast(),
            plain.len(),
            plain.as_ptr().cast(),
            0,
            plain.len(),
        );
        r.page_offset = i * 64;
        enqueue(queue, &r);
    }
    std::thread::sleep(Duration::from_millis(50));
    // SAFETY: our own list, which a worker may be reading.
    unsafe { std::ptr::write_volatile(list.as_mut_ptr(), right) };
    enqueue_status(queue, array, 0);
    void(queue, 0x30);
    assert_eq!(wait(array, 0), S_OK);
    let bytes = |page: usize| -> Vec<u8> {
        xmem::page_window(&[page], 0, 6400).unwrap().bytes().to_vec()
    };
    assert!(bytes(stale) == [0; 6400], "written to the page the list named before");
    assert!(bytes(right) == plain.repeat(100), "not written to the page the list names");
}

#[test]
fn a_full_queue_submits_itself() {
    let (_dir, file) = file_with(&pattern(64));
    let create: extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT =
        call(factory(), 0x50);
    let desc = QueueDesc {
        source_type: 0,
        capacity: 4,
        priority: 1,
        name: c"small".as_ptr(),
        unknown: 1,
    };
    let mut queue = std::ptr::null_mut();
    assert_eq!(create(factory(), &desc, std::ptr::null(), &mut queue), S_OK);
    let array = status_array(1);
    let mut outs = [[0u8; 16]; 4];
    for out in &mut outs[..2] {
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 8, 16));
    }
    enqueue_status(queue, array, 0);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(result(array, 0), E_PENDING, "three of four: not submitted");
    // One more than fits: would wait for good.
    for out in &mut outs[2..] {
        enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 8, 16));
    }
    assert_eq!(wait(array, 0), S_OK);
    void(queue, 0x40);
    assert!(outs[..2].iter().all(|out| out[..] == pattern(64)[8..24]));
}

#[test]
fn priorities_share_by_bytes_about_tenfold() {
    let mut l = lanes(STAGING);
    l.lanes.insert(0, Lane::new(0, 4096));
    l.lanes.insert(1, Lane::new(1, 4096));
    l.lanes.insert(2, Lane::new(2, 4096)); // real-time: as high
    for _ in 0..400 {
        for id in 0..3 {
            read(&mut l, id, 0, 4096);
        }
    }
    (0..3).for_each(|id| l.submit(id, false));
    let mut started = [0u32; 3];
    for _ in 0..330 {
        let job = l.start().unwrap();
        started[job.lane as usize] += 1;
        l.finish(&job, Ok(()));
    }
    // Normal gets about a tenth of what high and real-time get together.
    assert_eq!(started[1] + started[2], 300, "{started:?}");
    assert_eq!(started[0], 30, "{started:?}");
    assert!(started[1].abs_diff(started[2]) <= 1, "{started:?}");
}

#[test]
fn staging_bounds_compressed_bytes_in_flight() {
    let mut l = lanes(100);
    l.lanes.insert(0, Lane::new(0, 64));
    read(&mut l, 0, ZLIB, 60);
    read(&mut l, 0, ZLIB, 60);
    read(&mut l, 0, 0, 1000); // raw: straight to its destination
    l.submit(0, false);
    let first = l.start().unwrap();
    assert!(l.start().is_none(), "60 + 60 > 100");
    l.finish(&first, Ok(()));
    let second = l.start().unwrap();
    let raw = l.start().unwrap();
    assert_eq!((second.staging, raw.staging), (60, 0));
    // Alone, a request larger than the buffer still runs.
    read(&mut l, 0, ZLIB, 500);
    l.submit(0, false);
    assert!(l.start().is_none());
    l.finish(&second, Ok(()));
    assert!(l.start().is_some());
}

#[test]
fn a_status_waits_for_every_earlier_read() {
    let mut l = lanes(STAGING);
    l.lanes.insert(0, Lane::new(0, 64));
    let array = status_array(2).cast::<Instance<StatusArray>>();
    let status = |index| {
        add_ref_instance(array);
        Command::Status(Status { array, index, enqueued: 0 })
    };
    read(&mut l, 0, 0, 16);
    read(&mut l, 0, 0, 16);
    l.lanes.get_mut(&0).unwrap().unsubmitted.push(status(0));
    read(&mut l, 0, 0, 16);
    l.lanes.get_mut(&0).unwrap().unsubmitted.push(status(1));
    // SAFETY: an array we created.
    for entry in unsafe { &(*array).state.entries }.iter() {
        entry.store(E_PENDING, Ordering::Release);
    }
    l.submit(0, false);
    let jobs: Vec<Job> = (0..3).map(|_| l.start().unwrap()).collect();
    // The last read and the first finish; the second then fails.
    l.finish(&jobs[2], Ok(()));
    l.finish(&jobs[0], Ok(()));
    let results = || (result(array.cast(), 0), result(array.cast(), 1));
    assert_eq!(results(), (E_PENDING, E_PENDING));
    l.finish(&jobs[1], Err(INVALID_DATA));
    assert_eq!(results(), (INVALID_DATA, S_OK));
}

#[test]
fn a_closed_queue_completes_nothing() {
    let (_dir, file) = file_with(&pattern(64));
    let (queue, array) = (new_queue(0), status_array(1));
    let mut out = [0u8; 16];
    enqueue(queue, &request(0, out.as_mut_ptr().cast(), 16, file, 0, 16));
    enqueue_status(queue, array, 0);
    void(queue, 0x40);
    void(queue, 0x30);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(result(array, 0), E_PENDING);
    assert_eq!(out, [0; 16]);
}
