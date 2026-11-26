use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};

use super::super::xgameruntime::{HRESULT, S_OK};
use super::queue::{COMPLETION, Queue, Task, WORK, lock, queue};
use super::{E_ABORT, E_ILLEGAL_METHOD_CALL, E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER, E_PENDING};

pub(super) type CompletionRoutine = extern "system" fn(*mut AsyncBlock);
pub(super) type Provider = extern "system" fn(u32, *const ProviderData) -> HRESULT;

/// An async block: what the title fills in, then the system area.
#[repr(C)]
pub struct AsyncBlock {
    pub queue: *const Queue,
    pub context: *mut c_void,
    pub callback: Option<CompletionRoutine>,
    /// Ours: [unused, status, result size, unused].
    pub internal: [usize; 4],
}

#[repr(C)]
pub struct ProviderData {
    pub block: *mut AsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

pub(super) const OP_BEGIN: u32 = 0;
pub(super) const OP_DO_WORK: u32 = 1;
pub(super) const OP_GET_RESULT: u32 = 2;
pub(super) const OP_CANCEL: u32 = 3;
pub(super) const OP_CLEANUP: u32 = 4;

/// One call in flight, until its result is taken or it fails.
pub(super) struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    done: Mutex<bool>,
    finished: Condvar,
}

impl Call {
    fn provider(
        &self,
        op: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> HRESULT {
        let data = ProviderData {
            block,
            buffer_size: size,
            buffer,
            context: self.context as *mut c_void,
        };
        (self.provider)(op, &data)
    }
}

/// Calls by block address.
static CALLS: LazyLock<Mutex<HashMap<usize, Arc<Call>>>> = LazyLock::new(Default::default);

pub(super) fn call(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    lock(&CALLS).get(&(block as usize)).cloned()
}

/// Whether `call` is still the call in flight on `block`: once it is over,
/// the title may begin another call on the same block.
fn is_current(call: &Arc<Call>, block: *mut AsyncBlock) -> bool {
    lock(&CALLS)
        .get(&(block as usize))
        .is_some_and(|current| Arc::ptr_eq(current, call))
}

/// The block's status and result-size words.
fn words(block: *mut AsyncBlock) -> (&'static AtomicUsize, &'static AtomicUsize) {
    // SAFETY: the title's block outlives the call; its system area is ours.
    unsafe {
        let internal = std::ptr::addr_of_mut!((*block).internal).cast::<AtomicUsize>();
        (&*internal.add(1), &*internal.add(2))
    }
}

fn status(block: *mut AsyncBlock) -> HRESULT {
    words(block).0.load(Ordering::Acquire) as u32 as i32
}

/// Whether the block's system area is all zero, as in a block not yet used.
fn zeroed(block: *mut AsyncBlock) -> bool {
    // SAFETY: the title's block, whose system area is ours to read.
    unsafe { std::ptr::addr_of!((*block).internal).read_volatile() == [0; 4] }
}

fn release(block: *mut AsyncBlock) {
    // Unlocked before the provider runs: it may call back into us.
    let removed = lock(&CALLS).remove(&(block as usize));
    if let Some(call) = removed {
        call.provider(OP_CLEANUP, block, 0, std::ptr::null_mut());
    }
}

pub fn XAsyncBegin(
    block: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    let Some(provider) = provider else {
        return E_INVALIDARG;
    };
    if block.is_null() {
        return E_INVALIDARG;
    }
    if let Some(left) = call(block) {
        // Calls are filed by block address, and one that succeeded stays
        // filed until its result is taken. A title that never takes it and
        // frees the block may get the same address for its next one, which
        // it zeroes as every new block must be; the block of a result left
        // behind never reads zero (it holds the result's size). So this is
        // a new block, and the call left behind ends here. A call in flight
        // keeps its address, and a block still holding a result is in use.
        let over = *lock(&left.done);
        if !(over && zeroed(block)) {
            return E_INVALIDARG;
        }
        release(block);
    }
    let (status, size) = words(block);
    status.store(E_PENDING as u32 as usize, Ordering::Release);
    size.store(0, Ordering::Release);
    let call = Arc::new(Call {
        provider,
        context: context as usize,
        identity: identity as usize,
        done: Mutex::new(false),
        finished: Condvar::new(),
    });
    lock(&CALLS).insert(block as usize, call.clone());
    // A failed begin fails the call, so the completion routine still runs.
    let hr = call.provider(OP_BEGIN, block, 0, std::ptr::null_mut());
    if hr < 0 {
        complete_call(&call, block, hr, 0);
    }
    S_OK
}

/// Queues the provider's `DoWork` on the block's work port. The work
/// belongs to the call that scheduled it: when it runs, that call may be
/// over and the block may hold another.
pub fn XAsyncSchedule(block: *mut AsyncBlock, delay_ms: u32) -> HRESULT {
    struct Work {
        call: Arc<Call>,
        block: *mut AsyncBlock,
    }
    extern "system" fn work(context: *mut c_void, canceled: bool) {
        // SAFETY: boxed in `XAsyncSchedule`; a task runs once.
        let Work { call, block } = *unsafe { Box::from_raw(context.cast::<Work>()) };
        if !is_current(&call, block) {
            return;
        }
        let hr = if canceled {
            E_ABORT
        } else {
            call.provider(OP_DO_WORK, block, 0, std::ptr::null_mut())
        };
        complete_call(&call, block, hr, 0);
    }
    if block.is_null() {
        return E_INVALIDARG;
    }
    let Some(call) = call(block) else {
        return E_INVALIDARG;
    };
    // SAFETY: a block in flight is valid.
    let handle = unsafe { (*block).queue };
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    let context = Box::into_raw(Box::new(Work { call, block }));
    let hr = queue.submit(WORK, delay_ms, Task::new(context.cast(), work));
    if hr < 0 {
        // Refused, so the task never runs.
        // SAFETY: still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Records the result (the first one wins), wakes waiters and queues the
/// completion routine on the block's completion port.
pub fn XAsyncComplete(block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    if let Some(call) = call(block) {
        complete_call(&call, block, result, required_size);
    }
}

/// [`XAsyncComplete`] for `call`, which is or was in flight on `block`. Our
/// own completions name the call they belong to, not the block: if the
/// provider already completed it, the title may have begun another call on
/// the block, which a second complete by block would end.
fn complete_call(call: &Call, block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    extern "system" fn complete(block: *mut c_void, _canceled: bool) {
        let block = block.cast::<AsyncBlock>();
        // SAFETY: read before the status was published; see below.
        if let Some(routine) = unsafe { (*block).callback } {
            routine(block);
        }
    }
    if result == E_PENDING {
        return;
    }
    let mut done = lock(&call.done);
    if *done {
        return;
    }
    // Read before publishing: once the status is set, the title may free
    // the block.
    // SAFETY: a block in flight is valid.
    let (handle, routine) = unsafe { ((*block).queue, (*block).callback) };
    let (status, size) = words(block);
    if result < 0 || required_size == 0 {
        release(block);
    }
    size.store(required_size, Ordering::Release);
    status.store(result as u32 as usize, Ordering::Release);
    *done = true;
    call.finished.notify_all();
    drop(done);
    if routine.is_some() {
        let task = Task::new(block.cast(), complete);
        match queue(handle) {
            Some(queue) if queue.submit(COMPLETION, 0, task) == S_OK => {}
            _ => task.run(true),
        }
    }
}

pub fn XAsyncGetStatus(block: *mut AsyncBlock, wait: bool) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if wait && let Some(call) = call(block) {
        let done = lock(&call.done);
        drop(call.finished.wait_while(done, |d| !*d));
    }
    status(block)
}

pub fn XAsyncGetResultSize(block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    // Any nonnegative completion carries a result, not just S_OK: S_FALSE
    // is a success code too and a provider may return data with it.
    if hr >= 0 {
        // SAFETY: checked non-null.
        unsafe { out.write(words(block).1.load(Ordering::Acquire)) };
    }
    hr
}

/// Asks the provider to cancel; the call fails with `E_ABORT` unless the
/// provider completed it first. A completed call is left alone.
pub fn XAsyncCancel(block: *mut AsyncBlock) {
    if let Some(call) = call(block) {
        if *lock(&call.done) {
            return;
        }
        call.provider(OP_CANCEL, block, 0, std::ptr::null_mut());
        complete_call(&call, block, E_ABORT, 0);
    }
}

/// Has the provider write the result into `buffer`, then ends the call.
pub fn XAsyncGetResult(
    block: *mut AsyncBlock,
    identity: *const c_void,
    buffer_size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    let size = words(block).1.load(Ordering::Acquire);
    if hr < 0 || size == 0 {
        if hr >= 0 && !used.is_null() {
            // SAFETY: checked non-null.
            unsafe { used.write(0) };
        }
        return hr;
    }
    let Some(call) = call(block) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if call.identity != identity as usize {
        return E_INVALIDARG;
    }
    if buffer_size < size || buffer.is_null() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    let hr = call.provider(OP_GET_RESULT, block, buffer_size, buffer);
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(size) };
    }
    release(block);
    hr
}

/// Starts a call on `block` that is already over: it completes with `hr`
/// and no result data, through the block's completion port as usual.
pub fn finished(block: *mut AsyncBlock, identity: *const c_void, hr: HRESULT) -> HRESULT {
    finished_with(
        block,
        identity,
        if hr < 0 { Err(hr) } else { Ok(Vec::new()) },
    )
}

/// Like [`finished`], with the bytes `XAsyncGetResult` hands out for
/// `identity` (an error has none).
pub fn finished_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    result: Result<Vec<u8>, HRESULT>,
) -> HRESULT {
    type Data = Result<Vec<u8>, HRESULT>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // boxed result until cleanup frees it.
        let data = unsafe { &*data };
        let result = data.context.cast::<Data>();
        match op {
            // SAFETY: boxed in `finished_with`; nothing runs after cleanup.
            OP_CLEANUP => drop(unsafe { Box::from_raw(result) }),
            OP_BEGIN => {
                // SAFETY: as above. Copied out: completing may clean up.
                let (hr, len) = match unsafe { &*result } {
                    Ok(bytes) => (S_OK, bytes.len()),
                    Err(hr) => (*hr, 0),
                };
                XAsyncComplete(data.block, hr, len);
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                if let Ok(bytes) = &*result {
                    data.buffer
                        .cast::<u8>()
                        .copy_from(bytes.as_ptr(), bytes.len());
                }
            },
            _ => {}
        }
        S_OK
    }
    let context = Box::into_raw(Box::new(result));
    let hr = XAsyncBegin(
        block,
        context.cast(),
        identity,
        std::ptr::null(),
        Some(provider),
    );
    if hr < 0 {
        // Refused before the call existed, so no cleanup will come.
        // SAFETY: still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Like [`finished_with`], but the result comes from `work`, run on a
/// thread of ours; the call stays pending until it returns (a dialog, say).
pub fn pending_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    work: impl FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
) -> HRESULT {
    type Data = Mutex<(Option<Box<dyn FnOnce() -> Result<Vec<u8>, HRESULT> + Send>>, Vec<u8>)>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // boxed state until cleanup frees it.
        let data = unsafe { &*data };
        let state = data.context.cast::<Data>();
        match op {
            // SAFETY: boxed in `pending_with`; nothing runs after cleanup.
            OP_CLEANUP => drop(unsafe { Box::from_raw(state) }),
            OP_BEGIN => {
                // SAFETY: as above; cleanup waits for completion.
                let work = unsafe { &*state }.lock().unwrap_or_else(|e| e.into_inner()).0.take();
                let (block, state) = (data.block as usize, state as usize);
                std::thread::spawn(move || {
                    let result = work.map_or(Err(E_ABORT), |work| work());
                    let (hr, len) = match &result {
                        Ok(bytes) => (S_OK, bytes.len()),
                        Err(hr) => (*hr, 0),
                    };
                    if let Ok(bytes) = result {
                        // SAFETY: the call is pending, so its state lives.
                        let state = unsafe { &*(state as *const Data) };
                        state.lock().unwrap_or_else(|e| e.into_inner()).1 = bytes;
                    }
                    XAsyncComplete(block as *mut AsyncBlock, hr, len);
                });
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                let state = (*state).lock().unwrap_or_else(|e| e.into_inner());
                data.buffer
                    .cast::<u8>()
                    .copy_from(state.1.as_ptr(), state.1.len());
            },
            _ => {}
        }
        S_OK
    }
    let state: Box<Data> = Box::new(Mutex::new((Some(Box::new(work)), Vec::new())));
    let context = Box::into_raw(state);
    let hr = XAsyncBegin(block, context.cast(), identity, std::ptr::null(), Some(provider));
    if hr < 0 {
        // SAFETY: refused before the call existed; still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// The result of a call begun with [`finished_with`] for `identity`, as a
/// `T` written through `out`.
pub fn result<T>(block: *mut AsyncBlock, identity: *const c_void, out: *mut T) -> HRESULT {
    XAsyncGetResult(
        block,
        identity,
        size_of::<T>(),
        out.cast(),
        std::ptr::null_mut(),
    )
}

/// The bytes of a value, for [`finished_with`].
pub fn bytes<T: Copy>(value: T) -> Vec<u8> {
    // SAFETY: a `Copy` value is plain data; padding bytes are never read.
    unsafe { std::slice::from_raw_parts((&raw const value).cast::<u8>(), size_of::<T>()) }.to_vec()
}

pub(super) type Work = extern "system" fn(*mut AsyncBlock) -> HRESULT;

/// Runs `work` on the block's work port as an async call of its own; its
/// return value is the call's result.
pub fn XAsyncRun(block: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                // SAFETY: the context is the `Work` given to `XAsyncRun`.
                let work = unsafe { std::mem::transmute::<*mut c_void, Work>(data.context) };
                work(data.block)
            }
            _ => S_OK,
        }
    }
    let Some(work) = work else {
        return E_INVALIDARG;
    };
    let identity = XAsyncRun as *const c_void;
    XAsyncBegin(
        block,
        work as *mut c_void,
        identity,
        std::ptr::null(),
        Some(provider),
    )
}
