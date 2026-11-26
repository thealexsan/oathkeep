use super::*;

mod guards_tests;
mod holes_tests;
mod many_askers_tests;
mod mapping_tests;
mod parts_tests;
mod views_tests;

/// A placeholder of `blocks` blocks, each its own.
pub(super) fn placeholder(blocks: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, VirtualAlloc2,
        VirtualFree,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a fresh placeholder, split.
    unsafe {
        let p = VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            blocks * BLOCK,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        ) as usize;
        assert_ne!(p, 0);
        for block in 0..blocks - 1 {
            assert_ne!(
                VirtualFree((p + block * BLOCK) as *mut _, BLOCK, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER),
                0
            );
        }
        p
    }
}

/// Private memory of `protect` in place of the placeholder of one block.
pub(super) fn private_at(at: usize, protect: u32) {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, VirtualAlloc2};
    // SAFETY: our placeholder.
    let got = unsafe {
        VirtualAlloc2(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            at as *const _,
            BLOCK,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            protect,
            std::ptr::null_mut(),
            0,
        )
    };
    assert_eq!(got as usize, at);
}

pub(super) fn protection(at: usize) -> u32 {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data the query fills in.
    unsafe {
        let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
        VirtualQuery(at as *const _, &mut info, size_of_val(&info));
        info.Protect
    }
}

/// A pagefile section mapped twice: two views of the same physical
/// pages, the second left for the test; the section handle, the two
/// addresses.
pub(super) fn two_views(bytes: usize) -> (usize, usize, usize) {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE};
    // SAFETY: a fresh pagefile section, mapped twice; the views keep
    // the section alive and the handle is closed by the caller.
    unsafe {
        let section = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            bytes as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
        let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
        assert_ne!(one, 0);
        assert_ne!(two, 0);
        crate::inplace::title_memory_view(one, bytes, section as usize, 0);
        crate::inplace::title_memory_view(two, bytes, section as usize, 0);
        (section as usize, one, two)
    }
}

/// The section of [`two_views`] is closed and its views unmapped; the
/// guard is told they are gone first.
pub(super) fn drop_views(section: usize, views: &[usize], bytes: usize) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
    for &view in views {
        title_memory_remapped(view, bytes);
        // SAFETY: a view of the section mapped by `two_views`.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: view as *mut _,
            })
        };
    }
    // SAFETY: the section's last reference.
    unsafe { CloseHandle(section as *mut std::ffi::c_void) };
}
