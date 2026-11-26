use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::modules::xasync::E_PENDING;
use crate::modules::xgameruntime::S_OK;

use super::super::execute::execute;
use super::super::factory::QueueDesc;
use super::super::files::{Files, Io};
use super::super::{MEMORY_SOURCE, Request, ZLIB};
use super::{
    compress_zlib, enqueue, enqueue_status, file_with, new_queue, pattern, request, result,
    status_array, void, wait,
};

#[test]
fn request_layout_is_the_titles() {
    assert_eq!(size_of::<Request>(), 0x48);
    assert_eq!(std::mem::offset_of!(Request, destination_size), 0x18);
    assert_eq!(std::mem::offset_of!(Request, source), 0x20);
    assert_eq!(std::mem::offset_of!(Request, source_size), 0x30);
    assert_eq!(std::mem::offset_of!(Request, name), 0x40);
    assert_eq!(std::mem::offset_of!(QueueDesc, unknown), 0x18);
}

#[test]
fn raw_and_zlib_reads_from_a_file() {
    let plain = pattern(300_000);
    let packed = compress_zlib(&plain);
    let mut contents = plain.clone();
    contents.extend_from_slice(&packed);
    let (_dir, file) = file_with(&contents);
    let (queue, array) = (new_queue(0), status_array(1));

    let mut raw = vec![0u8; 1000];
    let mut inflated = vec![0u8; plain.len()];
    let at = |b: &mut Vec<u8>| b.as_mut_ptr().cast();
    enqueue(queue, &request(0, at(&mut raw), 1000, file, 5000, 1000));
    let offset = plain.len() as u64;
    let zlib = request(
        ZLIB,
        at(&mut inflated),
        plain.len(),
        file,
        offset,
        packed.len(),
    );
    enqueue(queue, &zlib);
    enqueue_status(queue, array, 0);
    assert_eq!(result(array, 0), E_PENDING, "pending until done");
    void(queue, 0x30);
    assert_eq!(wait(array, 0), S_OK);
    assert_eq!(raw, plain[5000..6000]);
    assert_eq!(inflated, plain);
    void(queue, 0x40);
}

#[test]
fn memory_sources_inflate_too() {
    let plain = pattern(70_000);
    let packed = compress_zlib(&plain);
    let (queue, array) = (new_queue(1), status_array(1));
    let mut out = vec![0u8; plain.len()];
    let options = ZLIB | MEMORY_SOURCE;
    let r = request(
        options,
        out.as_mut_ptr().cast(),
        out.len(),
        packed.as_ptr().cast(),
        0,
        packed.len(),
    );
    enqueue(queue, &r);
    enqueue_status(queue, array, 0);
    void(queue, 0x30);
    assert_eq!(wait(array, 0), S_OK);
    assert_eq!(out, plain);
}

#[test]
fn a_read_into_memory_guarded_for_a_moment_is_tried_again() {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
    };
    let plain = pattern(3 * 4096);
    let (_dir, file) = file_with(&plain);
    // SAFETY: a fresh allocation; its middle page is read-only until the
    // thread opens it, as a guard of the GPU side's would be.
    let at = unsafe {
        VirtualAlloc(std::ptr::null(), plain.len(), MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
    } as usize;
    assert_ne!(at, 0);
    let protect = move |to| {
        let mut old = 0;
        // SAFETY: as above.
        unsafe { VirtualProtect((at + 4096) as *const _, 4096, to, &mut old) }
    };
    assert_ne!(protect(PAGE_READONLY), 0);
    let opener = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        protect(PAGE_READWRITE)
    });
    let r = request(0, at as *mut c_void, plain.len(), file, 0, plain.len());
    assert_eq!(execute(&r), Ok(()));
    assert_ne!(opener.join().unwrap(), 0);
    // SAFETY: as above, all of it readable.
    assert_eq!(unsafe { std::slice::from_raw_parts(at as *const u8, plain.len()) }, plain);
}

/// What ways of reading a file give, at `OATHKEEP_BENCH_FILE` (a file of
/// the title's, only read): 24 threads, requests of 48 KiB at random,
/// first from places nobody read lately, then the same once more.
/// `OATHKEEP_BENCH_FILE=... cargo test --release -p oathkeep-hle --lib
/// read_bench -- --ignored --nocapture`
#[test]
#[ignore = "a benchmark"]
fn read_bench() {
    const THREADS: usize = 24;
    const SIZE: usize = 48 << 10;
    const READS: usize = 12_000;
    let path = std::env::var("OATHKEEP_BENCH_FILE").expect("OATHKEEP_BENCH_FILE");
    let len = std::fs::metadata(&path).unwrap().len();
    let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 | 1;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..2 {
        for mode in [Io::One, Io::Handles, Io::Overlapped, Io::Direct] {
            let file = Files::open_for_test(&path, mode);
            let at: Vec<u64> = (0..READS).map(|_| random() % (len - SIZE as u64) & !15).collect();
            for pass in ["cold", "again"] {
                let next = std::sync::atomic::AtomicUsize::new(0);
                let busy = AtomicU64::new(0);
                let start = Instant::now();
                std::thread::scope(|s| {
                    for _ in 0..THREADS {
                        s.spawn(|| {
                            let mut out = vec![0u8; SIZE];
                            while let Some(&at) = at.get(next.fetch_add(1, Ordering::Relaxed)) {
                                let began = Instant::now();
                                let mut done = 0;
                                while done < SIZE {
                                    done += file.read_at(&mut out[done..], at + done as u64).unwrap();
                                }
                                busy.fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                            }
                        });
                    }
                });
                let took = start.elapsed().as_secs_f64();
                println!(
                    "{round} {mode:?} {pass}: {:.0} MB/s, {:.2} ms a read",
                    (READS * SIZE) as f64 / took / 1e6,
                    busy.load(Ordering::Relaxed) as f64 / READS as f64 / 1e3
                );
            }
        }
    }
}

/// Every way of reading a file gives its bytes, also past its end.
#[test]
fn every_way_of_reading_gives_the_same_bytes() {
    let plain = pattern(300_001);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.bin");
    std::fs::write(&path, &plain).unwrap();
    for mode in [Io::One, Io::Handles, Io::Overlapped, Io::Direct] {
        let file = Files::open_for_test(path.to_str().unwrap(), mode);
        for (at, len) in [(0, 4096), (1, 100), (4095, 2), (8192, 200_000), (12_345, 250_000), (299_990, 11), (300_000, 1)] {
            let mut out = vec![0u8; len];
            let mut done = 0;
            while done < len {
                let n = file.read_at(&mut out[done..], (at + done) as u64).unwrap();
                assert_ne!(n, 0, "{mode:?} {at} {len}");
                done += n;
            }
            assert!(out == plain[at..at + len], "{mode:?} {at} {len}");
        }
        let mut out = [0u8; 16];
        assert_eq!(file.read_at(&mut out, 299_999).unwrap(), 2, "{mode:?}");
        assert_eq!(file.read_at(&mut out, 300_001).unwrap(), 0, "{mode:?}");
        assert_eq!(file.read_at(&mut out, 400_000).unwrap(), 0, "{mode:?}");
    }
}
