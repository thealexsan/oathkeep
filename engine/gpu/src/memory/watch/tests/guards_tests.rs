use super::super::ask::ask_rest;
use super::super::fault::writable;
use super::super::guards::{guards, known_holes, published};
use super::super::journal::journal;
use super::super::protect::guard;
use super::super::*;
use super::{placeholder, private_at, protection};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};

const MEM_WRITE_WATCH: u32 = 0x20_0000;

#[test]
fn written_pages_are_reported_to_every_asker() {
    // SAFETY: a fresh write-watched allocation, freed at the end.
    let p = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            4 * PAGE,
            MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
            PAGE_READWRITE,
        )
    } as usize;
    assert_ne!(p, 0);
    let (start, end) = (p, p + 4 * PAGE);
    // The journal may hold pages of another test's memory that was here.
    let (none, before) = writes(start, end, false).expect("watched");
    assert!(before.iter().all(|&(_, w)| w < none), "{before:?}");
    // SAFETY: committed pages of ours.
    unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
    let (now, after) = writes(start, end, false).expect("watched");
    let new = |w: Vec<(usize, u64)>| w.into_iter().filter(|w| w.1 > none).collect::<Vec<_>>();
    assert_eq!(new(after), [(p / PAGE + 1, now)]);
    // Another asker after the kernel's bit was cleared still sees it.
    let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
    assert!(later > now);
    assert_eq!(new(again), [(p / PAGE + 1, now)]);
    let mut plain = vec![0u8; PAGE];
    assert!(
        writes(
            plain.as_mut_ptr() as usize,
            plain.as_ptr() as usize + PAGE,
            true
        )
        .is_none()
    );
    // SAFETY: our allocation.
    unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
}

/// A kernel read into a guarded view block fails; opened first, it
/// lands and the block counts as written.
#[test]
fn kernel_reads_into_guarded_blocks_after_opening() {
    use std::io::{Read, Seek, Write};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
    };
    // SAFETY: a pagefile section of two blocks, mapped read-write.
    let view = unsafe {
        let section = CreateFileMappingW(
            -1isize as _,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            (2 * BLOCK) as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
    };
    assert_ne!(view, 0);
    assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"from the disk").unwrap();
    // `ReadFile` inside: the kernel writes the view.
    let read = |file: &mut std::fs::File| {
        file.rewind().unwrap();
        // SAFETY: 13 bytes of our view, nothing else refers to them.
        let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
        file.read(into).is_ok_and(|n| n == 13)
    };
    assert!(
        !read(&mut file),
        "the kernel does not fault into our handler"
    );
    writing(view + 100, 13);
    assert!(read(&mut file));
    // SAFETY: the bytes just read.
    let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
    assert_eq!(landed, b"from the disk");
    assert_eq!(
        guard(view, view + 2 * BLOCK),
        Some((view / PAGE..view / PAGE + 1).collect())
    );
    // Before the view goes, as the runtime does: the address may be
    // another test's the moment it is free.
    title_memory_unmapping(view, 2 * BLOCK);
    // SAFETY: our view.
    unsafe {
        UnmapViewOfFile(
            windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: view as *mut _,
            },
        )
    };
}

/// A refresh between the opening of a kernel read's destination and the
/// read itself leaves it open: the read lands, and is seen.
#[test]
fn a_refresh_leaves_open_what_the_kernel_is_writing() {
    use std::io::{Read, Seek, Write};
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile};
    // SAFETY: a pagefile section of two blocks, mapped read-write.
    let view = unsafe {
        let section = CreateFileMappingW(
            -1isize as _,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            (2 * BLOCK) as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK)
    };
    let at = view.Value as usize;
    assert_ne!(at, 0);
    let pages = |block: usize| at / PAGE + block..at / PAGE + block + 1;
    assert!(writes(at, at + 2 * BLOCK, true).is_some());
    assert!(published(at, at + 2 * BLOCK) || !light());
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"from the disk").unwrap();
    file.rewind().unwrap();
    let writing = kernel_writing(at + 100, 13);
    // The refresh: the block being written is new to it every time, and
    // stays open; the other is as it was.
    for _ in 0..2 {
        assert_eq!(guard(at, at + 2 * BLOCK), Some(pages(0).collect()));
        let (now, found) = writes(at, at + BLOCK, true).expect("a view");
        assert!(found.contains(&(at / PAGE, now)));
        assert!(writable(at) && !writable(at + BLOCK));
        assert!(!published(at, at + BLOCK));
    }
    // SAFETY: 13 bytes of our view, nothing else refers to them.
    let into = unsafe { std::slice::from_raw_parts_mut((at + 100) as *mut u8, 13) };
    assert_eq!(file.read(into).expect("the kernel's write lands"), 13);
    assert_eq!(into, b"from the disk");
    drop(writing);
    // Seen by the next refresh, and guarded again.
    assert_eq!(guard(at, at + 2 * BLOCK), Some(pages(0).collect()));
    assert!(!writable(at));
    assert_eq!(guard(at, at + 2 * BLOCK), Some(Vec::new()));
    title_memory_unmapping(at, 2 * BLOCK);
    // SAFETY: our view.
    unsafe { UnmapViewOfFile(view) };
}

/// A hole the kernel is to write is no known hole meanwhile, nor is one
/// the runtime has begun to change; after that it is again.
#[test]
fn what_is_left_alone_is_no_known_hole() {
    let p = placeholder(3);
    let known = |block: usize| published(p + block * BLOCK, p + (block + 1) * BLOCK);
    let ask = || writes(p, p + 3 * BLOCK, true).expect("as a view");
    ask();
    let holes = known_holes();
    assert!((0..3).all(known) == holes);
    let writing = kernel_writing(p + BLOCK + 5, 100);
    for _ in 0..2 {
        assert!(known(0) == holes && !known(1) && known(2) == holes);
        let (now, written) = ask();
        assert!(written.contains(&(p / PAGE + 1, now)));
        assert!(written.iter().all(|w| w.0 == p / PAGE + 1 || w.1 < now));
    }
    assert!(!guards().held.contains(p / BLOCK + 1));
    drop(writing);
    ask();
    assert!((0..3).all(known) == holes);
    {
        let _changing = title_memory_remapping();
        title_memory_unmapping(p, BLOCK);
        ask();
        assert!(!known(0) && !guards().held.contains(p / BLOCK));
    }
    ask();
    assert!((0..3).all(known) == holes);
    title_memory_freed(p, BLOCK);
    assert!(!known(0) && !guards().held.contains(p / BLOCK));
    title_memory_unmapping(p, 3 * BLOCK);
    // SAFETY: our placeholders.
    unsafe {
        for block in 0..3 {
            VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
        }
    }
}

/// A refresh during a change of mapping guards nothing the runtime has
/// begun to unmap: no note of it stays behind.
#[test]
fn nothing_is_guarded_between_our_forgetting_it_and_its_unmapping() {
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile};
    // SAFETY: a pagefile section of a block, mapped read-write.
    let view = unsafe {
        let section =
            CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, BLOCK as u32, std::ptr::null());
        assert!(!section.is_null());
        MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, BLOCK)
    };
    let at = view.Value as usize;
    assert_ne!(at, 0);
    assert!(writes(at, at + BLOCK, true).is_some());
    let changing = title_memory_remapping();
    title_memory_unmapping(at, BLOCK);
    assert!(writes(at, at + BLOCK, true).is_some());
    {
        let g = guards();
        assert!(!g.protected.contains(at / BLOCK) && !g.clean.contains(at / BLOCK));
    }
    assert!(!published(at, at + BLOCK));
    // SAFETY: our view.
    unsafe { UnmapViewOfFile(view) };
    drop(changing);
    assert!(!guards().left_alone(at / BLOCK));
}

/// Memory at an address where blocks were guarded or absent is not
/// taken for them: code stays code, private memory is not guarded.
#[test]
fn other_memory_at_a_guarded_address_keeps_its_protection() {
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER, MapViewOfFile3, PAGE_EXECUTE_READ,
        PAGE_EXECUTE_READWRITE, UnmapViewOfFile2,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let p = placeholder(3);
    // SAFETY: a section's block in place of the first placeholder.
    let view = unsafe {
        let section =
            CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, BLOCK as u32, std::ptr::null());
        assert!(!section.is_null());
        MapViewOfFile3(
            section,
            GetCurrentProcess(),
            p as *const _,
            0,
            BLOCK,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    assert_eq!(view.Value as usize, p);
    // One block guarded, two absent.
    assert_eq!(guard(p, p + 3 * BLOCK).map(|w| w.len()), Some(3));
    // Not told of what comes: the view goes, code is there; and private
    // memory in the holes.
    // SAFETY: our view.
    assert_ne!(unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) }, 0);
    private_at(p, PAGE_EXECUTE_READ);
    private_at(p + BLOCK, PAGE_READWRITE);
    private_at(p + 2 * BLOCK, PAGE_EXECUTE_READWRITE);
    // (The runtime's allocations it tells of, or the holes stay known.)
    title_memory_allocated(p + BLOCK, 2 * BLOCK);
    let as_made = || {
        assert_eq!(protection(p), PAGE_EXECUTE_READ);
        assert_eq!(protection(p + BLOCK), PAGE_READWRITE);
        assert_eq!(protection(p + 2 * BLOCK), PAGE_EXECUTE_READWRITE);
    };
    let holes = [p / PAGE + 1, p / PAGE + 2];
    for _ in 0..2 {
        let written = guard(p, p + 3 * BLOCK).expect("as a view");
        assert!(holes.iter().all(|page| written.contains(page)), "copied every time");
        as_made();
        let g = guards();
        assert!(holes.iter().all(|&block| !g.held.contains(block) && !g.absent.contains(block)));
    }
    writing(p, BLOCK);
    as_made();
    title_memory_unmapping(p, 3 * BLOCK);
    // SAFETY: our allocations.
    unsafe {
        for block in 0..3 {
            VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
        }
    }
}

/// Memory the title frees leaves no note of its blocks, whether it names
/// its length or the allocation.
#[test]
fn freed_memory_leaves_no_notes() {
    let p = placeholder(3);
    let noted = |block: usize| {
        let g = guards();
        let block = p / BLOCK + block;
        g.protected.contains(block) || g.clean.contains(block) || g.absent.contains(block)
    };
    assert!(guard(p, p + 3 * BLOCK).is_some());
    assert!(noted(0) && noted(1) && noted(2));
    // The middle allocation.
    title_memory_freed(p + BLOCK, 0);
    assert!(noted(0) && !noted(1) && noted(2));
    title_memory_freed(p, BLOCK);
    assert!(!noted(0) && noted(2));
    // Nothing noted there: nothing to do, and nothing beside it.
    title_memory_freed(p + BLOCK, 0);
    assert!(noted(2));
    title_memory_freed(p + 2 * BLOCK, 0);
    assert!(!noted(2));
    // SAFETY: our placeholders.
    unsafe {
        for block in 0..3 {
            VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
        }
    }
}

/// A write between another thread's asking about a range and the second
/// part of a submission's one asking is later than what that thread
/// copied.
#[test]
fn the_rest_of_the_one_asking_has_a_number_of_its_own() {
    // SAFETY: a fresh write-watched allocation, freed at the end.
    let p = unsafe {
        VirtualAlloc(std::ptr::null(), 4 * PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
    } as usize;
    assert_ne!(p, 0);
    // One range: `ask_ahead` leaves it.
    let ranges = [(p, p + 4 * PAGE)];
    ask_ahead(&ranges);
    let (copied, _) = writes(p, p + 4 * PAGE, false).expect("watched");
    // SAFETY: committed pages of ours.
    unsafe { ((p + PAGE) as *mut u8).write_volatile(1) };
    ask_rest(&ranges);
    let last = journal().written.get(&(p / PAGE + 1)).copied();
    ask_once_end();
    assert!(last.is_some_and(|last| last > copied), "{last:?} is not after {copied}");
    // SAFETY: our allocation.
    unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
}
