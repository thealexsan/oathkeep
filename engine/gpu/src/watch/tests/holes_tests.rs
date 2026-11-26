use super::super::fault::writable;
use super::super::protect::{guard, is_view};
use super::super::*;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

/// The runtime maps a view and then tells us; a copy's refresh on another
/// thread can guard it in between. Told, it is the title's to write: a
/// block left read-only and forgotten would be an access violation in
/// title code.
#[test]
fn a_block_guarded_before_we_were_told_of_its_view_can_be_written() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
    };
    // SAFETY: a pagefile section of two blocks, mapped read-write; the
    // view holds it.
    let view = unsafe {
        let section = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            (2 * BLOCK) as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK);
        CloseHandle(section);
        view
    };
    let at = view.Value as usize;
    assert_ne!(at, 0);
    // The refresh, then the runtime's word.
    assert!(guard(at, at + 2 * BLOCK).is_some());
    assert!(!writable(at) && !writable(at + BLOCK));
    title_memory_remapped(at, 2 * BLOCK);
    assert!(writable(at) && writable(at + BLOCK));
    // SAFETY: inside the view; a fault would end the process.
    unsafe { ((at + 8) as *mut u8).write_volatile(1) };
    // Looked at again as new, and guarded again.
    assert_eq!(
        guard(at, at + 2 * BLOCK),
        Some((at / PAGE..at / PAGE + 2).collect())
    );
    assert!(!writable(at));
    title_memory_unmapping(at, 2 * BLOCK);
    // SAFETY: our view.
    unsafe { UnmapViewOfFile(view) };
}

#[test]
fn writes_in_partly_committed_view_blocks_are_reported() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        SEC_RESERVE, UnmapViewOfFile,
    };

    const SIZE: usize = 3 * BLOCK;
    struct View(MEMORY_MAPPED_VIEW_ADDRESS);
    impl Drop for View {
        fn drop(&mut self) {
            title_memory_unmapping(self.0.Value as usize, SIZE);
            // SAFETY: our view is still mapped.
            unsafe { UnmapViewOfFile(self.0) };
        }
    }
    // SAFETY: a new reserved pagefile section. The view holds its own
    // reference after we close the handle, including on assertion failure.
    let view = unsafe {
        let section = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE | SEC_RESERVE,
            0,
            SIZE as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
        CloseHandle(section);
        assert!(!view.Value.is_null());
        View(view)
    };
    let start = view.0.Value as usize;
    let commit = |at: usize, len| {
        // SAFETY: these pages belong to our reserved view.
        let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(committed as usize, at);
    };
    let early = start + BLOCK + 3 * KERNEL_PAGE;
    let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
    commit(start, BLOCK);
    commit(early, KERNEL_PAGE);
    let refresh = || guard(start, start + SIZE).unwrap();
    assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
    // The last block is still entirely reserved.
    assert!(!refresh().contains(&(late / PAGE)));

    commit(late, KERNEL_PAGE); // Ordinary commit, without a runtime mapping notification.
    for at in [early, late] {
        assert!(writable(at));
        // SAFETY: each address is inside its committed read-write page.
        unsafe { (at as *mut u8).write_volatile(1) };
    }
    assert_eq!(refresh(), [early / PAGE, late / PAGE]);
}

/// A range of view memory whose first block is not mapped is a view
/// all the same: its blocks are reported once, then the ones written
/// or mapped since.
#[test]
fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
        MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
        VirtualAlloc2,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a fresh placeholder of three blocks and a pagefile
    // section of three; the middle block is mapped first.
    let (p, section) = unsafe {
        let p = VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            3 * BLOCK,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        ) as usize;
        assert_ne!(p, 0);
        let section = CreateFileMappingW(
            -1isize as _,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            (3 * BLOCK) as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        (p, section)
    };
    let map = |block: usize| {
        // SAFETY: splits the placeholder at the block and maps the
        // section's block there.
        let view = unsafe {
            VirtualFree(
                (p + block * BLOCK) as *mut _,
                BLOCK,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            );
            MapViewOfFile3(
                section,
                GetCurrentProcess(),
                (p + block * BLOCK) as *const _,
                (block * BLOCK) as u64,
                BLOCK,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(view.Value as usize, p + block * BLOCK);
        view
    };
    let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
    let all: Vec<usize> = (0..3).flat_map(pages).collect();
    // Nothing mapped at all: a view with every block absent.
    assert!(is_view(p, p + 3 * BLOCK));
    let middle = map(1);
    title_memory_remapped(p + BLOCK, BLOCK);
    // The range begins in the placeholder, its middle block is a view.
    assert!(is_view(p, p + 3 * BLOCK));
    assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
    assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()), "nothing written since");
    // SAFETY: inside the mapped middle block.
    unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
    assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
    let first = map(0);
    title_memory_remapped(p, BLOCK);
    assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0).collect()));
    assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
    // Memory of ours that is no view stays none.
    let mut local = [0u8; 16];
    let at = local.as_mut_ptr() as usize;
    assert!(!is_view(at, at + local.len()));
    title_memory_unmapping(p, 3 * BLOCK);
    // SAFETY: our views and placeholder.
    unsafe {
        UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
        UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
        for block in 0..3 {
            VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
        }
        windows_sys::Win32::Foundation::CloseHandle(section);
    }
}

/// Blocks of a placeholder between views are reported once, then not
/// again (they cannot be guarded) until the runtime maps them.
#[test]
fn unmapped_blocks_are_reported_once_until_mapped() {
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
        MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
        VirtualAlloc2,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a fresh placeholder of three blocks and a pagefile
    // section of two; block 0 shows the section's first block, as the
    // runtime maps physical pages.
    let (p, section) = unsafe {
        let p = VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            3 * BLOCK,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        ) as usize;
        assert_ne!(p, 0);
        let section = CreateFileMappingW(
            -1isize as _,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            (2 * BLOCK) as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        (p, section)
    };
    let map = |block: usize| {
        // SAFETY: splits the placeholder at the block and maps the
        // section's block there.
        let view = unsafe {
            VirtualFree(
                (p + block * BLOCK) as *mut _,
                BLOCK,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            );
            MapViewOfFile3(
                section,
                GetCurrentProcess(),
                (p + block * BLOCK) as *const _,
                (block * BLOCK) as u64,
                BLOCK,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(view.Value as usize, p + block * BLOCK);
        view
    };
    let first = map(0);
    let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
    let all: Vec<usize> = (0..3).flat_map(pages).collect();
    assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
    assert_eq!(
        guard(p, p + 3 * BLOCK),
        Some(Vec::new()),
        "nothing written since"
    );
    let second = map(1);
    title_memory_remapped(p + BLOCK, BLOCK);
    assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
    assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
    title_memory_unmapping(p, 3 * BLOCK);
    // SAFETY: our views and placeholder.
    unsafe {
        UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
        UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
        for block in 0..3 {
            VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
        }
        windows_sys::Win32::Foundation::CloseHandle(section);
    }
}
