#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{FreeLibrary, SetLastError};
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::LibraryLoader::{
        DisableThreadLibraryCalls, GetModuleFileNameA, GetModuleFileNameW, GetModuleHandleA,
        GetModuleHandleW, GetProcAddress, LoadLibraryExA, LoadLibraryExW,
    };
    use windows_sys::Win32::System::Threading::TerminateProcess;

    unsafe extern "C" {
        #[link_name = "exit"]
        fn crt_exit(code: i32) -> !;
        #[link_name = "abort"]
        fn crt_abort() -> !;
    }

    // SAFETY (every function): forwarding the title's own arguments to the
    // host API of the same name.

    pub(in super::super) fn get_module_handle_w(name: *const u16) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetModuleHandleW(name) }
    }

    pub(in super::super) fn get_module_handle_a(name: *const u8) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetModuleHandleA(name) }
    }

    pub(in super::super) fn get_module_file_name_w(
        module: *mut c_void,
        out: *mut u16,
        size: u32,
    ) -> u32 {
        // SAFETY: see above.
        unsafe { GetModuleFileNameW(module, out, size) }
    }

    pub(in super::super) fn get_module_file_name_a(
        module: *mut c_void,
        out: *mut u8,
        size: u32,
    ) -> u32 {
        // SAFETY: see above.
        unsafe { GetModuleFileNameA(module, out, size) }
    }

    pub(in super::super) fn set_last_error(code: u32) {
        // SAFETY: see above.
        unsafe { SetLastError(code) }
    }

    pub(in super::super) fn load_library_ex_a(
        name: *const u8,
        file: *mut c_void,
        flags: u32,
    ) -> *mut c_void {
        // SAFETY: see above.
        unsafe { LoadLibraryExA(name, file, flags) }
    }

    pub(in super::super) fn load_library_ex_w(
        name: *const u16,
        file: *mut c_void,
        flags: u32,
    ) -> *mut c_void {
        // SAFETY: see above.
        unsafe { LoadLibraryExW(name, file, flags) }
    }

    pub(in super::super) fn get_proc_address(
        module: *mut c_void,
        name: *const u8,
    ) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetProcAddress(module, name) }.map_or(std::ptr::null_mut(), |f| f as *mut c_void)
    }

    pub(in super::super) fn free_library(module: *mut c_void) -> i32 {
        // SAFETY: see above.
        unsafe { FreeLibrary(module) }
    }

    pub(in super::super) fn disable_thread_library_calls(module: *mut c_void) -> i32 {
        match oathkeep_loader::disable_thread_calls(module as usize) {
            Some(done) => i32::from(done),
            // SAFETY: see above.
            None => unsafe { DisableThreadLibraryCalls(module) },
        }
    }

    pub(in super::super) fn exit(code: i32) -> ! {
        // SAFETY: see above.
        unsafe { crt_exit(code) }
    }

    pub(in super::super) fn abort() -> ! {
        // SAFETY: see above.
        unsafe { crt_abort() }
    }

    pub(in super::super) fn terminate_process(process: *mut c_void, code: u32) -> i32 {
        // SAFETY: see above.
        unsafe { TerminateProcess(process, code) }
    }

    pub(in super::super) fn set_unhandled_exception_filter(
        filter: extern "system" fn(*const c_void) -> i32,
    ) {
        // SAFETY: our filter takes the exception pointers as an opaque pointer.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(
                std::mem::transmute::<
                    extern "system" fn(*const c_void) -> i32,
                    windows_sys::Win32::System::Diagnostics::Debug::LPTOP_LEVEL_EXCEPTION_FILTER,
                >(filter),
            )
        };
    }

    pub(in super::super) fn report_unhandled(info: *const c_void) {
        // SAFETY: the system's exception pointers, passed to the filter.
        unsafe { oathkeep_loader::report_unhandled(info.cast()) };
    }

    pub(in super::super) fn note_title_output(line: &str) {
        oathkeep_loader::note_title_output(line);
    }

    /// Our caller's callers, as `module+rva`.
    pub(in super::super) fn callers() -> String {
        let mut frames = [std::ptr::null_mut(); 12];
        // SAFETY: fills at most `frames.len()` return addresses.
        let n = unsafe {
            RtlCaptureStackBackTrace(
                2,
                frames.len() as u32,
                frames.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        let names: Vec<_> = frames[..usize::from(n)]
            .iter()
            .map(|&f| oathkeep_loader::locate(f as usize))
            .collect();
        names.join(" < ")
    }
}

#[cfg(not(windows))]
mod imp {
    use std::ffi::c_void;

    pub(in super::super) fn get_module_handle_w(_: *const u16) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(in super::super) fn get_module_handle_a(_: *const u8) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(in super::super) fn load_library_ex_a(
        _: *const u8,
        _: *mut c_void,
        _: u32,
    ) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(in super::super) fn get_module_file_name_w(_: *mut c_void, _: *mut u16, _: u32) -> u32 {
        0
    }

    pub(in super::super) fn get_module_file_name_a(_: *mut c_void, _: *mut u8, _: u32) -> u32 {
        0
    }

    pub(in super::super) fn set_last_error(_: u32) {}

    pub(in super::super) fn load_library_ex_w(
        _: *const u16,
        _: *mut c_void,
        _: u32,
    ) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(in super::super) fn get_proc_address(_: *mut c_void, _: *const u8) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(in super::super) fn free_library(_: *mut c_void) -> i32 {
        0
    }

    pub(in super::super) fn disable_thread_library_calls(_: *mut c_void) -> i32 {
        0
    }

    pub(in super::super) fn exit(code: i32) -> ! {
        std::process::exit(code)
    }

    pub(in super::super) fn abort() -> ! {
        std::process::abort()
    }

    pub(in super::super) fn terminate_process(_: *mut c_void, code: u32) -> i32 {
        std::process::exit(code as i32)
    }

    pub(in super::super) fn set_unhandled_exception_filter(
        _: extern "system" fn(*const c_void) -> i32,
    ) {
    }

    pub(in super::super) fn report_unhandled(_: *const c_void) {}

    pub(in super::super) fn note_title_output(_: &str) {}

    pub(in super::super) fn callers() -> String {
        String::new()
    }
}

pub(super) use imp::*;
