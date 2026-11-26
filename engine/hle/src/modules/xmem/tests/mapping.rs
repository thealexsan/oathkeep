use std::ffi::c_void;

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualFree,
};

use super::super::alloc::{PAGES_64K, XMemIsToolsMemAvailable, XMemVirtualAlloc};
use super::super::mapping::{
    XMemMapPhysicalPages, XMemMapPhysicalPagesScatter, map_alias, map_aliases,
};
use super::super::physical::{
    PAGE, XMemAllocatePhysicalPages, XMemFreePhysicalPages, physical,
};
use super::super::physical_io::write_physical;
use super::super::pool::busy_after;
use super::{allocate, query};

#[test]
fn physical_pages_map_alias_and_unmap() {
    let mut pages = [0usize; 3];
    let mut n = 3;
    assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
    assert_eq!(n, 3);
    // Packed into 21 bits as `number >> 4`, as the title keeps them.
    for p in pages {
        assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
    }
    // Reservations of 64 KiB pages, as titles make them for mapping.
    let reserve = || {
        XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        )
    };
    let (a, b) = (reserve(), reserve());
    assert!(!a.is_null() && !b.is_null());
    let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
    assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

    // The same page at a second address shows the same bytes.
    assert_eq!(
        XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
        at(b, 1)
    );
    // SAFETY: both pages are mapped read-write.
    unsafe {
        at(a, 2).cast::<u8>().write(7);
        assert_eq!(at(b, 1).cast::<u8>().read(), 7);
        a.cast::<u8>().write(1);
    }
    // Writes into pages (DirectStorage's) show through their views.
    assert!(write_physical(&pages[2..], 1, &[9]));
    // SAFETY: mapped read-write.
    assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

    // Unmapping the middle page keeps its neighbours.
    assert_eq!(
        XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
        at(a, 1)
    );
    assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
    assert_eq!(query(a).host.State, MEM_COMMIT);
    assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
    // SAFETY: still mapped.
    assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

    // A scattered page lands in the hole.
    let addresses = [at(a, 1)];
    assert_eq!(
        XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
        1
    );
    // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
    assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
    // Page number 0 unmaps.
    assert_eq!(
        XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
        1
    );
    assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
    // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
    assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
    // SAFETY: mapped read-write.
    assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
    assert!(map_alias(b as u64, 0, PAGE as u64));
    assert_eq!(query(b).host.State, MEM_RESERVE);
    assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
    assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    assert_eq!(XMemIsToolsMemAvailable(), 0);
}

/// A thread that reads a mapped page in place, as the GPU side reads a
/// texture, never meets the moment in which the runtime has taken the
/// page's view away to map it again: mapping the same pages at the same
/// address unmaps the view and maps a new one, and without the lock the
/// reader faults on a page "reserved, never committed" (the crashes of
/// runs 71 and 74).
#[test]
fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut pages = [0usize; 3];
    let mut n = 3;
    assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
    let a = XMemVirtualAlloc(
        std::ptr::null_mut(),
        3 * PAGE,
        MEM_RESERVE | PAGES_64K,
        0,
        PAGE_READWRITE,
    );
    assert!(!a.is_null());
    assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
    // SAFETY: mapped read-write.
    unsafe { a.cast::<u8>().write(1) };
    let first = a as usize;
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut reads = 0u64;
            // (Once at least: the mapping thread may be done before this
            // one's first turn.)
            while reads == 0 || !stop.load(Ordering::Relaxed) {
                let _reading = oathkeep_gpu::title_memory_reading();
                // SAFETY: the page is mapped whenever no change of
                // mapping is under way, and none is while `_reading`
                // is held.
                assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                reads += 1;
            }
            reads
        })
    };
    for _ in 0..2000 {
        // The same pages at the same address: the view goes and comes
        // back, with the same bytes.
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
    }
    stop.store(true, Ordering::Relaxed);
    assert!(reader.join().expect("the reader never faulted") > 0);
    assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
    assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
}

/// A title thread that reads and writes a page of a view while other
/// pages of the view are unmapped and mapped again, as the console lets
/// it: the page was never given up.
#[test]
fn a_title_thread_goes_on_using_the_rest_of_a_view() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let pages = allocate(4);
    for _ in 0..200 {
        // A new reservation each time: the pieces of a view cut up stay
        // cut up.
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
        let (first, stop) = (a as usize, AtomicBool::new(false));
        // SAFETY: mapped read-write.
        unsafe { (first as *mut u64).write_volatile(0) };
        std::thread::scope(|s| {
            let title = s.spawn(|| {
                let mut n = 0u64;
                // SAFETY (the block): the first page is the title's all
                // the time; a fault would end the process.
                unsafe {
                    while !stop.load(Ordering::Relaxed) {
                        assert_eq!((first as *const u64).read_volatile(), n);
                        n += 1;
                        (first as *mut u64).write_volatile(n);
                    }
                }
                n
            });
            // SAFETY: as above.
            while !title.is_finished() && unsafe { (first as *const u64).read_volatile() } < 100 {}
            for page in [3, 2, 1] {
                let at = a.wrapping_byte_add(page * PAGE);
                assert_eq!(XMemMapPhysicalPages(at, 1, std::ptr::null()), at);
                assert_eq!(XMemMapPhysicalPages(at, 1, pages[page..].as_ptr()), at);
            }
            stop.store(true, Ordering::Relaxed);
            assert!(title.join().expect("the title's thread never faulted") > 0);
        });
        assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
    }
    assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
}

/// Many pages in one call: mapped, moved over each other's addresses,
/// given twice in one list, copied by the GPU's mappings and unmapped.
#[test]
fn a_scatter_of_many_pages_is_what_one_after_the_other_is() {
    const N: usize = 200;
    let pages = allocate(N);
    let a = XMemVirtualAlloc(std::ptr::null_mut(), 2 * N * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE) as usize;
    let at = |k: usize| (a + k * PAGE) as *mut c_void;
    let first = |k: usize| at(k).cast::<u64>();
    for round in 0..4_usize {
        // Every address shows another page each round.
        let to: Vec<*mut c_void> = (0..N).map(|i| at((i + 7 * round) % N)).collect();
        assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), N, pages.as_ptr()), 1);
        for i in 0..N {
            // SAFETY: mapped read-write.
            unsafe {
                if round > 0 {
                    assert_eq!(first((i + 7 * round) % N).read(), (1000 * (round - 1) + i) as u64);
                }
                first((i + 7 * round) % N).write((1000 * round + i) as u64);
            }
        }
    }
    // The GPU's copies of the mappings, in ranges and one by one; a page
    // mapped from one the same call has mapped shows the same.
    let half = (N / 2 * PAGE) as u64;
    let (from, to) = (a as u64, at(N) as u64);
    assert!(map_aliases(&[(to, from, half), (to + half, to, half)]));
    for k in 0..N / 2 {
        // SAFETY: mapped read-write.
        unsafe {
            assert_eq!(first(N + k).read(), first(k).read());
            assert_eq!(first(N + N / 2 + k).read(), first(k).read());
            first(N + k).write(5);
            assert_eq!(first(k).read(), 5);
        }
    }
    // An address twice: the last one counts; page 0 unmaps.
    let to = [at(0), at(1), at(0), at(2), at(3), at(0)];
    let what = [pages[5], pages[6], 0, 0, pages[7], pages[8]];
    assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), 6, what.as_ptr()), 1);
    assert_eq!(query(at(2)).host.State, MEM_RESERVE);
    // SAFETY: mapped read-write; the same page at both.
    unsafe {
        first(0).write(77);
        assert_eq!(XMemMapPhysicalPagesScatter([at(2), at(4), at(6)].as_ptr(), 3, [pages[8]; 3].as_ptr()), 1);
        assert_eq!((first(2).read(), first(4).read(), first(6).read()), (77, 77, 77));
    }
    // A page the title does not own.
    assert_eq!(XMemMapPhysicalPagesScatter([at(0), at(1)].as_ptr(), 2, [pages[0], 1].as_ptr()), 0);
    // All gone, also parts of a larger view.
    assert_eq!(XMemMapPhysicalPages(at(0), 4, std::ptr::null()), at(0));
    // SAFETY: placeholders of ours, nothing mapped in them.
    unsafe { VirtualFree(at(0), 4 * PAGE, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */) };
    assert_eq!(XMemMapPhysicalPages(at(0), 4, pages.as_ptr()), at(0));
    assert!(map_aliases(&[(a as u64, 0, (2 * N * PAGE) as u64)]));
    assert!((0..2 * N).all(|k| query(at(k)).host.State == MEM_RESERVE));
    assert!(physical().views.range(a..a + 2 * N * PAGE).next().is_none());
    assert_eq!(XMemFreePhysicalPages(N, pages.as_ptr()), 1);
}

/// Pages keep what they hold however often they are mapped, moved and
/// unmapped without a commit or a split of their own.
#[test]
fn pages_mapped_again_hold_what_they_held() {
    let pages = allocate(3);
    let a = XMemVirtualAlloc(std::ptr::null_mut(), 8 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
    assert!(!a.is_null());
    let at = |k: usize| a.wrapping_byte_add(k * PAGE);
    for round in 0..6_usize {
        for (k, page) in pages.iter().enumerate() {
            let to = at((round + 2 * k) % 8);
            assert_eq!(XMemMapPhysicalPages(to, 1, page), to);
            let first = to.cast::<u64>();
            // SAFETY: mapped read-write; the far end of the page too.
            unsafe {
                if round > 0 {
                    assert_eq!(first.read(), (round * 16 + k - 16) as u64);
                    assert_eq!(first.add(PAGE / 8 - 1).read(), k as u64);
                }
                first.write((round * 16 + k) as u64);
                first.add(PAGE / 8 - 1).write(k as u64);
            }
        }
        for k in 0..pages.len() {
            let to = at((round + 2 * k) % 8);
            assert_eq!(XMemMapPhysicalPages(to, 1, std::ptr::null()), to);
            assert_eq!(query(to).host.State, MEM_RESERVE);
        }
        // The title joins what it unmapped: the next view splits it anew.
        if round == 3 {
            // SAFETY: placeholders of ours, nothing mapped in them.
            unsafe {
                VirtualFree(a, 8 * PAGE, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */)
            };
        }
    }
    assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
}

#[test]
fn mapping_no_pages_changes_nothing() {
    let pages = allocate(2);
    let a = XMemVirtualAlloc(
        std::ptr::null_mut(),
        2 * PAGE,
        MEM_RESERVE | PAGES_64K,
        0,
        PAGE_READWRITE,
    );
    assert!(!a.is_null());
    assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
    // SAFETY: both pages are mapped read-write.
    unsafe { a.cast::<u8>().write(5) };
    let second = a.wrapping_byte_add(PAGE);

    // A count of 0 at the start of a view, inside it and past it: with
    // pages, without (an unmap), and as the GPU's page-mapping copy.
    for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
        assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
        assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
        assert!(map_alias(at as u64, a as u64, 0));
        assert!(map_alias(at as u64, 0, 0));
    }

    // The view is still the one view it was.
    let view = physical().views.get(&(a as usize)).map(|v| v.0);
    assert_eq!(view, Some(2 * PAGE));
    assert_eq!(query(a).host.State, MEM_COMMIT);
    assert_eq!(query(second).host.State, MEM_COMMIT);
    // SAFETY: still mapped.
    assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

    // A count of 1 still unmaps that page alone.
    assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
    assert_eq!(query(second).host.State, MEM_RESERVE);
    assert_eq!(query(a).host.State, MEM_COMMIT);
    assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
    assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
}

#[test]
fn mapping_is_busy_from_a_quarter_of_the_time_to_a_twentieth() {
    let ms = std::time::Duration::from_millis;
    assert!(!busy_after(false, ms(60), ms(250)) && busy_after(false, ms(70), ms(250)));
    assert!(busy_after(true, ms(15), ms(250)) && !busy_after(true, ms(12), ms(250)));
    // A window without a call, then one short one.
    assert!(!busy_after(true, ms(5), ms(4000)));
}
