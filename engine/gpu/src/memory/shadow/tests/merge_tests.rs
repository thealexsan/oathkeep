use super::{PIECE, back_piece, changed_runs, dword_runs, put_changed, word_runs};

/// A piece put back takes the GPU's bytes where the copy changed and
/// the title's everywhere else, reads and writes title memory only from
/// the first change to the last, and its scratch holds that stretch and
/// no more: not the 64 KiB per range it once was.
#[test]
fn a_piece_put_back_reads_only_its_stretch() {
    let old: Vec<u8> = (0..PIECE).map(|i| (i * 7 + i / 251) as u8).collect();
    // The title wrote 7 and 40,000 since the snapshot; the GPU wrote 5,
    // 9 and 40,000 (the GPU's byte wins where both wrote).
    let mut title_memory = old.clone();
    title_memory[7] ^= 0x11;
    title_memory[40_000] ^= 0x22;
    let mut host = old.clone();
    for at in [5, 9] {
        host[at] ^= 0x33;
    }
    let gpu = host.clone();
    let mut scratch = Vec::new();
    let (mut mask, mut runs) = (Vec::new(), Vec::new());
    let mut asked = None;
    let read = |first: usize, into: &mut [u8]| {
        asked = Some((first, into.len()));
        into.copy_from_slice(&title_memory[first..first + into.len()]);
        true
    };
    assert_eq!(back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read), Some((5, 5)));
    assert_eq!(asked, Some((5, 5)));
    let mut wanted = title_memory[5..10].to_vec();
    wanted[0] = gpu[5];
    wanted[4] = gpu[9];
    assert_eq!(scratch, wanted);
    assert_eq!(host[5..10], wanted);
    // Only the GPU's two bytes are written back (upstream #2009).
    assert_eq!(runs, [(0, 1), (4, 1)]);
    // The copy outside the stretch is left as it was.
    assert!(host[..5] == gpu[..5] && host[10..] == gpu[10..]);
    assert!(scratch.capacity() < PIECE / 2, "scratch of {} bytes for a stretch of 5", scratch.capacity());

    // Nothing differs: title memory is not read.
    let mut same = old.clone();
    assert_eq!(back_piece(&mut same, &old, &mut scratch, &mut mask, &mut runs, |_, _| unreachable!()), None);
    // Title memory not there: nothing to write, the copy unchanged.
    let mut host = gpu.clone();
    assert_eq!(back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, |_, _| false), None);
    assert!(host == gpu);
    // The GPU wrote what the title holds already: nothing to write.
    let mut title_memory = old.clone();
    title_memory[5] = gpu[5];
    title_memory[9] = gpu[9];
    let mut host = gpu.clone();
    let read = |first: usize, into: &mut [u8]| {
        into.copy_from_slice(&title_memory[first..first + into.len()]);
        true
    };
    assert_eq!(back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read), None);
    // A whole piece changed: the scratch grows to it.
    let mut host: Vec<u8> = old.iter().map(|b| !b).collect();
    assert_eq!(back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, |_, into| into.len() == PIECE), Some((0, PIECE)));
    assert_eq!(scratch.len(), PIECE);
}

/// A byte the shader and the title both changed while the GPU ran keeps
/// the title's (the shader computed it from the copy's older data); a
/// byte only the shader changed goes back, and only such bytes are in
/// the runs to write (PalmVIDev e0b70476, upstream #2009).
#[test]
fn a_byte_both_changed_keeps_the_titles_and_only_gpu_bytes_are_written() {
    let old = vec![1u8, 2, 3, 4, 5, 6];
    let mut host = old.clone();
    host[1] = 20; // the GPU alone
    host[3] = 40; // both
    host[4] = 50; // the GPU alone
    let mut title_memory = old.clone();
    title_memory[3] = 44;
    title_memory[5] = 66; // the title alone
    let (mut scratch, mut mask, mut runs) = (Vec::new(), Vec::new(), Vec::new());
    let read = |first: usize, into: &mut [u8]| {
        into.copy_from_slice(&title_memory[first..first + into.len()]);
        true
    };
    assert_eq!(back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read), Some((1, 4)));
    assert_eq!(scratch, [20, 3, 44, 50], "the title's byte where both changed");
    assert_eq!(host[1..5], [20, 3, 44, 50], "the copy takes the result");
    assert_eq!(runs, [(0, 1), (3, 1)], "the GPU's bytes only");
}


/// What shaders wrote reaches a page that is away only while the runtime
/// changes a mapping; a page that stays away loses it, and nothing else.
#[test]
fn a_put_waits_for_a_change_of_mapping() {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    // SAFETY: a reservation of ours, nothing of it committed.
    let at = unsafe { VirtualAlloc(std::ptr::null(), 1 << 16, MEM_RESERVE, PAGE_NOACCESS) } as usize;
    assert_ne!(at, 0);
    put_changed(at, &[0; 64], &[3; 64]);
    let (began, wait) = std::sync::mpsc::channel();
    let change = std::thread::spawn(move || {
        let _changing = crate::memory::watch::title_memory_remapping();
        began.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: the reservation's first page.
        assert!(!unsafe { VirtualAlloc(at as _, 4096, MEM_COMMIT, PAGE_READWRITE) }.is_null());
    });
    wait.recv().unwrap();
    put_changed(at, &[0; 64], &[1; 64]);
    change.join().unwrap();
    // SAFETY: committed by now.
    assert_eq!(unsafe { std::slice::from_raw_parts(at as *const u8, 64) }, [1; 64]);
    // SAFETY: our reservation.
    unsafe { VirtualFree(at as _, 0, MEM_RELEASE) };
}

/// Whole dwords around every changed byte and no other, whatever the
/// alignment; small gaps joined only when asked.
#[test]
fn dword_runs_hold_the_dwords_that_changed() {
    let title: Vec<u8> = (0..200u32).map(|i| (i * 7 + 3) as u8).collect();
    for at in [0x1000usize, 0x1001, 0x1002, 0x1003] {
        for changes in [vec![0], vec![199], vec![5, 6], vec![8, 12, 40, 41, 43, 120]] {
            let mut merged = title.clone();
            for &c in &changes {
                merged[c] ^= 0x55;
            }
            let runs = dword_runs(at, &title, &merged, 0);
            let held: Vec<usize> = runs.iter().flat_map(|&(a, n)| a..a + n).collect();
            let mut expect: Vec<usize> = (0..200).filter(|i| changes.iter().any(|c| (at + c) / 4 == (at + i) / 4)).collect();
            expect.dedup();
            assert_eq!(held, expect, "{at:#x} {changes:?}");
            let joined = dword_runs(at, &title, &merged, 32);
            assert!(joined.len() <= runs.len());
            assert!(runs.iter().all(|r| joined.iter().any(|j| j.0 <= r.0 && r.0 + r.1 <= j.0 + j.1)));
        }
    }
    assert!(dword_runs(0, &title, &title, 0).is_empty());
}

/// The runs hold exactly the bytes that differ: putting them into the
/// old bytes gives the new ones, and no run holds an equal byte.
/// What looking at a page that did not change costs, against the look
/// eight bytes at a time that it was (a frame has some 250 of them, on
/// the submitting thread). Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn runs_of_equal_pages() {
    let (a, b) = (vec![7u8; 64 << 10], vec![7u8; 64 << 10]);
    let eights = || {
        let (now, was) = (std::hint::black_box(&a), std::hint::black_box(&b));
        let mut i = 0;
        while i + 8 <= now.len() && now[i..i + 8] == was[i..i + 8] {
            i += 8;
        }
        std::hint::black_box(i);
    };
    let best = |f: &dyn Fn()| {
        (0..500)
            .map(|_| {
                let t = std::time::Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e6
            })
            .fold(f64::MAX, f64::min)
    };
    let runs = || drop(std::hint::black_box(changed_runs(std::hint::black_box(&a), std::hint::black_box(&b))));
    println!("64 KiB that are the same: {:.1} us by eights, {:.1} us now", best(&eights), best(&runs));
}

#[test]
fn word_runs_are_the_dword_runs() {
    let mut seed = 7u32;
    let mut random = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        seed >> 8
    };
    for round in 0..200 {
        let n = 4 * (random() as usize % 400);
        let was: Vec<u8> = (0..n).map(|_| random() as u8).collect();
        let mut now = was.clone();
        for _ in 0..random() % 200 {
            if n == 0 {
                break;
            }
            let at = random() as usize % n;
            let len = (random() as usize % 24).min(n - at);
            for b in &mut now[at..at + len] {
                *b = random() as u8;
            }
        }
        assert_eq!(word_runs(0x1000, &now, &was), dword_runs(0x1000, &now, &was, 0), "round {round}");
    }
}

#[test]
fn changed_runs_are_exactly_the_bytes_that_differ() {
    let was: Vec<u8> = (0..300u32).map(|i| (i * 13 + 5) as u8).collect();
    for changes in [vec![], vec![0], vec![299], vec![7, 8, 9], vec![7, 9], vec![15, 16, 17, 23, 24, 64, 65, 66, 67, 68, 69, 70, 71, 72, 298, 299]] {
        let mut now = was.clone();
        for &at in &changes {
            now[at] ^= 0x41;
        }
        let runs = changed_runs(&now, &was);
        let mut put = was.clone();
        let mut held = Vec::new();
        for &(at, n) in &runs {
            assert!(n > 0);
            put[at..at + n].copy_from_slice(&now[at..at + n]);
            held.extend(at..at + n);
        }
        assert_eq!(put, now, "{changes:?}");
        assert_eq!(held, changes, "{changes:?}");
        // Runs do not touch: two that did would be one.
        assert!(runs.windows(2).all(|w| w[0].0 + w[0].1 < w[1].0), "{changes:?}");
    }
    assert!(changed_runs(&[], &[]).is_empty());
}
