use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

const DLL_THREAD_ATTACH: u32 = 2;

#[link(name = "xgameruntime", kind = "raw-dylib")]
unsafe extern "system" {
    fn XGameRuntimeInitialize() -> i32;
}

#[link(name = "oathkeep-probe-missing", kind = "raw-dylib")]
unsafe extern "system" {
    fn OathkeepProbeNotImplemented(x: u64) -> u64;
}

#[link(name = "oathkeep-probe-title", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetFileVersionInfoSizeW(path: *const u16, handle: *mut u32) -> u32;
}

#[link(name = "oathkeep-probe-delayed", kind = "raw-dylib")]
unsafe extern "system" {
    fn OathkeepProbeDelayed() -> u32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTickCount() -> u32;
    fn GetCurrentProcessId() -> u32;
}

#[unsafe(no_mangle)]
pub extern "system" fn probe_xgameruntime_init() -> i32 {
    unsafe { XGameRuntimeInitialize() }
}

#[unsafe(no_mangle)]
pub extern "system" fn probe_tick() -> u32 {
    unsafe { GetTickCount() }
}

#[unsafe(no_mangle)]
pub extern "system" fn probe_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

#[unsafe(no_mangle)]
pub extern "system" fn probe_title() -> u32 {
    let path: Vec<u16> = r"C:\Windows\System32\kernel32.dll"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut handle = 0;
    unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut handle) }
}

#[unsafe(no_mangle)]
pub extern "system" fn probe_delayed() -> u32 {
    unsafe { OathkeepProbeDelayed() }
}

#[unsafe(no_mangle)]
pub extern "system" fn probe_missing() -> u64 {
    unsafe { OathkeepProbeNotImplemented(42) }
}

static TLS_THREAD_STARTS: AtomicU32 = AtomicU32::new(0);
static MAIN_THREAD_STARTS: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn on_thread(_: *mut c_void, reason: u32, _: *mut c_void) {
    if reason == DLL_THREAD_ATTACH {
        TLS_THREAD_STARTS.fetch_add(1, Ordering::Relaxed);
    }
}

#[used]
#[unsafe(link_section = ".CRT$XLB")]
static ON_THREAD: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) = on_thread;

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_: *mut c_void, reason: u32, _: *mut c_void) -> i32 {
    if reason == DLL_THREAD_ATTACH {
        MAIN_THREAD_STARTS.fetch_add(1, Ordering::Relaxed);
        unsafe { GetTickCount() };
    }
    if reason == DLL_PROCESS_DETACH {
        DETACHES.fetch_add(1, Ordering::Relaxed);
    }
    1
}

const DLL_PROCESS_DETACH: u32 = 0;
static DETACHES: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
pub extern "system" fn probe_detaches() -> u32 {
    DETACHES.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
pub extern "system" fn probe_thread_starts() -> u64 {
    // SAFETY: reading our own static keeps the linker from dropping it.
    unsafe { std::ptr::from_ref(&ON_THREAD).read_volatile() };
    u64::from(TLS_THREAD_STARTS.load(Ordering::Relaxed)) << 32
        | u64::from(MAIN_THREAD_STARTS.load(Ordering::Relaxed))
}
