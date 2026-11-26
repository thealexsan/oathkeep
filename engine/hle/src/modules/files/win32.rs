use std::ffi::{c_char, c_void};

use super::{Handle, SEPARATORS, host, narrow, wide};

// kernel32 and its file API sets.

pub extern "system" fn CreateFileW(
    path: *const u16,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
        p.ptr(),
        access,
        share,
        security,
        disposition,
        flags,
        template,
    )
}

pub extern "system" fn CreateFileA(
    path: *const c_char,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                p.as_ptr(),
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
        None => {
            host!("kernel32.dll", "CreateFileA": extern "system" fn(*const c_char, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                path,
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
    }
}

/// Used by Bink.
pub extern "system" fn CreateFile2(
    path: *const u16,
    access: u32,
    share: u32,
    disposition: u32,
    params: *const c_void,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "CreateFile2": extern "system" fn(*const u16, u32, u32, u32, *const c_void) -> Handle)(
        p.ptr(),
        access,
        share,
        disposition,
        params,
    )
}

pub extern "system" fn DeleteFileA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "DeleteFileW": extern "system" fn(*const u16) -> i32)(p.as_ptr())
        }
        None => {
            host!("kernel32.dll", "DeleteFileA": extern "system" fn(*const c_char) -> i32)(path)
        }
    }
}

pub extern "system" fn DeleteFileW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "DeleteFileW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn RemoveDirectoryW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "RemoveDirectoryW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "SetFileAttributesW": extern "system" fn(*const u16, u32) -> i32)(
        p.ptr(),
        attributes,
    )
}

pub extern "system" fn GetFileAttributesA(path: *const c_char) -> u32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "GetFileAttributesA": extern "system" fn(*const c_char) -> u32)(
                path,
            )
        }
    }
}

pub extern "system" fn CreateDirectoryW(path: *const u16, security: *const c_void) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "CreateDirectoryW": extern "system" fn(*const u16, *const c_void) -> i32)(
        p.ptr(),
        security,
    )
}

pub extern "system" fn GetFileAttributesExW(
    path: *const u16,
    level: i32,
    info: *mut c_void,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesExW": extern "system" fn(*const u16, i32, *mut c_void) -> i32)(
        p.ptr(),
        level,
        info,
    )
}

pub extern "system" fn GetFileAttributesW(path: *const u16) -> u32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(p.ptr())
}

pub extern "system" fn GetDiskFreeSpaceExW(
    path: *const u16,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetDiskFreeSpaceExW": extern "system" fn(*const u16, *mut u64, *mut u64, *mut u64) -> i32)(
        p.ptr(),
        available,
        total,
        free,
    )
}

/// Wants a volume root: a virtual drive answers for the host volume its
/// folder is on.
pub extern "system" fn GetDiskFreeSpaceW(
    root: *const u16,
    sectors_per_cluster: *mut u32,
    bytes_per_sector: *mut u32,
    free_clusters: *mut u32,
    clusters: *mut u32,
) -> i32 {
    let mut p = wide(root);
    if let Some(host) = &mut p.1
        && let [letter, colon, sep, ..] = host[..]
        && colon == u16::from(b':')
        && SEPARATORS.contains(&sep)
    {
        *host = vec![letter, colon, sep, 0];
    }
    host!("kernel32.dll", "GetDiskFreeSpaceW": extern "system" fn(*const u16, *mut u32, *mut u32, *mut u32, *mut u32) -> i32)(
        p.ptr(),
        sectors_per_cluster,
        bytes_per_sector,
        free_clusters,
        clusters,
    )
}

pub extern "system" fn FindFirstFileW(path: *const u16, data: *mut c_void) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileW": extern "system" fn(*const u16, *mut c_void) -> Handle)(
        p.ptr(),
        data,
    )
}

pub extern "system" fn FindFirstFileExW(
    path: *const u16,
    level: i32,
    data: *mut c_void,
    search: i32,
    filter: *mut c_void,
    flags: u32,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileExW": extern "system" fn(*const u16, i32, *mut c_void, i32, *mut c_void, u32) -> Handle)(
        p.ptr(),
        level,
        data,
        search,
        filter,
        flags,
    )
}

pub extern "system" fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "MoveFileExW": extern "system" fn(*const u16, *const u16, u32) -> i32)(
        from.ptr(),
        to.ptr(),
        flags,
    )
}

pub extern "system" fn SetCurrentDirectoryA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "SetCurrentDirectoryW": extern "system" fn(*const u16) -> i32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "SetCurrentDirectoryA": extern "system" fn(*const c_char) -> i32)(
                path,
            )
        }
    }
}

// Kernel reads into title memory. The GPU side leaves blocks of title GPU
// buffers read-only to see the title's next write, and the kernel's writes
// fail there instead of faulting into its handler; the title's
// synchronous reads (title+0x313fc0) read straight into 4 KiB-aligned
// destinations and retry a failed `ReadFile` forever. So the destination
// is opened first, and not guarded again before the call is done.

pub extern "system" fn ReadFile(
    file: *mut c_void,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    let writing = oathkeep_gpu::kernel_writing(buffer as usize, len as usize);
    let done = host!("kernel32.dll", "ReadFile": extern "system" fn(*mut c_void, *mut u8, u32, *mut u32, *mut c_void) -> i32)(
        file, buffer, len, read, overlapped,
    );
    // The call's error is the title's to ask for.
    // SAFETY: plain calls.
    let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    // shortcut: a read that goes on after the call (overlapped) is written
    // into pages the kernel has locked by now: it lands all the same, but a
    // refresh that guards its blocks before it does will not see it.
    if done == 0 && error == windows_sys::Win32::Foundation::ERROR_IO_PENDING {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(len, "ReadFile: a read of the title's goes on after the call")
        });
    }
    if done == 0 && error == windows_sys::Win32::Foundation::ERROR_NOACCESS {
        static FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            tracing::warn!(
                n,
                buffer = format_args!("{buffer:p}"),
                len,
                "ReadFile: the title's buffer could not be written"
            );
        }
    }
    drop(writing);
    // SAFETY: as above.
    unsafe { windows_sys::Win32::Foundation::SetLastError(error) };
    done
}
