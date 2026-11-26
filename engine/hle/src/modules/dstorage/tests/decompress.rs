use crate::modules::xgameruntime::S_OK;

use super::super::execute::{inflate, into_destination};
use super::super::{INVALID_DATA, MEMORY_SOURCE, PAGE, PAGE_DESTINATION, ZLIB};
use super::{
    BC4_STREAM, bc4_blocks, compress_zlib, enqueue, enqueue_status, file_with, new_queue, pattern,
    request, status_array, void, wait, with_test_tables,
};

#[test]
fn a_list_that_changes_during_the_write_has_its_new_pages_written() {
    use crate::modules::xmem;
    let mut pages = [0usize; 4];
    let mut n = 4;
    assert_eq!(xmem::XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
    // In place (consecutive pages) and through a buffer.
    for old in [[pages[0], pages[1]], [pages[1], pages[0]]] {
        let new = [pages[3], pages[2]];
        let mut list = old;
        let at = list.as_mut_ptr();
        let len = PAGE + 100;
        let r = request(PAGE_DESTINATION, at.cast(), len, std::ptr::null(), 0, len);
        let mut runs = 0;
        let written = into_destination(&r, len, |out| {
            runs += 1;
            out.fill(0xa5);
            // SAFETY: our own list.
            unsafe { at.cast::<[usize; 2]>().write_volatile(new) };
            Ok(out.len())
        });
        assert_eq!((written, runs), (Ok(()), 2));
        assert!(xmem::page_window(&new[..1], 0, PAGE).unwrap().bytes().iter().all(|&b| b == 0xa5));
        assert!(xmem::page_window(&new[1..], 0, 100).unwrap().bytes().iter().all(|&b| b == 0xa5));
        for page in new {
            xmem::page_window(&[page], 0, PAGE).unwrap().bytes().fill(0);
        }
    }
}

#[test]
fn zlib_then_bcpack() {
    with_test_tables();
    let packed = compress_zlib(&BC4_STREAM);
    let (_dir, file) = file_with(&packed);
    let (queue, array) = (new_queue(0), status_array(1));
    let mut out = [0u8; 16];
    let options = ZLIB | 4 << 1;
    let mut r = request(options, out.as_mut_ptr().cast(), 16, file, 0, packed.len());
    r.intermediate_size = BC4_STREAM.len() as u32;
    enqueue(queue, &r);
    enqueue_status(queue, array, 0);
    void(queue, 0x30);
    assert_eq!(wait(array, 0), S_OK);
    assert_eq!(out, bc4_blocks());
}

#[test]
fn bcpack_requests_unpack_into_the_destination() {
    with_test_tables();
    let mut contents = pattern(100);
    contents.extend_from_slice(&BC4_STREAM);
    let (_dir, file) = file_with(&contents);
    let (queue, array) = (new_queue(0), status_array(1));
    let mut out = [0u8; 16];
    enqueue(queue, &request(4 << 1, out.as_mut_ptr().cast(), 16, file, 100, 88));
    enqueue_status(queue, array, 0);
    void(queue, 0x30);
    assert_eq!(wait(array, 0), S_OK);
    assert_eq!(out, bc4_blocks());
}

#[test]
fn physical_page_destinations() {
    use crate::modules::xmem;
    let mut pages = [0usize; 2];
    let mut n = 2;
    assert_eq!(
        xmem::XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()),
        1
    );
    // Where the title would see them: a reservation of 64 KiB pages.
    let mapped = xmem::XMemVirtualAlloc(
        std::ptr::null_mut(),
        2 * PAGE,
        0x2000 | 0x2040_0000, // reserve, 64 KiB pages
        0,
        0x04,
    );
    assert!(!mapped.is_null());
    assert_eq!(
        xmem::XMemMapPhysicalPages(mapped, 2, pages.as_ptr()),
        mapped
    );

    let plain = pattern(100);
    let (queue, array) = (new_queue(1), status_array(1));
    let mut r = request(
        PAGE_DESTINATION | MEMORY_SOURCE,
        pages.as_mut_ptr().cast(),
        plain.len(),
        plain.as_ptr().cast(),
        0,
        plain.len(),
    );
    r.page_offset = (PAGE - 40) as u64; // across the page boundary
    enqueue(queue, &r);
    enqueue_status(queue, array, 0);
    void(queue, 0x30);
    assert_eq!(wait(array, 0), S_OK);
    // SAFETY: both pages are mapped read-write.
    let seen = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>().add(PAGE - 40), 100) };
    assert_eq!(seen, plain);
}

#[test]
fn zlib_into_pages_in_place_and_across_allocations() {
    use crate::modules::xmem;
    let allocate = |n: usize| {
        let (mut pages, mut given) = (vec![0usize; n], n);
        assert_eq!(
            xmem::XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        pages
    };
    let (a, b) = (allocate(3), allocate(1));
    let plain = pattern(2 * PAGE + 100);
    let packed = compress_zlib(&plain);
    // One allocation's pages in order (one window), then pages of two
    // (through a buffer), both from 50 bytes into the first.
    let mut destinations = [a.clone(), vec![a[0], a[1], b[0]]];
    let (queue, array) = (new_queue(1), status_array(1));
    for pages in &mut destinations {
        let mut r = request(
            ZLIB | PAGE_DESTINATION | MEMORY_SOURCE,
            pages.as_mut_ptr().cast(),
            plain.len(),
            packed.as_ptr().cast(),
            0,
            packed.len(),
        );
        r.page_offset = 50;
        enqueue(queue, &r);
    }
    enqueue_status(queue, array, 0);
    void(queue, 0x30);
    assert_eq!(wait(array, 0), S_OK);
    for pages in &destinations {
        let mapped = xmem::XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            0x2000 | 0x2040_0000, // reserve, 64 KiB pages
            0,
            0x04,
        );
        assert!(!mapped.is_null());
        assert_eq!(
            xmem::XMemMapPhysicalPages(mapped, 3, pages.as_ptr()),
            mapped
        );
        // SAFETY: all three pages are mapped read-write.
        let seen = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), 3 * PAGE) };
        assert!(seen[..50].iter().all(|&b| b == 0));
        assert_eq!(seen[50..50 + plain.len()], plain);
        assert_eq!(
            xmem::XMemMapPhysicalPages(mapped, 3, std::ptr::null()),
            mapped
        );
    }
    assert_eq!(xmem::XMemFreePhysicalPages(3, a.as_ptr()), 1);
    assert_eq!(xmem::XMemFreePhysicalPages(1, b.as_ptr()), 1);
}

#[test]
fn zlib_inflate_handles_short_exact_empty_and_bad_streams() {
    let plain = pattern(65_537);
    let packed = compress_zlib(&plain);

    // Exact fit.
    let mut exact = vec![0u8; plain.len()];
    assert_eq!(inflate(&packed, &mut exact), Ok(plain.len()));
    assert_eq!(exact, plain);

    // Short stream into a larger buffer passes and returns bytes written;
    // guard bytes outside the slice passed to `inflate` stay untouched.
    let mut larger = vec![0xAA; plain.len() + 64];
    assert_eq!(
        inflate(&packed, &mut larger[..plain.len() + 32]),
        Ok(plain.len())
    );
    assert_eq!(&larger[..plain.len()], &plain[..]);
    assert!(larger[plain.len() + 32..].iter().all(|&b| b == 0xAA));

    // Empty valid stream passes for empty or non-empty destinations.
    let empty = compress_zlib(&[]);
    assert_eq!(inflate(&empty, &mut []), Ok(0));
    let mut scratch = [0x55u8; 4];
    assert_eq!(inflate(&empty, &mut scratch), Ok(0));

    // Oversized stream (destination too small, including empty) fails.
    let mut small = vec![0u8; plain.len() - 1];
    assert_eq!(inflate(&packed, &mut small), Err(INVALID_DATA));
    assert_eq!(inflate(&packed, &mut []), Err(INVALID_DATA));

    // Truncated stream (missing trailer or payload bytes) fails.
    let mut out = vec![0u8; plain.len()];
    assert_eq!(
        inflate(&packed[..packed.len() - 1], &mut out),
        Err(INVALID_DATA)
    );
    assert_eq!(
        inflate(&packed[..packed.len() / 2], &mut out),
        Err(INVALID_DATA)
    );
    assert_eq!(inflate(&[], &mut out), Err(INVALID_DATA));

    // Corrupt Adler-32 trailer fails.
    let mut bad_adler = packed.clone();
    *bad_adler.last_mut().unwrap() ^= 1;
    assert_eq!(inflate(&bad_adler, &mut out), Err(INVALID_DATA));

    // Corrupt header/payload fails, and subsequent valid inflate on the
    // same thread still succeeds (verifies thread-local state reset).
    let mut bad_body = packed.clone();
    bad_body[2] ^= 0xFF;
    assert_eq!(inflate(&bad_body, &mut out), Err(INVALID_DATA));
    assert_eq!(inflate(&packed, &mut out), Ok(plain.len()));
    assert_eq!(out, plain);
}
