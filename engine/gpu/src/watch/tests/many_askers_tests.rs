use super::super::bits::Bits;
use super::super::guards::{REST_IN_TEST, placeholder, published};
use super::super::{Beside, PAGE, landed_beside, look_at_holes, put_beside, title_memory_allocated, title_memory_reading, title_memory_remapping, title_memory_unmapping, writes, writing};
use windows_sys::Win32::System::Memory::*;

/// Threads that write pages and ask about overlapping ranges at the same
/// time: whoever wrote a page before asking is told of that write (by
/// its own asking or by one that took the kernel's answer before it).
#[test]
fn no_write_is_lost_between_askers() {
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    const PAGES: usize = 48;
    const THREADS: usize = 8;
    // SAFETY: a reservation of our own, freed at the end.
    let at = unsafe {
        VirtualAlloc(std::ptr::null(), PAGES * PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
    } as usize;
    assert!(at != 0 && at.is_multiple_of(PAGE));
    std::thread::scope(|s| {
        for t in 0..THREADS {
            s.spawn(move || {
                let mut last = [0u64; PAGES];
                let mut seed = t as u64 * 7919 + 1;
                let mut next = |n: usize| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (seed >> 33) as usize % n
                };
                for _ in 0..4000 {
                    let page = next(PAGES);
                    let from = page - next(page.min(12) + 1);
                    let to = page + 1 + next((PAGES - page).min(12));
                    // SAFETY: committed memory of ours; what the racing
                    // writers leave in it does not matter.
                    unsafe { ((at + page * PAGE + t * 8) as *mut u64).write_volatile(1) };
                    let (now, written) = writes(at + from * PAGE, at + to * PAGE, false).expect("write-watched");
                    let number = written.iter().find(|w| w.0 == at / PAGE + page).map_or(0, |w| w.1);
                    assert!(number > last[page], "page {page}: {number} is not after {}", last[page]);
                    for l in &mut last[from..to] {
                        *l = now;
                    }
                }
            });
        }
    });
    // SAFETY: the reservation made above.
    unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
}

/// The same of view memory, which is guarded: what its bits answer
/// misses no write either, however often blocks are opened and guarded
/// again between.
#[test]
fn no_write_to_views_is_lost_between_askers() {
    writes_to_views_are_not_lost::<1>(0);
}

/// The same with blocks that rest after a write (`rest`).
#[test]
fn no_write_to_resting_views_is_lost_between_askers() {
    writes_to_views_are_not_lost::<1>(50);
    writes_to_views_are_not_lost::<3>(50);
}

/// The same of pages shown at three addresses, each of which has copies
/// of its own: a write through any of them, or put past the guards, is
/// told at all. A fourth view comes and goes meanwhile, and is written
/// too. (The first assertion fails within a few rounds if a guard holds
/// for one address only, `OATHKEEP_VIEW_GUARD=0`.)
#[test]
fn no_write_through_another_view_is_lost_between_askers() {
    writes_to_views_are_not_lost::<3>(0);
}

fn writes_to_views_are_not_lost<const VIEWS: usize>(rest: u64) {
    let rest = std::time::Duration::from_micros(rest);
    const PAGES: usize = 12;
    const THREADS: usize = 8;
    // SAFETY: views of a section of our own.
    let (section, all): (usize, [usize; VIEWS]) = unsafe {
        let section = CreateFileMappingW(
            -1isize as _,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            (PAGES * PAGE) as u32,
            std::ptr::null(),
        );
        crate::title_memory_section(section as usize, (PAGES * PAGE) as u64, true);
        let all = std::array::from_fn(|_| {
            let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, PAGES * PAGE).Value as usize;
            assert!(at != 0 && at.is_multiple_of(PAGE));
            super::super::title_memory_remapped(at, PAGES * PAGE);
            crate::title_memory_view(at, PAGES * PAGE, section as usize, 0);
            at
        });
        (section as usize, all)
    };
    let by_bits = std::sync::atomic::AtomicUsize::new(0);
    let past = std::sync::atomic::AtomicUsize::new(0);
    let title_beside = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        if VIEWS > 1 {
            s.spawn(move || {
                REST_IN_TEST.set(rest);
                let mut last = [0u64; VIEWS];
                for round in 0..300usize {
                    let (page, asked) = (round * 5 % PAGES, round % VIEWS);
                    // SAFETY (all): a view of one page of the section,
                    // this thread's alone.
                    let one = unsafe {
                        MapViewOfFile(section as _, FILE_MAP_WRITE, 0, (page * PAGE) as u32, PAGE).Value as usize
                    };
                    assert_ne!(one, 0);
                    super::super::title_memory_remapped(one, PAGE);
                    crate::title_memory_view(one, PAGE, section, (page * PAGE) as u64);
                    unsafe { ((one + THREADS * 8) as *mut u64).write_volatile(round as u64) };
                    let (now, written) =
                        writes(all[asked] + page * PAGE, all[asked] + (page + 1) * PAGE, true).expect("a view");
                    let number = written.first().map_or(0, |w| w.1);
                    assert!(number > last[asked], "page {page}, through a new view: {number} is not after {}", last[asked]);
                    last[asked] = now.max(last[asked]);
                    {
                        let _changing = title_memory_remapping();
                        title_memory_unmapping(one, PAGE);
                        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: one as *mut _ }) };
                    }
                    last = [last[asked]; VIEWS];
                }
            });
        }
        for t in 0..THREADS {
            let (by_bits, past, title_beside) = (&by_bits, &past, &title_beside);
            s.spawn(move || {
                REST_IN_TEST.set(rest);
                let mut last = [[0u64; PAGES]; VIEWS];
                let mut seed = t as u64 * 7919 + 1;
                let mut next = |n: usize| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (seed >> 33) as usize % n
                };
                for _ in 0..500 {
                    let page = next(PAGES);
                    let from = page - next(page.min(3) + 1);
                    let to = page + 1 + next((PAGES - page).min(3));
                    // Written through one view, asked about at another.
                    let (through, asked) = (all[next(VIEWS)], next(VIEWS));
                    #[allow(unused_assignments)]
                    let mut how = 0;
                    let (at, last) = (all[asked], &mut last[asked]);
                    if next(4) != 0 {
                        // As in the test above, or after a word of ours,
                        // or past the guard.
                        let word = through + page * PAGE + t * 8;
                        // SAFETY (both): as above.
                        // (Waited for: other tests hold changes of mapping.)
                        // (Some as a copy of the title's, or a store of it, does.)
                        if next(3) == 0
                            && put_beside(word / PAGE, 1, |delta| unsafe {
                                (word.wrapping_add(delta) as *mut u64).write_volatile(3);
                            }) == Beside::Done
                        {
                            title_beside.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let (now, written) = writes(at + from * PAGE, at + to * PAGE, true).expect("a view");
                            let number = written.iter().find(|w| w.0 == at / PAGE + page).map_or(0, |w| w.1);
                            assert!(number > last[page], "page {page}, beside: {number} is not after {}", last[page]);
                            for l in &mut last[from..to] {
                                *l = now;
                            }
                            continue;
                        }
                        let reading = (next(3) == 0).then(title_memory_reading);
                        how = usize::from(reading.is_some());
                        let beside = reading.as_ref().and_then(|_| crate::inplace::beside(word, 8));
                        if let Some(to) = beside {
                            unsafe { (to as *mut u64).write_volatile(2) };
                            landed_beside(&[word / 4096], &[(word, 8)]);
                            past.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            assert_eq!(unsafe { (word as *const u64).read_volatile() }, 2, "put elsewhere");
                        } else {
                            how += 2;
                            if next(4) == 0 {
                                writing(word, 8);
                                how += 4;
                            }
                            unsafe { (word as *mut u64).write_volatile(1) };
                        }
                    } else {
                        // Nothing written: what is asked counts as before.
                        by_bits.fetch_add(
                            usize::from(published(at + from * PAGE, at + to * PAGE)),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        let (now, _) = writes(at + from * PAGE, at + to * PAGE, true).expect("a view");
                        for l in &mut last[from..to] {
                            *l = now;
                        }
                        continue;
                    }
                    let (now, written) = writes(at + from * PAGE, at + to * PAGE, true).expect("a view");
                    let number = written.iter().find(|w| w.0 == at / PAGE + page).map_or(0, |w| w.1);
                    assert!(
                        number > last[page],
                        "{VIEWS} views, page {page}, through {:x} ({how}), asked at {:x}: {number} is not after {}",
                        through / PAGE + page,
                        at / PAGE + page,
                        last[page]
                    );
                    for l in &mut last[from..to] {
                        *l = now;
                    }
                }
            });
        }
    });
    assert!(by_bits.into_inner() > 0, "the bits answered none");
    assert!(past.into_inner() > 0, "nothing was put past the guards");
    assert!(title_beside.into_inner() > 0, "nothing of the title's was put in beside the guards");
    // (The view stays: at its address other tests' memory would be told
    // of these writes.)
}

/// A range of a placeholder with views in it is known whole, its holes
/// among it, until something is mapped into one, unmapped from it or
/// allocated there. A hole that is no placeholder's is asked about.
#[test]
fn holes_are_known_until_the_runtime_tells() {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const BLOCKS: usize = 8;
    // SAFETY (all): a placeholder and a section of our own, which stay.
    let (at, section) = unsafe {
        let at = VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            BLOCKS * PAGE,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        ) as usize;
        let section =
            CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, (BLOCKS * PAGE) as u32, std::ptr::null());
        (at, section)
    };
    let map = |block: usize| unsafe {
        let to = at + block * PAGE;
        VirtualFree(to as *mut _, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
        let view = MapViewOfFile3(
            section,
            GetCurrentProcess(),
            to as *const _,
            (block * PAGE) as u64,
            PAGE,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        );
        assert_eq!(view.Value as usize, to);
        super::super::title_memory_remapped(to, PAGE);
    };
    map(1);
    map(5);
    assert!(placeholder(at / PAGE) && placeholder(at / PAGE + 7) && !placeholder(at / PAGE + 5));
    let end = at + BLOCKS * PAGE;
    let ask = |since: u64| -> (u64, Vec<usize>) {
        let (now, written) = writes(at, end, true).expect("a view");
        (now, written.into_iter().filter(|w| w.1 > since).map(|w| w.0 - at / PAGE).collect())
    };
    let (first, all) = ask(0);
    assert_eq!(all, (0..BLOCKS).collect::<Vec<_>>());
    assert!(published(at, end), "with its holes");
    // Written; mapped into; unmapped.
    unsafe { ((at + 5 * PAGE) as *mut u8).write_volatile(1) };
    assert!(!published(at, end));
    let (second, all) = ask(first);
    assert_eq!(all, [5]);
    map(2);
    assert!(!published(at, end));
    let (third, all) = ask(second);
    assert_eq!(all, [2]);
    unsafe { ((at + 2 * PAGE) as *mut u8).write_volatile(1) };
    let (fourth, all) = ask(third);
    assert_eq!(all, [2]);
    title_memory_unmapping(at + 5 * PAGE, PAGE);
    unsafe {
        let view = MEMORY_MAPPED_VIEW_ADDRESS { Value: (at + 5 * PAGE) as *mut _ };
        UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER);
    }
    let (fifth, all) = ask(fourth);
    assert_eq!(all, [5]);
    assert!(published(at, end));
    // Told of memory there, though there is none: asked about again.
    title_memory_allocated(at, PAGE);
    assert!(!published(at, end));
    let (_, all) = ask(fifth);
    assert_eq!(all, []);
    // A reservation of the ordinary kind is no placeholder.
    let plain = unsafe { VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE, PAGE_NOACCESS) } as usize;
    assert!(!placeholder(plain / PAGE));
    look_at_holes();
    assert!(!published(at, end), "block 0 stays asked about");
    assert!(published(at + PAGE, end), "the others are as they were");
    title_memory_unmapping(at, BLOCKS * PAGE);
}

/// Pages mapped into a placeholder, written and unmapped again, each by a
/// thread of its own as the runtime does it, while others ask about all
/// of it: what was written, and that a page came or went, is told to
/// whoever asks after it, and blocks are known by their bits meanwhile.
#[test]
fn no_mapping_is_lost_between_askers() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const BLOCKS: usize = 12;
    // SAFETY (all): a placeholder and a section of our own, which stay.
    let (at, section) = unsafe {
        let at = VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            BLOCKS * PAGE,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        ) as usize;
        for block in 0..BLOCKS - 1 {
            VirtualFree((at + block * PAGE) as *mut _, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
        }
        let section =
            CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, (BLOCKS * PAGE) as u32, std::ptr::null());
        (at, section as usize)
    };
    let end = at + BLOCKS * PAGE;
    let (stop, by_bits) = (AtomicBool::new(false), AtomicUsize::new(0));
    let number = |written: &[(usize, u64)], block: usize| written.iter().find(|w| w.0 == at / PAGE + block).map_or(0, |w| w.1);
    std::thread::scope(|s| {
        let mappers: Vec<_> = (0..4usize)
            .map(|t| {
                let number = &number;
                s.spawn(move || {
                    for round in 0..120 {
                        let block = t * 3 + round % 3;
                        let to = at + block * PAGE;
                        let (before, _) = writes(at, end, true).expect("a view");
                        unsafe {
                            let view = MapViewOfFile3(
                                section as _,
                                GetCurrentProcess(),
                                to as *const _,
                                (block * PAGE) as u64,
                                PAGE,
                                MEM_REPLACE_PLACEHOLDER,
                                PAGE_READWRITE,
                                std::ptr::null_mut(),
                                0,
                            );
                            assert_eq!(view.Value as usize, to);
                            super::super::title_memory_remapped(to, PAGE);
                            (to as *mut u8).write_volatile(round as u8);
                        }
                        let (mapped, written) = writes(at, end, true).expect("a view");
                        assert!(number(&written, block) > before, "mapped and written: not told");
                        unsafe { ((to + 9) as *mut u8).write_volatile(round as u8) };
                        let (again, written) = writes(to, to + PAGE, true).expect("a view");
                        assert!(number(&written, block) > mapped, "written again: not told");
                        title_memory_unmapping(to, PAGE);
                        unsafe {
                            let view = MEMORY_MAPPED_VIEW_ADDRESS { Value: to as *mut _ };
                            assert_ne!(UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER), 0);
                        }
                        let _ = again;
                    }
                })
            })
            .collect();
        for t in 0..3usize {
            let (stop, by_bits) = (&stop, &by_bits);
            s.spawn(move || {
                let mut k = t;
                while !stop.load(Ordering::Relaxed) {
                    k = (k * 5 + 3) % 64;
                    let (from, to) = (at + k % 6 * PAGE, end - k / 8 % 5 * PAGE);
                    by_bits.fetch_add(usize::from(published(from, to)), Ordering::Relaxed);
                    assert!(writes(from, to, true).is_some());
                }
            });
        }
        let failed = mappers.into_iter().map(|m| m.join()).filter(Result::is_err).count();
        stop.store(true, Ordering::Relaxed);
        assert_eq!(failed, 0);
    });
    assert!(by_bits.into_inner() > 0, "the bits answered none");
    // All unmapped: known whole, and nothing but holes.
    writes(at, end, true).expect("a view");
    assert!(published(at, end));
    title_memory_unmapping(at, BLOCKS * PAGE);
}

/// A block's bit goes with everything that makes it the title's to write,
/// or forgets it.
#[test]
fn bits_go_with_the_guard() {
    // SAFETY: a view of a section of our own, which stays.
    let at = unsafe {
        let section =
            CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, (2 * PAGE) as u32, std::ptr::null());
        MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * PAGE).Value as usize
    };
    let end = at + 2 * PAGE;
    let takes: [&dyn Fn(); 4] = [
        &|| super::super::title_memory_remapped(at, PAGE),
        // SAFETY: the view.
        &|| unsafe { ((at + 8) as *mut u8).write_volatile(1) },
        &|| writing(at, 1),
        // (Told again, as of a view mapped there next: it is read-only.)
        &|| {
            title_memory_unmapping(at, PAGE);
            assert!(!published(at, end), "forgotten");
            super::super::title_memory_remapped(at, PAGE);
            let mut old = 0;
            // SAFETY: the view.
            unsafe { VirtualProtect(at as *const _, PAGE, PAGE_READWRITE, &mut old) };
        },
    ];
    for (i, take) in takes.iter().enumerate() {
        writes(at, end, true).expect("a view");
        assert!(published(at, end), "{i}");
        take();
        assert!(!published(at, end), "{i}");
        assert!(published(at + PAGE, end), "{i}: the other block");
    }
    title_memory_unmapping(at, 2 * PAGE);
}

/// A range's bits are all of its blocks', whatever lies beside them.
#[test]
fn bits_of_ranges() {
    static BITS: Bits = Bits::new();
    let blocks = [0usize, 1, 63, 64, 65, 127, 128, 200, 65535, 65536, 65537, 1 << 20, (1 << 31) - 1];
    for &b in &blocks {
        assert!(!BITS.all(b, b + 1));
        BITS.set(b);
        assert!(BITS.all(b, b + 1));
    }
    assert!(BITS.all(0, 2) && BITS.all(63, 66) && BITS.all(127, 129) && BITS.all(65535, 65538));
    assert!(!BITS.all(0, 3) && !BITS.all(62, 64) && !BITS.all(65534, 65536) && !BITS.all(5, 5));
    for b in 300..700 {
        BITS.set(b);
    }
    assert!(BITS.all(300, 700) && !BITS.all(299, 700) && !BITS.all(300, 701));
    BITS.clear(511);
    assert!(!BITS.all(300, 700) && BITS.all(300, 511) && BITS.all(512, 700));
    assert!(!BITS.all(1 << 40, (1 << 40) + 1), "outside the address space");
}
