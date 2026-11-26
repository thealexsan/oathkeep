//! `gameinput.dll`: input devices from the host. Gamepads come from XInput
//! (host slots 0-3); while none is connected, the keyboard drives one
//! stand-in gamepad (`FALLBACK_BUTTONS`). The keyboard is a device of its own
//! too. A poll thread turns host state into readings (a new one per change)
//! and runs device callbacks on connect and disconnect.
#![allow(non_snake_case)]
// Title code calls these through the objects, passing pointers the real API
// would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

#[cfg(windows)]
mod host;
mod hub;
mod input;
mod objects;
#[cfg(test)]
mod tests;
mod types;

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleExport, HleModule};

pub use hub::Hub;
pub use types::{
    DEVICE_DISCONNECTED, DEVICE_NOT_FOUND, Device, Object, READING_NOT_FOUND, REFERENCE_TOO_OLD,
    Reading,
};

use hub::HUB;
use types::This;

/// The DLL's only export (ordinal 1): the input object, static and shared.
pub extern "system" fn GameInputCreate(out: *mut This) -> HRESULT {
    tracing::info!("GameInputCreate");
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a pointer.
    unsafe { out.write(&HUB.object) };
    S_OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "gameinput",
        dlls: &["gameinput.dll"],
        // Titles import it by ordinal only.
        exports: vec![HleExport {
            name: "#1",
            address: GameInputCreate as *const () as usize,
        }],
    }
}
