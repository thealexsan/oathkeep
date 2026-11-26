use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::super::super::xgameruntime::{HRESULT, S_OK};
use super::super::async_call::{
    AsyncBlock, OP_BEGIN, OP_CANCEL, OP_CLEANUP, OP_DO_WORK, OP_GET_RESULT, Provider,
    ProviderData, Work, XAsyncBegin, XAsyncCancel, XAsyncComplete, XAsyncGetResult,
    XAsyncGetResultSize, XAsyncGetStatus, XAsyncSchedule, call, finished_with, result,
};
use super::super::queue::{
    COMPLETION, INFINITE, Queue, WORK, XTaskQueueCloseHandle, XTaskQueueCreate,
    XTaskQueueDispatch,
};
use super::super::vtable::{OBJECT, This};
use super::super::{E_ABORT, E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER, E_PENDING};
use super::slot;

/// A provider that schedules its work and returns a u64 result.
extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
    // SAFETY: the runtime passes valid provider data.
    let data = unsafe { &*data };
    match op {
        OP_BEGIN => XAsyncSchedule(data.block, 0),
        OP_DO_WORK => {
            XAsyncComplete(data.block, S_OK, 8);
            S_OK
        }
        OP_GET_RESULT => {
            // SAFETY: at least 8 bytes, checked by the runtime.
            unsafe { data.buffer.cast::<u64>().write(data.context as u64) };
            S_OK
        }
        _ => S_OK,
    }
}

static COMPLETED: AtomicU32 = AtomicU32::new(0);
extern "system" fn completed(_: *mut AsyncBlock) {
    COMPLETED.fetch_add(1, Ordering::SeqCst);
}

/// A title that never takes a call's result and frees its block: the
/// next block allocated at that address (zeroed, as a new block must
/// be) begins a call like any other.
#[test]
fn a_new_block_where_a_result_was_left_begins_a_call() {
    static CLEANED: AtomicU32 = AtomicU32::new(0);
    /// Completes at once with one byte to fetch; counts its cleanups.
    extern "system" fn left(op: u32, data: *const ProviderData) -> HRESULT {
        match op {
            // SAFETY: the runtime passes valid provider data.
            OP_BEGIN => XAsyncComplete(unsafe { (*data).block }, S_OK, 1),
            OP_CLEANUP => drop(CLEANED.fetch_add(1, Ordering::SeqCst)),
            _ => {}
        }
        S_OK
    }
    /// Never completes by itself.
    extern "system" fn idle(_: u32, _: *const ProviderData) -> HRESULT {
        S_OK
    }
    let fresh = || AsyncBlock {
        queue: std::ptr::null(),
        context: std::ptr::null_mut(),
        callback: None,
        internal: [0; 4],
    };
    let (none, identity) = (std::ptr::null_mut(), 0x1e as *const c_void);
    let begin = |block: &mut AsyncBlock, provider: Provider| {
        XAsyncBegin(block, none, identity, std::ptr::null(), Some(provider))
    };

    let mut block = fresh();
    assert_eq!(begin(&mut block, left), S_OK);
    assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
    // The block as the call left it is still that call's.
    assert_eq!(begin(&mut block, left), E_INVALIDARG);
    assert_eq!(CLEANED.load(Ordering::SeqCst), 0);

    // Freed with its result never taken, and a new block in its place.
    block = fresh();
    assert_eq!(finished_with(&mut block, identity, Ok(vec![9])), S_OK);
    assert_eq!(CLEANED.load(Ordering::SeqCst), 1, "the call left behind");
    let mut value = 0u8;
    assert_eq!(result(&mut block, identity, &mut value), S_OK);
    assert_eq!(value, 9);
    assert!(call(&mut block).is_none());

    // A call still in flight keeps its address, whatever the block holds.
    let mut block = fresh();
    assert_eq!(begin(&mut block, idle), S_OK);
    block = fresh();
    assert_eq!(begin(&mut block, left), E_INVALIDARG);
    XAsyncComplete(&mut block, E_ABORT, 0);
    assert!(call(&mut block).is_none());
}

#[test]
fn async_calls_complete_and_return_results() {
    let mut q = std::ptr::null();
    // Work on a thread pool, completion dispatched by hand.
    assert_eq!(XTaskQueueCreate(1, 0, &mut q), S_OK);
    let mut block = AsyncBlock {
        queue: q,
        context: std::ptr::null_mut(),
        callback: Some(completed),
        internal: [0; 4],
    };
    let identity = 0x1d as *const c_void;
    assert_eq!(
        XAsyncBegin(
            &mut block,
            42 as *mut c_void,
            identity,
            std::ptr::null(),
            Some(provider)
        ),
        S_OK
    );
    assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
    let mut size = 0;
    assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_OK);
    assert_eq!(size, 8);
    assert_eq!(COMPLETED.load(Ordering::SeqCst), 0, "completion is manual");
    assert!(XTaskQueueDispatch(q, COMPLETION, INFINITE));
    assert_eq!(COMPLETED.load(Ordering::SeqCst), 1);

    let mut value = 0u64;
    let mut small = 0u32;
    let out = (&mut small as *mut u32).cast();
    assert_eq!(
        XAsyncGetResult(&mut block, identity, 4, out, std::ptr::null_mut()),
        E_NOT_SUFFICIENT_BUFFER
    );
    let out = (&mut value as *mut u64).cast();
    let mut used = 0;
    assert_eq!(
        XAsyncGetResult(&mut block, identity, 8, out, &mut used),
        S_OK
    );
    assert_eq!((value, used), (42, 8));
    assert!(call(&mut block).is_none(), "the call ends with its result");
    XTaskQueueCloseHandle(q);
}

/// What a test sees of a block that a title reuses: its completion
/// routine begins the next call on the same block.
#[derive(Default)]
struct Reuse {
    completions: AtomicU32,
    next_begun: AtomicBool,
    next_worked: AtomicBool,
}

/// A block whose completion routine is [`begin_next`].
fn reused_block(queue: *const Queue, reuse: &Reuse) -> AsyncBlock {
    AsyncBlock {
        queue,
        context: std::ptr::from_ref(reuse).cast_mut().cast(),
        callback: Some(begin_next),
        internal: [0; 4],
    }
}

/// Begins a call without an identity.
fn begin(block: *mut AsyncBlock, context: *mut c_void, provider: Provider) -> HRESULT {
    let none = std::ptr::null();
    XAsyncBegin(block, context, none, none.cast(), Some(provider))
}

/// Begins the next call on the block the first time a call completes,
/// as a title that keeps one block for repeated calls does.
extern "system" fn begin_next(block: *mut AsyncBlock) {
    // SAFETY: the block is from `reused_block`, its `Reuse` still alive.
    let context = unsafe { (*block).context };
    // SAFETY: as above.
    let reuse = unsafe { &*context.cast::<Reuse>() };
    if reuse.completions.fetch_add(1, Ordering::SeqCst) == 0 {
        let hr = begin(block, context, next_call);
        reuse.next_begun.store(hr == S_OK, Ordering::SeqCst);
    }
}

/// The next call's provider: it never ends on its own, and notes being
/// asked to work.
extern "system" fn next_call(op: u32, data: *const ProviderData) -> HRESULT {
    // SAFETY: the runtime passes valid provider data; its context is
    // the test's `Reuse`.
    let reuse = unsafe { &*(*data).context.cast::<Reuse>() };
    if op == OP_DO_WORK {
        reuse.next_worked.store(true, Ordering::SeqCst);
        return E_PENDING;
    }
    S_OK
}

/// A provider that ends its call itself, in `DoWork` and on cancel, then
/// returns `S_OK`, as providers do.
extern "system" fn ends_itself(op: u32, data: *const ProviderData) -> HRESULT {
    // SAFETY: the runtime passes valid provider data.
    let block = unsafe { (*data).block };
    match op {
        OP_DO_WORK => XAsyncComplete(block, S_OK, 0),
        OP_CANCEL => XAsyncComplete(block, E_ABORT, 0),
        _ => {}
    }
    S_OK
}

#[test]
fn a_finished_calls_work_result_leaves_the_next_call_alone() {
    let mut q = std::ptr::null();
    // Work dispatched by hand; completion routines run where the call
    // completes, inside the provider's `DoWork`.
    assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
    let reuse = Reuse::default();
    let mut block = reused_block(q, &reuse);
    assert_eq!(begin(&mut block, std::ptr::null_mut(), ends_itself), S_OK);
    assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
    assert!(XTaskQueueDispatch(q, WORK, 0));
    assert!(reuse.next_begun.load(Ordering::SeqCst));
    assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
    assert_eq!(reuse.completions.load(Ordering::SeqCst), 1);

    XAsyncCancel(&mut block);
    assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
    XTaskQueueCloseHandle(q);
}

#[test]
fn a_canceled_call_leaves_the_next_call_alone() {
    let mut q = std::ptr::null();
    assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
    let reuse = Reuse::default();
    let mut block = reused_block(q, &reuse);
    assert_eq!(begin(&mut block, std::ptr::null_mut(), ends_itself), S_OK);
    assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
    // The provider ends the call on cancel, before its work has run.
    XAsyncCancel(&mut block);
    assert!(reuse.next_begun.load(Ordering::SeqCst));
    assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
    // The canceled call's work is still queued; it runs nothing now.
    assert!(XTaskQueueDispatch(q, WORK, 0));
    assert!(!reuse.next_worked.load(Ordering::SeqCst));
    assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
    assert_eq!(reuse.completions.load(Ordering::SeqCst), 1);

    XAsyncCancel(&mut block);
    assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
    XTaskQueueCloseHandle(q);
}

#[test]
fn a_nonnegative_completion_still_returns_its_payload() {
    const S_FALSE: HRESULT = 1;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                XAsyncComplete(data.block, S_FALSE, 8);
                S_OK
            }
            OP_GET_RESULT => {
                // SAFETY: the caller checked eight bytes.
                unsafe { data.buffer.cast::<u64>().write(0x1234) };
                S_OK
            }
            _ => S_OK,
        }
    }
    let mut q = std::ptr::null();
    assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
    let mut block = AsyncBlock {
        queue: q,
        context: std::ptr::null_mut(),
        callback: None,
        internal: [0; 4],
    };
    let identity = 0x77 as *const c_void;
    assert_eq!(
        XAsyncBegin(&mut block, std::ptr::null_mut(), identity, std::ptr::null(), Some(provider)),
        S_OK
    );
    assert!(XTaskQueueDispatch(q, WORK, 0));
    assert_eq!(XAsyncGetStatus(&mut block, false), S_FALSE);
    let mut size = 0;
    assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_FALSE);
    assert_eq!(size, 8);
    let mut value = 0u64;
    let mut used = 0;
    assert_eq!(
        XAsyncGetResult(&mut block, identity, 8, (&mut value as *mut u64).cast(), &mut used),
        S_OK
    );
    assert_eq!((value, used), (0x1234, 8));
    assert!(call(&raw mut block).is_none(), "the call ends with its result");
    XTaskQueueCloseHandle(q);
}

#[test]
fn canceling_a_completed_call_leaves_it_alone() {
    static CANCELS: AtomicU32 = AtomicU32::new(0);
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => {
                XAsyncComplete(data.block, S_OK, 1);
                S_OK
            }
            OP_CANCEL => {
                CANCELS.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            OP_GET_RESULT => {
                // SAFETY: the runtime checked one byte.
                unsafe { data.buffer.cast::<u8>().write(0x5a) };
                S_OK
            }
            _ => S_OK,
        }
    }
    let mut q = std::ptr::null();
    assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
    let mut block = AsyncBlock {
        queue: q,
        context: std::ptr::null_mut(),
        callback: None,
        internal: [0; 4],
    };
    let identity = 0x78 as *const c_void;
    assert_eq!(
        XAsyncBegin(
            &mut block,
            std::ptr::null_mut(),
            identity,
            std::ptr::null(),
            Some(provider),
        ),
        S_OK
    );
    assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
    XAsyncCancel(&mut block);
    assert_eq!(CANCELS.load(Ordering::SeqCst), 0);
    assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
    let mut byte = 0u8;
    let mut used = 0;
    assert_eq!(
        XAsyncGetResult(
            &mut block,
            identity,
            1,
            (&mut byte as *mut u8).cast(),
            &mut used,
        ),
        S_OK
    );
    assert_eq!((byte, used), (0x5a, 1));
    XTaskQueueCloseHandle(q);
}

#[test]
fn run_returns_the_work_result() {
    extern "system" fn work(_: *mut AsyncBlock) -> HRESULT {
        E_ABORT
    }
    let mut block = AsyncBlock {
        queue: std::ptr::null(),
        context: std::ptr::null_mut(),
        callback: None,
        internal: [0; 4],
    };
    assert_eq!(
        slot::<extern "system" fn(This, *mut AsyncBlock, Option<Work>) -> HRESULT>(0x30)(
            &OBJECT,
            &mut block,
            Some(work)
        ),
        S_OK
    );
    assert_eq!(XAsyncGetStatus(&mut block, true), E_ABORT);
}
