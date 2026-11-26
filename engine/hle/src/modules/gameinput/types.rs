use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use super::super::xgameruntime::HRESULT;
use super::objects::{DEVICE, reading_add_ref, reading_release};
use super::hub::Hub;

/// The documented errors.
pub const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
pub const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
pub const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
pub const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;
pub(super) const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;

/// Input kinds (bits) our devices have.
pub(super) const KEYBOARD_KIND: u32 = 0x10;
pub(super) const GAMEPAD_KIND: u32 = 0x4_0000;
/// Device status of a connected device: connected, input enabled.
pub(super) const STATUS_CONNECTED: u32 = 0x3;
pub(super) const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad buttons; the title maps each bit to one of its pad buttons.
pub(super) const MENU: u32 = 0x1;
pub(super) const VIEW: u32 = 0x2;
pub(super) const A: u32 = 0x4;
pub(super) const B: u32 = 0x8;
pub(super) const X: u32 = 0x10;
pub(super) const Y: u32 = 0x20;
pub(super) const UP: u32 = 0x40;
pub(super) const DOWN: u32 = 0x80;
pub(super) const LEFT: u32 = 0x100;
pub(super) const RIGHT: u32 = 0x200;
pub(super) const LEFT_SHOULDER: u32 = 0x400;
pub(super) const RIGHT_SHOULDER: u32 = 0x800;
pub(super) const LEFT_STICK: u32 = 0x1000;
pub(super) const RIGHT_STICK: u32 = 0x2000;

/// Devices: host pad slots, the keyboard's stand-in pad, the keyboard.
pub(super) const PADS: usize = 4;
pub(super) const FALLBACK: usize = PADS;
pub(super) const KEYBOARD: usize = PADS + 1;
pub(super) const DEVICES: usize = PADS + 2;

/// Readings kept per device for next/previous lookups.
pub(super) const HISTORY: usize = 256;
pub(super) const POLL: Duration = Duration::from_millis(8);

/// The keyboard's stand-in gamepad (virtual key, button); README lists it.
/// Sticks: W/A/S/D left, I/J/K/L right; triggers Z and C.
pub(super) const FALLBACK_BUTTONS: [(u8, u32); 14] = [
    (0x1B, MENU), // Esc
    (0x09, VIEW), // Tab
    (0x0D, A),    // Enter
    (0x08, B),    // Backspace
    (b'X', X),
    (b'Y', Y),
    (0x26, UP), // arrows
    (0x28, DOWN),
    (0x25, LEFT),
    (0x27, RIGHT),
    (b'Q', LEFT_SHOULDER),
    (b'E', RIGHT_SHOULDER),
    (b'F', LEFT_STICK),
    (b'H', RIGHT_STICK),
];

/// A gamepad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct PadState {
    pub(super) buttons: u32,
    pub(super) left_trigger: f32,
    pub(super) right_trigger: f32,
    pub(super) left_x: f32,
    pub(super) left_y: f32,
    pub(super) right_x: f32,
    pub(super) right_y: f32,
}

/// One key held down: scan code (0xE0xx when extended), character, key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct KeyState {
    pub(super) scan_code: u32,
    pub(super) code_point: u32,
    pub(super) virtual_key: u8,
    pub(super) dead_key: bool,
}

const _: () = assert!(size_of::<KeyState>() == 12 && size_of::<PadState>() == 28);

/// Host input, or a fake in tests.
pub(super) trait Source: Send {
    /// The pad in host slot `slot`, `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down; none while the title's window lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    /// Motor speeds: low frequency (left), high frequency (right).
    fn rumble(&mut self, slot: usize, low: u16, high: u16);
}

pub(super) fn fallback(keys: &[KeyState]) -> PadState {
    let on = |vk: u8| {
        if keys.iter().any(|k| k.virtual_key == vk) {
            1.0
        } else {
            0.0
        }
    };
    PadState {
        buttons: FALLBACK_BUTTONS
            .iter()
            .filter(|(vk, _)| on(*vk) != 0.0)
            .fold(0, |all, (_, bit)| all | bit),
        left_trigger: on(b'Z'),
        right_trigger: on(b'C'),
        left_x: on(b'D') - on(b'A'),
        left_y: on(b'W') - on(b'S'),
        right_x: on(b'L') - on(b'J'),
        right_y: on(b'I') - on(b'K'),
    }
}

/// Two pads as one: buttons of both, and of each axis and trigger the one
/// pushed further.
pub(super) fn merged(a: PadState, b: PadState) -> PadState {
    let further = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: further(a.left_trigger, b.left_trigger),
        right_trigger: further(a.right_trigger, b.right_trigger),
        left_x: further(a.left_x, b.left_x),
        left_y: further(a.left_y, b.left_y),
        right_x: further(a.right_x, b.right_x),
        right_y: further(a.right_y, b.right_y),
    }
}

/// Whether the title sees one pad that is always there, fed by the keyboard
/// and by every host pad (on by default; `OATHKEEP_ONE_PAD=0` shows each host
/// pad as a device of its own, and the stand-in only while there is none).
/// The title keeps to the pad it started with: with the stand-in taken away
/// when a pad was switched on, it showed its controller-disconnected screen
/// until the pad was off again. It also keeps a pad the host lists twice (its
/// own slot and a remapper's) from being two players.
pub(super) fn one_pad() -> bool {
    std::env::var_os("OATHKEEP_ONE_PAD").is_none_or(|v| v != "0")
}

#[repr(C)]
pub(super) struct Table<const N: usize>(pub(super) [*const (); N]);
// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Table<N> {}

pub(super) type This = *const Object;
pub(super) type Out = *mut *const c_void;

/// The input object; one per hub.
#[repr(C)]
pub struct Object {
    pub(super) vtable: &'static Table<22>,
    pub(super) hub: *const Hub,
}

#[repr(C)]
pub struct Device {
    vtable: &'static Table<21>,
    hub: *const Hub,
    pub(super) index: usize,
    pub(super) info: DeviceInfo,
}

/// What a device says about itself. The title reads `id` (+0x20); the rest
/// follows the public layout and is zero where we have nothing to say.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by title code
pub(super) struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    pub(super) id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers.
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

pub(super) fn kind(index: usize) -> u32 {
    if index == KEYBOARD {
        KEYBOARD_KIND
    } else {
        GAMEPAD_KIND
    }
}

impl Device {
    pub(super) fn new(index: usize) -> Device {
        let mut id = [0; 32];
        id[..8].copy_from_slice(b"oathkeep");
        id[31] = index as u8 + 1;
        let info = DeviceInfo {
            size: size_of::<DeviceInfo>() as u32,
            id,
            root_id: id,
            supported_input: kind(index),
            // Low and high frequency.
            rumble_motors: if index <= FALLBACK { 0x3 } else { 0 },
            ..Default::default()
        };
        Device {
            vtable: &DEVICE,
            hub: std::ptr::null(),
            index,
            info,
        }
    }

    pub(super) fn set_hub(&mut self, hub: *const Hub) {
        self.hub = hub;
    }

    pub(super) fn hub(&self) -> &'static Hub {
        // SAFETY: set by `Hub::new`; hubs are never freed.
        unsafe { &*self.hub }
    }
}

#[derive(PartialEq)]
pub(super) enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
}

#[repr(C)]
pub struct Reading {
    pub(super) vtable: &'static Table<25>,
    pub(super) refs: AtomicU32,
    pub(super) device: *const Device,
    pub(super) sequence: u64,
    pub(super) timestamp: u64,
    pub(super) value: Value,
}

impl Reading {
    pub(super) fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => GAMEPAD_KIND,
            Value::Keys(_) => KEYBOARD_KIND,
        }
    }
}

/// A counted reference to a reading.
pub(super) struct Ref(NonNull<Reading>);

impl Ref {
    pub(super) fn new(reading: Reading) -> Ref {
        Ref(NonNull::from(Box::leak(Box::new(reading))))
    }

    /// Another reference, for the title.
    pub(super) fn share(&self) -> *const Reading {
        reading_add_ref(self.0.as_ptr());
        self.0.as_ptr()
    }
}

impl Deref for Ref {
    type Target = Reading;
    fn deref(&self) -> &Reading {
        // SAFETY: we hold a reference, so it is alive.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0.as_ptr());
    }
}

pub(super) type DeviceCallback = extern "system" fn(u64, *mut c_void, *const Device, u64, u32, u32);

#[derive(Clone, Copy)]
pub(super) struct Registration {
    pub(super) token: u64,
    /// One device's address, or 0 for any.
    pub(super) device: usize,
    pub(super) kind: u32,
    pub(super) status: u32,
    pub(super) context: usize,
    pub(super) callback: DeviceCallback,
}

impl Registration {
    pub(super) fn matches(&self, devices: &[Device; DEVICES], index: usize) -> bool {
        kind(index) & self.kind != 0
            && (self.device == 0 || self.device == &devices[index] as *const Device as usize)
    }

    pub(super) fn event(&self, device: usize, timestamp: u64, current: u32, previous: u32) -> Event {
        Event {
            registration: *self,
            device,
            timestamp,
            current,
            previous,
        }
    }
}

pub(super) struct Event {
    pub(super) registration: Registration,
    pub(super) device: usize,
    pub(super) timestamp: u64,
    pub(super) current: u32,
    pub(super) previous: u32,
}
