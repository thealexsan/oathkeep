use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use super::super::super::xgameruntime::{HRESULT, S_OK};
use super::super::queue::{
    COMPLETION, Callback, INFINITE, Queue, WORK, XTaskQueueCloseHandle, XTaskQueueCreate,
    XTaskQueueCreateComposite, XTaskQueueDispatch, XTaskQueueGetPort,
    XTaskQueueSubmitDelayedCallback, XTaskQueueTerminate, lock,
};
use super::super::vtable::{OBJECT, This};
use super::super::E_ABORT;
use super::slot;

static RAN: AtomicU32 = AtomicU32::new(0);
extern "system" fn count(context: *mut c_void, canceled: bool) {
    RAN.fetch_add(
        if canceled { 100 } else { context as u32 },
        Ordering::SeqCst,
    );
}

#[test]
fn manual_ports_run_callbacks_when_dispatched() {
    type Create = extern "system" fn(This, u32, u32, *mut *const Queue) -> HRESULT;
    type Submit = extern "system" fn(This, *const Queue, u32, *mut c_void, Callback) -> HRESULT;
    type Dispatch = extern "system" fn(This, *const Queue, u32, u32) -> bool;
    type Close = extern "system" fn(This, *const Queue);
    let this: This = &OBJECT;
    let mut q = std::ptr::null();
    assert_eq!(slot::<Create>(0x60)(this, 0, 0, &mut q), S_OK);
    assert_eq!(
        slot::<Submit>(0x90)(this, q, COMPLETION, 5 as *mut c_void, count),
        S_OK
    );
    assert_eq!(RAN.load(Ordering::SeqCst), 0, "manual: nothing runs yet");
    assert!(
        !slot::<Dispatch>(0x80)(this, q, WORK, 0),
        "work port is empty"
    );
    assert!(slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
    assert_eq!(RAN.load(Ordering::SeqCst), 5);
    assert!(!slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
    slot::<Close>(0x88)(this, q);
}

#[test]
fn pool_callbacks_may_wait_for_each_other() {
    static SECOND: AtomicBool = AtomicBool::new(false);
    static FIRST: AtomicBool = AtomicBool::new(false);
    extern "system" fn waits(_: *mut c_void, _: bool) {
        let start = std::time::Instant::now();
        while !SECOND.load(Ordering::SeqCst) && start.elapsed().as_secs() < 5 {
            std::thread::yield_now();
        }
        FIRST.store(SECOND.load(Ordering::SeqCst), Ordering::SeqCst);
    }
    extern "system" fn releases(_: *mut c_void, _: bool) {
        SECOND.store(true, Ordering::SeqCst);
    }
    let mut q = std::ptr::null();
    assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
    let none = std::ptr::null_mut();
    assert_eq!(
        XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(waits)),
        S_OK
    );
    assert_eq!(
        XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(releases)),
        S_OK
    );
    let start = std::time::Instant::now();
    while !FIRST.load(Ordering::SeqCst) && start.elapsed().as_secs() < 10 {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(FIRST.load(Ordering::SeqCst));
    XTaskQueueCloseHandle(q);
}

/// A dispatcher thread's loop, `while (XTaskQueueDispatch(queue, port,
/// INFINITE));`, ends when its queue is terminated: nothing can be
/// submitted to it any more, so nothing else would ever wake it.
#[test]
fn terminate_ends_a_waiting_dispatch() {
    static RAN: AtomicU32 = AtomicU32::new(0);
    extern "system" fn ran(_: *mut c_void, _: bool) {
        RAN.fetch_add(1, Ordering::SeqCst);
    }
    /// Dispatches `queue`'s work until a dispatch returns false, then
    /// says so.
    fn dispatcher(queue: *const Queue) -> std::sync::mpsc::Receiver<()> {
        let (done, ended) = std::sync::mpsc::channel();
        let queue = queue as usize;
        std::thread::spawn(move || {
            while XTaskQueueDispatch(queue as *const Queue, WORK, INFINITE) {}
            let _ = done.send(());
        });
        ended
    }
    let run_one = |queue, total| {
        let none = std::ptr::null_mut();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(queue, WORK, 0, none, Some(ran)),
            S_OK
        );
        for _ in 0..500 {
            if RAN.load(Ordering::SeqCst) == total {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("callback {total} did not run");
    };
    let (long, short) = (Duration::from_secs(5), Duration::from_millis(200));
    let none = std::ptr::null_mut();

    // Two queues on the same manual ports.
    let (mut a, mut b) = (std::ptr::null(), std::ptr::null());
    assert_eq!(XTaskQueueCreate(0, 0, &mut a), S_OK);
    let (mut work, mut completion) = (std::ptr::null(), std::ptr::null());
    assert_eq!(XTaskQueueGetPort(a, WORK, &mut work), S_OK);
    assert_eq!(XTaskQueueGetPort(a, COMPLETION, &mut completion), S_OK);
    assert_eq!(XTaskQueueCreateComposite(work, completion, &mut b), S_OK);

    let ended = dispatcher(a);
    run_one(a, 1);
    // The other queue's end is not this one's: the wait goes on and
    // still runs what comes in.
    assert_eq!(XTaskQueueTerminate(b, false, none, None), S_OK);
    assert!(
        ended.recv_timeout(short).is_err(),
        "ended with another queue"
    );
    run_one(a, 2);
    // Its own queue's end is.
    assert_eq!(XTaskQueueTerminate(a, false, none, None), S_OK);
    assert!(
        ended.recv_timeout(long).is_ok(),
        "still waiting on a terminated queue"
    );
    // And a dispatch that starts after the end returns at once.
    assert!(!XTaskQueueDispatch(a, WORK, INFINITE));
    XTaskQueueCloseHandle(a);
    XTaskQueueCloseHandle(b);
}

#[test]
fn terminate_cancels_what_is_queued() {
    static TERMINATED: AtomicBool = AtomicBool::new(false);
    extern "system" fn terminated(_: *mut c_void) {
        TERMINATED.store(true, Ordering::SeqCst);
    }
    extern "system" fn note(context: *mut c_void, canceled: bool) {
        // SAFETY: the test passes a pointer to its flag.
        unsafe { *context.cast::<bool>() = canceled };
    }
    let mut q = std::ptr::null();
    assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
    let mut canceled = false;
    let flag = (&mut canceled as *mut bool).cast();
    assert_eq!(
        XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
        S_OK
    );
    assert_eq!(
        XTaskQueueTerminate(q, true, std::ptr::null_mut(), Some(terminated)),
        S_OK
    );
    assert!(canceled && TERMINATED.load(Ordering::SeqCst));
    assert_eq!(
        XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
        E_ABORT
    );
    XTaskQueueCloseHandle(q);
}

#[test]
fn terminate_cancels_delayed_callbacks() {
    static EVENTS: Mutex<Vec<&str>> = Mutex::new(Vec::new());
    extern "system" fn delayed(_: *mut c_void, canceled: bool) {
        lock(&EVENTS).push(if canceled { "canceled" } else { "ran" });
    }
    extern "system" fn terminated(_: *mut c_void) {
        lock(&EVENTS).push("terminated");
    }
    let mut q = std::ptr::null();
    assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
    let none = std::ptr::null_mut();
    let hr = XTaskQueueSubmitDelayedCallback(q, WORK, 50, none, Some(delayed));
    assert_eq!(hr, S_OK);
    assert_eq!(XTaskQueueTerminate(q, true, none, Some(terminated)), S_OK);
    assert_eq!(*lock(&EVENTS), ["canceled", "terminated"]);
    // Its delay passes without it running.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(*lock(&EVENTS), ["canceled", "terminated"]);
    XTaskQueueCloseHandle(q);
}
