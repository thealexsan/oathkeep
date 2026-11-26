use super::*;

/// Whatever the steps, done level by level (in a level every read
/// before every write) they read and leave what they read and leave one
/// after the other, the CPU's steps ahead of all of them.
#[test]
fn levels_do_what_the_order_does() {
    let mut seed = 12345u32;
    let mut random = move |n: u32| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        (seed >> 16) % n
    };
    let (mut done, mut refused) = (0, 0);
    for _ in 0..4000 {
        let does: Vec<Does> = (0..2 + random(9))
            .map(|_| {
                let (at, other, len) = (random(60) as usize, random(60) as usize, 1 + random(8) as usize);
                let asked = random(4) == 0;
                match random(8) {
                    0..4 => copy(at, other, len, asked),
                    4..6 => fill(at, len, asked),
                    6 => stuck(at, len),
                    _ => Does::Read { from: at, to: at + len, asked: false },
                }
            })
            .collect();
        let Ok(chosen) = plan(&does, 2, true, &mut None) else {
            refused += 1;
            continue;
        };
        done += 1;
        let start: Vec<u32> = (0..80).collect();
        // One after the other: what each reads, and what is left.
        let mut memory = start.clone();
        let mut wanted = Vec::new();
        for d in &does {
            let (from, to) = match *d {
                Does::Read { from, to, .. } => (from, to),
                _ => reads_writes(d).unwrap().0,
            };
            wanted.push(memory[from..to].to_vec());
            if let Does::Copy { dst, src, len, .. } = *d {
                memory.copy_within(src..src + len, dst);
            }
        }
        // The CPU's, then the card's.
        let mut cpu = start.clone();
        let taken = |i: usize| chosen.iter().any(|c| c.0 == i);
        for (i, d) in does.iter().enumerate().filter(|&(i, _)| !taken(i)) {
            let (from, to) = match *d {
                Does::Read { from, to, .. } => (from, to),
                _ => reads_writes(d).unwrap().0,
            };
            assert_eq!(cpu[from..to], wanted[i][..], "step {i} of {does:?}");
            if let Does::Copy { dst, src, len, .. } = *d {
                cpu.copy_within(src..src + len, dst);
            }
        }
        let mut card = cpu.clone();
        for level in 0..=chosen.iter().map(|c| c.1).max().unwrap_or(0) {
            let of: Vec<usize> = chosen.iter().filter(|c| c.1 == level).map(|c| c.0).collect();
            let before = card.clone();
            for i in of {
                let ((from, to), (dst, _)) = reads_writes(&does[i]).unwrap();
                assert_eq!(before[from..to], wanted[i][..], "step {i} of {does:?}: {chosen:?}");
                if matches!(does[i], Does::Copy { .. }) {
                    card[dst..dst + to - from].copy_from_slice(&before[from..to]);
                }
            }
        }
        assert_eq!(card, memory, "{does:?}: {chosen:?}");
    }
    assert!(done > 500 && refused > 100, "{done} {refused}");
}

/// What cannot be done on the card refuses the lot, where it stands in
/// the way.
#[test]
fn steps_in_the_way_are_refused() {
    let taken = copy(1000, 5000, 100, true);
    assert!(plan(&[taken, Does::Run], 2, true, &mut None).is_err());
    // A texture that itself waits for flights; a fill that cannot be
    // taken but waits.
    assert!(plan(&[taken, Does::Read { from: 1, to: 2, asked: true }], 2, true, &mut None).is_err());
    assert!(plan(&[Does::Fill { src: 1, len: 1, asked: true, to: false }], 2, true, &mut None).is_err());
    // Reads on the CPU of what it writes: behind it, not ahead of it.
    for read in [Does::Read { from: 900, to: 1001, asked: false }, stuck(1050, 3)] {
        assert!(plan(&[taken, read], 2, true, &mut None).is_err(), "{read:?} behind");
        assert_eq!(planned(&[read, taken]), [(1, 0)], "{read:?} ahead");
    }
    // Also of what is taken along.
    assert!(plan(&[taken, copy(3000, 1000, 10, false), stuck(3005, 1)], 2, true, &mut None).is_err());
    // Into itself, and copies without stage 2.
    assert!(plan(&[copy(1000, 1050, 100, true)], 2, true, &mut None).is_err());
    assert!(plan(&[taken], 1, true, &mut None).is_err());
    assert!(plan(&[fill(1000, 4, true), copy(1000, 5000, 100, false)], 1, true, &mut None).is_err());
}

#[test]
fn a_refresh_of_another_host_copy_is_seen() {
    let does = [Does::Refresh { shadow: 7, from: 100, to: 200 }];
    assert!(!elsewhere(&does, 7, 100, 200));
    assert!(elsewhere(&does, 8, 199, 300));
    assert!(!elsewhere(&does, 8, 200, 300));
}

/// A value written behind a flight, or a read-back, is in no host copy
/// before it lands.
#[test]
fn what_no_host_copy_holds_is_refused() {
    let writers = [Writer { shadow: None, from: 100, to: 108, copy: false }];
    assert!(holder(&writers, 104, 120).is_err());
}

/// Reads are all before writes, between two buffers and within one, and
/// the buffers are usable in the lists that follow in the same call.
#[test]
fn the_card_copies_what_is_recorded() {
    let Some(host) = host() else {
        return; // no host GPU
    };
    let (a, a_data) = host.shared_buffer(1 << 16).unwrap();
    let (b, b_data) = host.shared_buffer(1 << 16).unwrap();
    // SAFETY: mappings of 64 KiB.
    let (a_bytes, b_bytes) =
        unsafe { (std::slice::from_raw_parts_mut(a_data, 1 << 16), std::slice::from_raw_parts_mut(b_data, 1 << 16)) };
    for (i, (x, y)) in a_bytes.iter_mut().zip(b_bytes.iter_mut()).enumerate() {
        (*x, *y) = (i as u8 ^ (i >> 8) as u8, 0xee);
    }
    let was = a_bytes.to_vec();
    let own = host.own_list().unwrap();
    // Within `a` (there and back), from `a` to `b`, and a read alone.
    record(
        true,
        &own.list.list,
        &own.between,
        &[(&a, 33, 0, 100), (&a, 2000, 256, 100), (&a, 5001, 512, 7), (&a, 9, 1024, 4)],
        &[(&a, 2000, 0, 100), (&a, 33, 256, 100), (&b, 77, 512, 7)],
    );
    // A level behind it, of what it wrote.
    record(false, &own.list.list, &own.between, &[(&b, 77, 2048, 7)], &[(&b, 1077, 2048, 7)]);
    own.list.close();
    // A list behind it that uses the buffers in other ways.
    let next = host.new_list().unwrap();
    let (out, out_data) = host.shared_buffer(1 << 16).unwrap();
    // SAFETY: an open list; live buffers.
    unsafe {
        next.list.CopyBufferRegion(&out, 0, &own.between, 1024, 4);
        next.list.CopyBufferRegion(&b, 4096, &a, 33, 100);
    }
    next.close();
    host.execute(&[&own.list, &next]);
    // With the debug layer (`OATHKEEP_D3D12_DEBUG=1`, diagnostics).
    assert_eq!(host.log_messages(), None);
    assert_eq!(a_bytes[2000..2100], was[33..133]);
    assert_eq!(a_bytes[33..133], was[2000..2100]);
    assert_eq!(b_bytes[77..84], was[5001..5008]);
    assert_eq!(b_bytes[1077..1084], was[5001..5008]);
    assert_eq!(b_bytes[4096..4196], was[2000..2100]);
    // SAFETY: a mapping of 64 KiB.
    assert_eq!(unsafe { std::slice::from_raw_parts(out_data, 4) }, &was[9..13]);
    assert!(b_bytes[..77].iter().all(|&x| x == 0xee));
    a_bytes[2000..2100].copy_from_slice(&was[2000..2100]);
    a_bytes[33..133].copy_from_slice(&was[33..133]);
    assert_eq!(a_bytes, &was[..]);
    // As the landing does: its next use records anew.
    POOL.lock().unwrap().push(own);
    let own = host.own_list().unwrap();
    own.list.close();
}

/// The whole way, in a process of its own with the relay on:
/// `OATHKEEP_RELAY=2 OATHKEEP_CARD_STEPS=2 cargo test -p oathkeep-gpu --lib
/// card_steps -- --ignored --test-threads=1`. A flight's list writes
/// part of what the next submission's copy reads, the title wrote the
/// rest; the copy is done on the card behind the flight, and a second
/// one reads what the first wrote. Title memory ends as on the CPU.
#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_CARD_STEPS=2 and a process of its own"]
fn a_copy_behind_a_flight_reads_what_the_flight_wrote() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    const LEN: usize = 4 << 20;
    // Without it (`TEST_UNWATCHED=1`) the title's bytes cannot be put in
    // under a flight's writes: the flights that declared them are waited
    // for.
    let write_watch = if std::env::var_os("TEST_UNWATCHED").is_some() { 0 } else { 0x20_0000 };
    let Some(host) = host() else {
        return;
    };
    assert!(relay_exact() && stage() == 2, "set OATHKEEP_RELAY=2 OATHKEEP_CARD_STEPS=2");
    // Two title buffers: in the first a copy within it, from it into the
    // second, and back.
    let (title, start) = {
        // SAFETY: memory of our own, never freed.
        let base = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | write_watch, PAGE_READWRITE) }.cast::<u8>();
        assert!(!base.is_null());
        // SAFETY: `LEN` bytes just committed.
        (unsafe { std::slice::from_raw_parts_mut(base, LEN) }, base as usize)
    };
    const HALF: usize = LEN / 2;
    crate::shadow::register(0x1111, start as u64, HALF as u64);
    crate::shadow::register(0x2222, (start + HALF) as u64, HALF as u64);
    let first = crate::shadow::find(start as u64, 64).unwrap();
    let (slow_from, slow_to) = (host.uav_buffer(64 << 20).unwrap(), host.uav_buffer(64 << 20).unwrap());
    let before = DONE[1].load(Ordering::Relaxed);
    let mut lists = Vec::new();
    for round in 0..8usize {
        // The title writes; then a "shader" of a flight writes 300 bytes
        // of what the copy reads, behind some work.
        let seed = |i: usize| (i * 7 + round * 13 + i / 251) as u8;
        for (i, b) in title[4000..6000].iter_mut().enumerate() {
            *b = seed(i);
        }
        let (upload, data) = host.upload_buffer(4096).unwrap();
        for i in 0..300 {
            // SAFETY: inside the mapping.
            unsafe { data.add(i).write(0x80 | (round * 31 + i) as u8) };
        }
        let a = host.new_list().unwrap();
        first.refresh(&a, start as u64, HALF as u64);
        first.gpu_writes(&a, start as u64, HALF as u64);
        // SAFETY: an open list; live buffers.
        unsafe {
            for _ in 0..24 {
                a.list.CopyBufferRegion(&slow_to, 0, &slow_from, 0, 64 << 20);
            }
            a.list.CopyBufferRegion(&first.buffer, 4500, &upload, 0, 300);
            a.list
                .ResourceBarrier(&[transition(&first.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
        }
        a.close();
        host.execute_own(&a);
        // With the flight in the air, beside what it writes.
        for (i, b) in title[5300..5400].iter_mut().enumerate() {
            *b = seed(1300 + i) ^ 0x5c;
        }
        // Thousands of runs of bytes of its own.
        for b in title[1_700_000..1_708_000].iter_mut().step_by(2) {
            *b = 1 + round as u8;
        }
        // Into the second buffer, and within the first.
        let b = host.new_list().unwrap();
        b.defer_copy((start + HALF + 100) as *mut u8, (start + 4001) as *const u8, 1500);
        b.defer_copy((start + 900_001) as *mut u8, (start + 4400) as *const u8, 777);
        b.defer_copy((start + HALF + 10_000) as *mut u8, (start + 1_700_000) as *const u8, 8000);
        // What depends on them: a copy of the first copy, upload memory
        // filled from that, and the first one's bytes copied over.
        b.defer_copy((start + 1_500_000) as *mut u8, (start + HALF + 600) as *const u8, 700);
        let (staging, staged) = b.upload(host, 4096).unwrap();
        b.defer_fill_buffer(&staging, staged, (start + 1_500_100) as *const u8, 500);
        b.defer_copy((start + HALF + 300) as *mut u8, (start + 8000) as *const u8, 64);
        b.close();
        host.execute_own(&b);
        // What the first wrote, read by the next submission.
        let c = host.new_list().unwrap();
        c.defer_copy((start + 1_200_000) as *mut u8, (start + HALF + 100) as *const u8, 1500);
        c.close();
        let number = host.execute_own(&c);
        settle_now(WHY_SYNC);
        assert!(FINISHED.load(Ordering::Acquire) >= number);
        let wanted: Vec<u8> = (0..2000)
            .map(|i| match i {
                500..800 => 0x80 | (round * 31 + i - 500) as u8,
                1300..1400 => seed(i) ^ 0x5c,
                _ => seed(i),
            })
            .collect();
        assert!(title[4000..6000] == wanted[..], "what the flight wrote, round {round}");
        // (The copy over the first one's bytes came last.)
        let mut over = wanted[1..1501].to_vec();
        over[200..264].fill(0);
        assert!(title[HALF + 100..HALF + 1600] == over[..], "the copy to the other buffer, round {round}");
        assert!(title[1_500_000..1_500_700] == wanted[501..1201], "the copy behind a copy, round {round}");
        // SAFETY: the mapping, which the flight wrote and has landed.
        let staged = unsafe { std::slice::from_raw_parts(staged, 500) };
        assert!(staged == &wanted[601..1101], "the fill behind it, round {round}");
        assert!(title[900_001..900_778] == wanted[400..1177], "the copy within the buffer, round {round}");
        assert!(title[1_200_000..1_201_500] == over[..], "the copy of the copy, round {round}");
        assert!(title[HALF..HALF + 100].iter().all(|&x| x == 0));
        assert!(title[HALF + 1600..HALF + 10_000].iter().all(|&x| x == 0));
        assert!(title[HALF + 18_000..].iter().all(|&x| x == 0));
        let runs = &title[HALF + 10_000..HALF + 18_000];
        assert!(runs.chunks(2).all(|b| b == [1 + round as u8, 0]), "the runs, round {round}");
        a.keep(&upload);
        lists.extend([a, b, c]);
    }
    assert_eq!(host.log_messages(), None);
    let done = DONE[1].load(Ordering::Relaxed) - before;
    eprintln!("{done} of 48 copies were done on the card, {} submissions waited", DONE[3].load(Ordering::Relaxed));
    eprintln!("settles for title memory under shader writes: {}", LATE_STATS[WHY_STALE].load(Ordering::Relaxed));
    assert!(done >= 20, "only {done} copies were done on the card");
}
