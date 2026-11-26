use std::ffi::c_void;
use std::sync::atomic::Ordering;

use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_READWRITE};

use super::super::alloc::{PAGES_64K, XMemVirtualAlloc};
use super::super::mapping::{XMemMapPhysicalPages, XMemMapPhysicalPagesScatter};
use super::super::physical::{PAGE, XMemFreePhysicalPages};
use super::super::physical_io::{
    page_window, pages_known, pages_written, runs, shown_at, write_physical,
};
use super::{allocate, page_bytes};

/// What a title that maps and unmaps pages all the time costs the
/// workers that write others: 24 threads look pages up, fill one and
/// tell of it, as a request does, while one maps.
/// `cargo test --release -p oathkeep-hle --lib map_bench -- --ignored --nocapture`
#[test]
#[ignore = "a benchmark"]
fn map_bench() {
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};
    let pages = allocate(1024);
    let a = XMemVirtualAlloc(std::ptr::null_mut(), 1024 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE) as usize;
    for mapping in [false, true] {
        let (done, waited, worst, maps) =
            (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
        let until = Instant::now() + Duration::from_secs(3);
        std::thread::scope(|s| {
            if mapping {
                s.spawn(|| {
                    let at: Vec<*mut c_void> = (0..256).map(|i| (a + i * PAGE) as *mut c_void).collect();
                    while Instant::now() < until {
                        assert_eq!(XMemMapPhysicalPagesScatter(at.as_ptr(), 256, pages.as_ptr()), 1);
                        assert_eq!(XMemMapPhysicalPagesScatter(at.as_ptr(), 256, std::ptr::null()), 1);
                        maps.fetch_add(512, Ordering::Relaxed);
                    }
                });
            }
            for t in 0..24 {
                let (pages, done, waited, worst) = (&pages, &done, &waited, &worst);
                s.spawn(move || {
                    let mut i = 256 + t * 16;
                    while Instant::now() < until {
                        let page = &pages[i..=i];
                        i = if i == 256 + t * 16 + 15 { 256 + t * 16 } else { i + 1 };
                        let began = Instant::now();
                        assert!(pages_known(page, 0, PAGE));
                        let mut window = page_window(page, 0, PAGE).unwrap();
                        let looked = began.elapsed();
                        window.bytes().fill(t as u8);
                        let began = Instant::now();
                        pages_written(page, 0, PAGE);
                        let took = (looked + began.elapsed()).as_nanos() as u64;
                        waited.fetch_add(took, Ordering::Relaxed);
                        worst.fetch_max(took, Ordering::Relaxed);
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        let n = done.load(Ordering::Relaxed).max(1);
        println!(
            "mapping {mapping}: {:.0} requests/s, {:.1} us of each on lookups and notices (worst {:.1} ms); {:.0} maps/s",
            n as f64 / 3.0,
            waited.load(Ordering::Relaxed) as f64 / n as f64 / 1e3,
            worst.load(Ordering::Relaxed) as f64 / 1e6,
            maps.load(Ordering::Relaxed) as f64 / 3.0
        );
    }
    assert_eq!(XMemFreePhysicalPages(1024, pages.as_ptr()), 1);
}

#[test]
fn runs_group_consecutive_pages_of_one_allocation() {
    let (a, b) = (allocate(3), allocate(1));
    let lengths = |pages: &[usize], offset, len| {
        runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
    };
    assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
    // Only the pages the bytes reach.
    assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
    assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
    assert_eq!(lengths(&a, 0, 0), Some(vec![]));
    // Out of order, or across allocations: one run each.
    assert_eq!(
        lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
        Some(vec![1, 1, 1])
    );
    assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
    // A slice that follows on in another section starts a run.
    assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
    // Too few pages for the bytes, or an unknown one.
    assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
    assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
    assert_eq!(lengths(&a, usize::MAX, 2), None);
    assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
    assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
}

#[test]
fn writes_cross_runs_and_allocations() {
    let (a, b) = (allocate(2), allocate(1));
    let pages = [a[0], a[1], b[0]];
    let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
    let offset = PAGE - 32;
    // Past the third page: the second run is too short.
    assert!(!write_physical(&pages, offset, &data));
    assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
    let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
    assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
    assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
    assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
    assert!(
        !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
        "unknown page"
    );
    assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
    assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
}

/// Pages say where the title has them mapped, byte for byte (after
/// fork PR #571's test; its runs are whole pages).
#[test]
fn pages_say_where_they_are_mapped() {
    let pages = allocate(3);
    let reserve = |n: usize| {
        let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!at.is_null());
        at
    };
    // `b` keeps a page unmapped at either end, so that no run at `b`
    // ever touches one at `a`, wherever the host puts the two.
    let (a, b) = (reserve(3), reserve(4));
    assert!(shown_at(&pages, 0, 3 * PAGE).is_empty(), "mapped nowhere");
    // All three at `a`; the last two at `b`, from its second page on.
    assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
    let second = b.wrapping_byte_add(PAGE);
    assert_eq!(XMemMapPhysicalPages(second, 2, pages[1..].as_ptr()), second);
    let (a, second) = (a as usize, second as usize);
    let sorted = |mut runs: Vec<(usize, usize)>| {
        runs.sort_unstable();
        runs
    };
    assert_eq!(shown_at(&pages, 0, PAGE), vec![(a, PAGE)]);
    // A request for part of a page names those bytes alone.
    assert_eq!(shown_at(&pages, 0x4000, 0x3000), vec![(a + 0x4000, 0x3000)]);
    // Across two pages, shown at two addresses each: one run at each.
    assert_eq!(
        shown_at(&pages, PAGE + 1, PAGE),
        sorted(vec![(a + PAGE + 1, PAGE), (second + 1, PAGE)])
    );
    // A page unmapped in the middle of a view: the rest still shows.
    let middle = (a + PAGE) as *mut c_void;
    assert_eq!(XMemMapPhysicalPages(middle, 1, std::ptr::null()), middle);
    assert_eq!(
        shown_at(&pages, 0, 3 * PAGE),
        sorted(vec![(a, PAGE), (a + 2 * PAGE, PAGE), (second, 2 * PAGE)])
    );
    // Too few pages for the bytes: nothing (the write fails too).
    assert!(shown_at(&pages, 0, 3 * PAGE + 1).is_empty());
    for (at, n) in [(a, 3), (second, 2)] {
        let at = at as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
    }
    assert!(shown_at(&pages, 0, 3 * PAGE).is_empty());
    assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
}

#[test]
fn page_windows_cover_one_run_in_place() {
    let (a, b) = (allocate(3), allocate(1));
    let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
    assert_eq!(window.bytes().len(), PAGE);
    window.bytes().fill(0x5a);
    drop(window);
    let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
    assert!(second[..3].iter().all(|&b| b == 0));
    assert!(second[3..].iter().all(|&b| b == 0x5a));
    assert!(third[..3].iter().all(|&b| b == 0x5a));
    assert!(third[3..].iter().all(|&b| b == 0));
    // Several runs, or an unknown page: no window.
    assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
    assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
    assert!(page_window(&[0x10], 0, 1).is_none());
    // One page of several is still one run.
    assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
    assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
    assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
}
