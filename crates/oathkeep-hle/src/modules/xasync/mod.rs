//! XTaskQueue and XAsync: the runtime's callback queues and async-call
//! blocks, served as the task-queue provider's object from `QueryApiImpl`.
//!
//! As documented for the public API: a queue has a work port and a
//! completion port, each dispatched manually (the title calls dispatch),
//! on a thread pool, serialised on one thread, or immediately on the
//! submitting thread. A null queue handle means the process queue. An
//! async block's system area holds its status and result size, so both
//! stay readable after our state for the call is gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod async_call;
mod queue;
#[cfg(test)]
mod tests;
mod vtable;

use super::xgameruntime::{Guid, HRESULT};

/// The task-queue provider; it is also the interface id titles ask for.
pub const PROVIDER: Guid = Guid {
    data1: 0x073b_7dcb,
    data2: 0x1fcf,
    data3: 0x4030,
    data4: [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28],
};

pub const E_PENDING: HRESULT = 0x8000_000A_u32 as i32;
pub const E_ABORT: HRESULT = 0x8000_4004_u32 as i32;
pub const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
pub const E_NOT_SUFFICIENT_BUFFER: HRESULT = 0x8007_007A_u32 as i32;
pub const E_ILLEGAL_METHOD_CALL: HRESULT = 0x8000_000E_u32 as i32;

pub use async_call::{
    AsyncBlock, ProviderData, XAsyncBegin, XAsyncCancel, XAsyncComplete, XAsyncGetResult,
    XAsyncGetResultSize, XAsyncGetStatus, XAsyncRun, XAsyncSchedule, bytes, finished,
    finished_with, pending_with, result,
};
pub use queue::{
    Queue, XTaskQueueCloseHandle, XTaskQueueCreate, XTaskQueueCreateComposite, XTaskQueueDispatch,
    XTaskQueueDuplicateHandle, XTaskQueueGetCurrentProcessTaskQueue, XTaskQueueGetPort,
    XTaskQueueRegisterMonitor, XTaskQueueSetCurrentProcessTaskQueue,
    XTaskQueueSubmitDelayedCallback, XTaskQueueTerminate, XTaskQueueUnregisterMonitor,
};
pub use vtable::{INTERFACES, OBJECT};
