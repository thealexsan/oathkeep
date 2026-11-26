use super::process::{TitleArgv, title_argv, title_priority};
use super::*;

/// The title's idle and lowest threads run below normal; the others as
/// it sets them.
#[test]
fn no_title_thread_waits_for_idle_processors() {
    let given: Vec<i32> = [-15, -2, -1, 0, 1, 2, 15]
        .into_iter()
        .map(title_priority)
        .collect();
    assert_eq!(given, [-1, -1, -1, 0, 1, 2, 15]);
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};
    std::thread::spawn(|| {
        // SAFETY: plain calls on this thread.
        let thread = unsafe { GetCurrentThread() };
        assert_eq!(SetThreadPriority(thread, -15), 1);
        // SAFETY: as above.
        assert_eq!(unsafe { GetThreadPriority(thread) }, -1);
    })
    .join()
    .unwrap();
}

#[test]
fn title_images_report_their_original_path() {
    let base = 0x7000_0000_0000;
    runtime::set_image_path(base, std::path::Path::new(r"C:\title\game.exe"));
    let module = base as *mut c_void;
    let mut buf = [0xFFFFu16; 32];
    let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32);
    assert_eq!(
        String::from_utf16_lossy(&buf[..n as usize]),
        r"C:\title\game.exe"
    );
    assert_eq!(buf[n as usize], 0);

    // Too small: truncated, NUL-terminated, the buffer size returned.
    let mut small = [0u8; 4];
    assert_eq!(GetModuleFileNameA(module, small.as_mut_ptr(), 4), 4);
    assert_eq!(&small, &[b'C', b':', b'\\', 0]);
}

/// The title's arguments come right after the program, and the list ends
/// in a null pointer; without any there is only the program.
#[test]
fn the_title_gets_its_own_arguments() {
    let read = |made: &TitleArgv| {
        // SAFETY: made above, kept for the whole run.
        unsafe {
            let (count, list) = (*made.count.get() as usize, *made.list.get());
            assert!((*list.add(count)).is_null());
            (0..count)
                .map(|i| CStr::from_ptr(*list.add(i)).to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    let some = title_argv("victory.exe", &["-skipToIIS".into(), "-x=1".into()]);
    assert_eq!(read(&some), ["victory.exe", "-skipToIIS", "-x=1"]);
    assert_eq!(read(&title_argv("victory.exe", &[])), ["victory.exe"]);
    // What the title is given: the same list every time.
    assert_eq!(__p___argc(), __p___argc());
    // SAFETY: as above.
    unsafe { assert_eq!((*__p___argc(), (**__p___argv()).is_null()), (1, false)) };
}

fn w(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[cfg(windows)]
#[test]
fn host_lookups_by_name_get_our_overrides() {
    let kernel32 = GetModuleHandleW(w("kernel32.dll").as_ptr());
    assert!(!kernel32.is_null());
    let ours = crate::registry::lookup("kernel32.dll", "CreateFileW").unwrap();
    assert_eq!(
        GetProcAddress(kernel32, c"CreateFileW".as_ptr().cast()) as usize,
        ours
    );
    // A plain name stays the host's.
    let host = host::get_proc_address(kernel32, c"GetTickCount".as_ptr().cast());
    assert!(!host.is_null());
    assert_eq!(
        GetProcAddress(kernel32, c"GetTickCount".as_ptr().cast()),
        host
    );
    // Host DLLs outside the host policy do not exist for the title.
    assert!(LoadLibraryW(w("wininet.dll").as_ptr()).is_null());
    assert!(LoadLibraryA(c"C:\\Windows\\System32\\urlmon".as_ptr().cast()).is_null());
    assert!(GetModuleHandleW(w("WININET.DLL").as_ptr()).is_null());
    assert_eq!(
        // SAFETY: reads this thread's last error.
        unsafe { windows_sys::Win32::Foundation::GetLastError() },
        ERROR_MOD_NOT_FOUND
    );
    assert!(!LoadLibraryW(w("kernel32").as_ptr()).is_null());
    // By ordinal too (ws2_32 #4 is `connect`).
    let ws2 = LoadLibraryW(w("ws2_32.dll").as_ptr());
    assert_eq!(
        GetProcAddress(ws2, 4 as *const u8) as usize,
        crate::registry::lookup("ws2_32.dll", "connect").unwrap()
    );
}

#[test]
fn title_dlls_and_pseudo_modules_are_found_by_name() {
    let base = 0x7100_0000_0000usize;
    runtime::set_image_path(base, std::path::Path::new(r"C:\title\Mapped.dll"));
    runtime::set_title_dll_loader(Box::new(|file| match file {
        "late.dll" => Ok(Some(0x7200_0000_0000)),
        "broken.dll" => Err("bad image".into()),
        _ => Ok(None),
    }));
    let mapped = base as *mut c_void;
    assert_eq!(GetModuleHandleW(w("MAPPED").as_ptr()), mapped);
    assert_eq!(GetModuleHandleA(c"sub\\mapped.dll".as_ptr().cast()), mapped);
    assert_eq!(LoadLibraryW(w("mapped.dll").as_ptr()), mapped);
    assert_eq!(FreeLibrary(mapped), 1, "title images stay loaded");

    // Not mapped yet: `LoadLibrary` maps it, `GetModuleHandle` does not.
    assert!(GetModuleHandleW(w("late.dll").as_ptr()).is_null());
    assert_eq!(
        LoadLibraryA(c"late".as_ptr().cast()) as usize,
        0x7200_0000_0000
    );
    assert!(LoadLibraryA(c"broken.dll".as_ptr().cast()).is_null());
    // As data it runs no code, so the host loads it (and finds nothing).
    let data = LoadLibraryExW(w("late.dll").as_ptr(), std::ptr::null_mut(), 0x2);
    assert_ne!(data as usize, 0x7200_0000_0000);

    let runtime = LoadLibraryA(c"xgameruntime.dll".as_ptr().cast());
    assert!(!runtime.is_null());
    assert_eq!(GetModuleHandleW(w("XGameRuntime.dll").as_ptr()), runtime);
}

/// No buffer: nothing written, the error says it was too small.
#[test]
fn a_title_image_name_into_no_buffer() {
    let base = 0x7300_0000_0000;
    runtime::set_image_path(base, std::path::Path::new(r"C:\title\game.exe"));
    let module = base as *mut c_void;
    assert_eq!(GetModuleFileNameW(module, std::ptr::null_mut(), 0), 0);
    assert_eq!(GetModuleFileNameA(module, std::ptr::null_mut(), 0), 0);
    #[cfg(windows)]
    {
        // SAFETY: reads this thread's last error.
        let last = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        assert_eq!(last, ERROR_INSUFFICIENT_BUFFER);
    }
}
