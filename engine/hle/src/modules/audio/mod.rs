//! The console's audio hardware (`AcpHal.dll`):
//!
//! * the APU heap: host memory with 32-bit device addresses;
//! * SHAPE context arrays allocated from it;
//! * the audio coprocessor's command and message queues, where every
//!   command completes at once and is acknowledged as the hardware would;
//! * the XMA decoder: enabled XMA contexts are decoded on a thread of ours
//!   into their output rings, like the hardware does.
//!
//! SHAPE's other units (rate conversion, EQ, filters, mixing through
//! flowgraphs) are not emulated, so voices that need them stay silent.
//! Mixed audio leaves through the host's WASAPI, which titles and
//! middleware reach through COM like on PC. fmod's console build sets up
//! the coprocessor inside `System_Create` and waits there for its first
//! command to be acknowledged; the title uses only XMA contexts.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::{Mutex, MutexGuard};

use super::xgameruntime::HRESULT;
use crate::{HleModule, hle_export};

#[cfg(all(windows, feature = "diagnostics"))]
pub mod capture;

mod acp;
mod heap;
mod xma;
#[cfg(test)]
mod tests;

pub use acp::{Acp, AcpHalCreate};
pub use heap::{
    AcpHalAllocateShapeContexts, AcpHalReleaseShapeContexts, ApuAlloc, ApuCreateHeap, ApuFree,
    ApuIsVirtualAddressValid, ApuMapApuAddress, ApuMapVirtualAddress, ShapeContexts,
};

const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "audio",
        dlls: &["acphal.dll"],
        exports: vec![
            hle_export!(AcpHalAllocateShapeContexts),
            hle_export!(AcpHalReleaseShapeContexts),
            hle_export!(AcpHalCreate),
            hle_export!(ApuAlloc),
            hle_export!(ApuCreateHeap),
            hle_export!(ApuFree),
            hle_export!(ApuIsVirtualAddressValid),
            hle_export!(ApuMapApuAddress),
            hle_export!(ApuMapVirtualAddress),
        ],
    }
}
