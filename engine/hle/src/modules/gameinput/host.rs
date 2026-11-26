//! Host input: XInput pads, the keyboard while our process has focus.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
};
use windows_sys::Win32::UI::Input::XboxController::{
    XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
    XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
    XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
    XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
    XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState,
    XInputSetState,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId,
};

use super::types::{
    A, B, DOWN, FALLBACK_BUTTONS, KeyState, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PADS, PadState,
    RIGHT, RIGHT_SHOULDER, RIGHT_STICK, Source, UP, VIEW, X, Y,
};

/// Asking XInput about an empty slot is slow; look again this often.
const EMPTY_SLOT_RECHECK: Duration = Duration::from_secs(1);

const BUTTONS: [(u16, u32); 14] = [
    (XINPUT_GAMEPAD_START, MENU),
    (XINPUT_GAMEPAD_BACK, VIEW),
    (XINPUT_GAMEPAD_A, A),
    (XINPUT_GAMEPAD_B, B),
    (XINPUT_GAMEPAD_X, X),
    (XINPUT_GAMEPAD_Y, Y),
    (XINPUT_GAMEPAD_DPAD_UP, UP),
    (XINPUT_GAMEPAD_DPAD_DOWN, DOWN),
    (XINPUT_GAMEPAD_DPAD_LEFT, LEFT),
    (XINPUT_GAMEPAD_DPAD_RIGHT, RIGHT),
    (XINPUT_GAMEPAD_LEFT_SHOULDER, LEFT_SHOULDER),
    (XINPUT_GAMEPAD_RIGHT_SHOULDER, RIGHT_SHOULDER),
    (XINPUT_GAMEPAD_LEFT_THUMB, LEFT_STICK),
    (XINPUT_GAMEPAD_RIGHT_THUMB, RIGHT_STICK),
];

/// Raw values, no dead zone: titles apply their own.
pub(super) fn convert(pad: &XINPUT_GAMEPAD) -> PadState {
    let stick = |v: i16| (f32::from(v) / 32767.0).max(-1.0);
    PadState {
        buttons: BUTTONS
            .iter()
            .filter(|(host, _)| pad.wButtons & host != 0)
            .fold(0, |all, (_, bit)| all | bit),
        left_trigger: f32::from(pad.bLeftTrigger) / 255.0,
        right_trigger: f32::from(pad.bRightTrigger) / 255.0,
        left_x: stick(pad.sThumbLX),
        left_y: stick(pad.sThumbLY),
        right_x: stick(pad.sThumbRX),
        right_y: stick(pad.sThumbRY),
    }
}

#[derive(Default)]
pub(super) struct Host {
    empty_until: [Option<Instant>; PADS],
}

impl Source for Host {
    fn pad(&mut self, slot: usize) -> Option<PadState> {
        if self.empty_until[slot].is_some_and(|until| Instant::now() < until) {
            return None;
        }
        let mut state = XINPUT_STATE::default();
        // SAFETY: room for the state.
        if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
            self.empty_until[slot] = Some(Instant::now() + EMPTY_SLOT_RECHECK);
            return None;
        }
        self.empty_until[slot] = None;
        #[allow(unused_mut)]
        let mut pad = convert(&state.Gamepad);
        // The scripted keys (`OATHKEEP_KEYS`) also press the first
        // pad's buttons, by the stand-in pad's table: with a real pad
        // connected the stand-in is not there, and a title that listens
        // to the pad (the title's start screen) saw nothing.
        if slot == 0 {
            for vk in scripted() {
                if let Some(&(_, button)) = FALLBACK_BUTTONS.iter().find(|(key, _)| *key == vk) {
                    pad.buttons |= button;
                }
            }
        }
        Some(pad)
    }

    fn keys(&mut self) -> Vec<KeyState> {
        let mut down = scripted();
        if focused() {
            // Left/right modifier keys only, as devices report them; no
            // mouse buttons (below 0x08).
            let held: Vec<u8> = (0x08..=0xFE_u8)
                .filter(|vk| !(0x10..=0x12).contains(vk) && !down.contains(vk))
                // SAFETY: plain queries.
                .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
                .collect();
            down.extend(held);
        }
        down.into_iter()
            .map(|vk| KeyState {
                // SAFETY: plain query.
                scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                virtual_key: vk,
                // shortcut: no characters (code point 0);
                // the title reads scan codes and virtual keys only.
                ..Default::default()
            })
            .collect()
    }

    fn rumble(&mut self, slot: usize, low: u16, high: u16) {
        let speeds = XINPUT_VIBRATION {
            wLeftMotorSpeed: low,
            wRightMotorSpeed: high,
        };
        // SAFETY: a valid vibration; an empty slot just fails.
        unsafe { XInputSetState(slot as u32, &speeds) };
    }
}

/// Debugging aid: `OATHKEEP_KEYS=<seconds>:<key>,...` holds each key for
/// 0.2 s that long after input starts (`<from>-<to>:<key>`: all that
/// time), with or without focus. A key is a
/// letter, a digit, or `Enter`, `Esc`, `Tab`, `Back`, `Up`, `Down`,
/// `Left`, `Right` (the stand-in pad's table maps them to buttons).
fn scripted() -> Vec<u8> {
    static SCRIPT: LazyLock<(Instant, Vec<(Duration, Duration, u8)>)> = LazyLock::new(|| {
        let list = std::env::var("OATHKEEP_KEYS").unwrap_or_default();
        let keys = list.split(',').filter_map(|entry| {
            let (at, key) = entry.split_once(':')?;
            let vk = match key.trim() {
                "Enter" => 0x0D,
                "Esc" => 0x1B,
                "Tab" => 0x09,
                "Back" => 0x08,
                "Up" => 0x26,
                "Down" => 0x28,
                "Left" => 0x25,
                "Right" => 0x27,
                k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
                    k.as_bytes()[0].to_ascii_uppercase()
                }
                _ => return None,
            };
            let time = |t: &str| Duration::try_from_secs_f64(t.trim().parse().ok()?).ok();
            let (from, to) = match at.split_once('-') {
                Some((from, to)) => (time(from)?, time(to)?),
                None => (time(at)?, time(at)? + Duration::from_millis(200)),
            };
            Some((from, to, vk))
        });
        (Instant::now(), keys.collect())
    });
    let (start, keys) = &*SCRIPT;
    let now = start.elapsed();
    keys.iter()
        .filter(|&&(from, to, _)| (from..to).contains(&now))
        .map(|&(_, _, vk)| vk)
        .collect()
}

/// The foreground window is one of ours (the title's or our fallback).
fn focused() -> bool {
    let mut process = 0;
    // SAFETY: plain queries; a null window gives process 0.
    unsafe {
        GetWindowThreadProcessId(GetForegroundWindow(), &mut process);
        process == GetCurrentProcessId()
    }
}

#[test]
fn host_pads_convert_to_console_pads() {
    let pad = XINPUT_GAMEPAD {
        wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_DPAD_LEFT,
        bLeftTrigger: 255,
        sThumbLX: -32768,
        sThumbLY: 32767,
        ..Default::default()
    };
    let state = convert(&pad);
    assert_eq!(state.buttons, A | MENU | LEFT);
    assert_eq!(
        (
            state.left_trigger,
            state.left_x,
            state.left_y,
            state.right_x
        ),
        (1.0, -1.0, 1.0, 0.0)
    );
}
