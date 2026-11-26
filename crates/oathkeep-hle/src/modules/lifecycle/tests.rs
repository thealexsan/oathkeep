use super::window::{TITLE_PROC, fit_into, fit_with_limit, parse_window_limit};
use super::*;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;
use windows_sys::core::w;

extern "system" fn noop(_: u8, _: *mut c_void) {}

#[test]
fn registrations_come_and_go() {
    let (mut a, mut b) = (0, 0);
    assert_eq!(
        RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut a),
        0
    );
    assert_eq!(
        RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut b),
        0
    );
    UnregisterAppStateChangeNotification(a);
    let left: Vec<_> = routines().iter().map(|r| r.0).collect();
    assert_eq!(left, [b]);
}

#[test]
fn constrained_registrations_come_and_go() {
    let (mut a, mut b) = (0, 0);
    assert_eq!(
        RegisterAppConstrainedChangeNotification(Some(noop), std::ptr::null_mut(), &mut a),
        0
    );
    assert_eq!(
        RegisterAppConstrainedChangeNotification(Some(noop), std::ptr::null_mut(), &mut b),
        0
    );
    assert_eq!(
        RegisterAppConstrainedChangeNotification(None, std::ptr::null_mut(), &mut a),
        ERROR_INVALID_PARAMETER
    );
    UnregisterAppConstrainedChangeNotification(a);
    let left: Vec<_> = constrained().iter().map(|r| r.0).collect();
    assert_eq!(left, [b]);
}

/// What `recording_routine` saw: (context, flag) per call, in order.
static CALLS: Mutex<Vec<(usize, u8)>> = Mutex::new(Vec::new());

extern "system" fn recording_routine(flag: u8, context: *mut c_void) {
    CALLS.lock().unwrap().push((context as usize, flag));
}

#[test]
fn constrained_routines_run_before_suspend_routines() {
    let r: Routine = recording_routine;
    constrain_then_suspend(&[(1, r, 10), (2, r, 11)], &[(3, r, 20), (4, r, 21)]);
    assert_eq!(*CALLS.lock().unwrap(), [(10, 1), (11, 1), (20, 1), (21, 1)]);
}

#[test]
fn oversized_windows_shrink_to_the_screen() {
    let area = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    assert_eq!(fit_into(&area, 3840, 2160), (1848, 1040));
    assert_eq!(fit_into(&area, 1280, 720), (1280, 720));
    assert_eq!(
        fit_into(&area, wm::CW_USEDEFAULT, 0),
        (wm::CW_USEDEFAULT, 0)
    );
}

#[test]
fn configured_window_limit_fits_4k_into_1080p() {
    let area = RECT {
        left: 100,
        top: 50,
        right: 2660,
        bottom: 1490,
    };
    assert_eq!(
        fit_with_limit(&area, 3840, 2160, Some((1920, 1080))),
        (1920, 1080)
    );
    assert_eq!(
        fit_with_limit(&area, 640, 480, Some((1920, 1080))),
        (640, 480)
    );
    assert_eq!(fit_with_limit(&area, 3840, 2160, None), (2560, 1440));
}

#[test]
fn configured_window_limit_still_respects_the_work_area() {
    let area = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    assert_eq!(
        fit_with_limit(&area, 3840, 2160, Some((2560, 1440))),
        (1848, 1040)
    );
    assert_eq!(
        fit_with_limit(&area, wm::CW_USEDEFAULT, 0, Some((1920, 1080))),
        (wm::CW_USEDEFAULT, 0)
    );
}

#[test]
fn configured_window_limit_preserves_non_widescreen_proportions() {
    let area = RECT {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1440,
    };
    assert_eq!(
        fit_with_limit(&area, 1600, 1200, Some((1920, 1080))),
        (1440, 1080)
    );
}

#[test]
fn window_limit_requires_two_positive_dimensions() {
    assert_eq!(parse_window_limit("1920x1080"), Some((1920, 1080)));
    assert_eq!(parse_window_limit(" 1920 X 1080 "), Some((1920, 1080)));
    for bad in [
        "",
        "1920",
        "0x1080",
        "1920x0",
        "-1x1080",
        "1920x-1",
        "1920x1080x2",
        "2147483648x1080",
    ] {
        assert_eq!(parse_window_limit(bad), None, "{bad}");
    }
}

#[test]
fn hooked_windows_still_reach_the_title_procedure() {
    let null = std::ptr::null_mut();
    let hwnd = CreateWindowExW(
        0,
        w!("STATIC"),
        w!("before"),
        0,
        0,
        0,
        10,
        10,
        null,
        null,
        null,
        std::ptr::null(),
    );
    assert!(!hwnd.is_null());
    let mut text = [0u16; 16];
    // SAFETY: our window; the buffer holds 16 characters.
    unsafe {
        wm::SetWindowTextW(hwnd, w!("after"));
        let n = wm::GetWindowTextW(hwnd, text.as_mut_ptr(), 16);
        assert_eq!(String::from_utf16_lossy(&text[..n as usize]), "after");
        assert!(!wm::GetPropW(hwnd, TITLE_PROC).is_null());
        wm::DestroyWindow(hwnd);
    }
}

thread_local! {
    /// The system commands `recording` received, low four bits cleared.
    static COMMANDS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Stands in for a title's procedure: notes system commands and acts
/// on none of them (no menu loop, no minimising).
extern "system" fn recording(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == wm::WM_SYSCOMMAND {
        COMMANDS.with(|c| c.borrow_mut().push(wparam & 0xFFF0));
        return 0;
    }
    // SAFETY: the system's default procedure, called as the system would.
    unsafe { wm::DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[test]
fn the_keyboard_menu_never_reaches_the_title() {
    let null = std::ptr::null_mut();
    let hwnd = CreateWindowExW(
        0,
        w!("STATIC"),
        w!("menu"),
        0,
        0,
        0,
        10,
        10,
        null,
        null,
        null,
        std::ptr::null(),
    );
    assert!(!hwnd.is_null());
    // SAFETY: our window, owned by this thread, so each message is
    // handled before SendMessageW returns. `recording` stands in for the
    // title's procedure and the original is put back before the window
    // is destroyed.
    unsafe {
        let title = wm::GetPropW(hwnd, TITLE_PROC);
        wm::SetPropW(hwnd, TITLE_PROC, recording as *const () as *mut c_void);
        let command = |c: u32, key: u8| {
            wm::SendMessageW(hwnd, wm::WM_SYSCOMMAND, c as usize, LPARAM::from(key))
        };
        command(wm::SC_KEYMENU, 0); // Alt or F10
        command(wm::SC_KEYMENU, b' '); // Alt+Space
        command(wm::SC_KEYMENU | 1, 0); // the low bits are the system's
        command(wm::SC_MINIMIZE, 0);
        wm::SetPropW(hwnd, TITLE_PROC, title);
        wm::DestroyWindow(hwnd);
    }
    let seen = COMMANDS.with(|c| c.borrow().clone());
    assert_eq!(seen, [wm::SC_MINIMIZE as usize]);
}
