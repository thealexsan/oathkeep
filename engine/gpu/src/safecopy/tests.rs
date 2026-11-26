use super::*;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc,
    VirtualFree, VirtualProtect,
};

struct Pages(*mut u8, usize);

impl Pages {
    fn new(len: usize) -> Pages {
        // SAFETY: plain allocation.
        let at = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!at.is_null());
        Pages(at.cast(), len)
    }
    fn fill(&self) {
        for i in 0..self.1 {
            // SAFETY: inside the allocation.
            unsafe { self.0.add(i).write((i * 31 + 7) as u8) };
        }
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        // SAFETY: our allocation.
        unsafe { VirtualFree(self.0.cast(), 0, MEM_RELEASE) };
    }
}

#[test]
fn copies_every_length_and_alignment() {
    assert!(on());
    let (a, b) = (Pages::new(1 << 16), Pages::new(1 << 16));
    a.fill();
    for len in [0usize, 1, 3, 4, 8, 31, 64, 255, 4095, 4096, 4097, 60_000] {
        for (da, sa) in [(0usize, 0usize), (1, 0), (0, 3), (5, 2)] {
            // SAFETY: inside the two allocations.
            unsafe {
                std::ptr::write_bytes(b.0, 0xEE, b.1);
                assert!(copy(b.0.add(da), a.0.add(sa), len));
                let (to, from) = (std::slice::from_raw_parts(b.0, b.1), std::slice::from_raw_parts(a.0, a.1));
                assert_eq!(to[da..da + len], from[sa..sa + len]);
                assert!(to[..da].iter().chain(&to[da + len..]).all(|&x| x == 0xEE));
            }
        }
    }
}

/// The dwords that changed are stored, and no byte of any other, at
/// every alignment and length; a page that is not there fails.
#[test]
fn only_changed_dwords_are_put() {
    assert!(on());
    let pages = Pages::new(3 << 12);
    for at in [0usize, 1, 2, 3, 4, 13, 4090] {
        for len in [0usize, 1, 3, 4, 5, 31, 32, 33, 64, 100, 1000, 4096] {
            let was: Vec<u8> = (0..len).map(|i| (i * 13 + 1) as u8).collect();
            for step in [1usize, 5, 17, 64, 10_000] {
                let mut now = was.clone();
                for i in (0..len).step_by(step) {
                    now[i] ^= 0x5A;
                }
                // Other threads changed everything meanwhile.
                // SAFETY: inside the allocation.
                unsafe {
                    std::ptr::write_bytes(pages.0, 0xC3, pages.1);
                    assert!(put_changed(pages.0.add(at), &was, &now));
                    let there = std::slice::from_raw_parts(pages.0, pages.1);
                    let base = pages.0.addr();
                    for (k, &byte) in there.iter().enumerate() {
                        let expect = if (at..at + len).contains(&k) {
                            let dword = (base + k) / 4;
                            let changed = (0..len).any(|i| now[i] != was[i] && (base + at + i) / 4 == dword);
                            if changed { now[k - at] } else { 0xC3 }
                        } else {
                            0xC3
                        };
                        if byte != expect {
                            panic!("at {at} len {len} step {step} byte {k}: {byte:#x}, not {expect:#x}");
                        }
                        if k > at + len + 64 {
                            break;
                        }
                    }
                }
            }
        }
    }
    // SAFETY: pages of our allocation.
    unsafe {
        VirtualFree(pages.0.add(4096).cast(), 4096, MEM_DECOMMIT);
        let (was, now) = (vec![0u8; 8192], vec![1u8; 8192]);
        assert!(!put_changed(pages.0, &was, &now));
        // Nothing differs on the page that is not there: nothing fails.
        let mut now = was.clone();
        now[0] = 1;
        assert!(put_changed(pages.0, &was, &now));
    }
}

/// [`merge`] against what it is to do, byte by byte, for every kind of
/// start, end and change: the bytes the card changed arrive in title
/// memory and in what it held, no dword without one is stored.
#[test]
fn a_merge_puts_what_the_card_changed_and_nothing_else() {
    assert!(on());
    let pages = Pages::new(1 << 12);
    let mut seed = 1u32;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for round in 0..4000u32 {
        let at = random() as usize % 100;
        let len = random() as usize % 300;
        let (from, to) = (at, (at + len).min(400));
        let n = to - from;
        let base: Vec<u8> = (0..512).map(|_| random() as u8).collect();
        let was: Vec<u8> = base[from..to].to_vec();
        let mut held = was.clone();
        let mut now = was.clone();
        let mut title = base.clone();
        // The card's writes: dwords and single bytes; the title's: some
        // in the same dwords, some the same values.
        for _ in 0..random() % 12 {
            if n > 0 {
                let i = random() as usize % n;
                now[i] = random() as u8;
            }
            let i = random() as usize % 512;
            title[i] = if (from..to).contains(&i) && round % 3 == 0 { now[i - from] } else { random() as u8 };
        }
        if round % 7 == 0 {
            now.iter_mut().for_each(|b| *b = b.wrapping_add(1));
        }
        // SAFETY: inside the allocation and the vectors.
        let done = unsafe {
            std::ptr::copy_nonoverlapping(title.as_ptr(), pages.0, 512);
            merge(pages.0.add(from), now.as_ptr(), held.as_mut_ptr(), n, round % 2 == 0)
        };
        assert!(done.there);
        // SAFETY: inside the allocation.
        let there = unsafe { std::slice::from_raw_parts(pages.0, 512) };
        for i in 0..512 {
            let wrote = (from..to).contains(&i) && now[i - from] != was[i - from];
            let expect = if wrote { now[i - from] } else { title[i] };
            assert_eq!(there[i], expect, "round {round} byte {i} of {from}..{to}");
        }
        assert_eq!(held, now, "round {round}");
        assert_eq!(done.new == 0, was == now);
        // Found merged already.
        // SAFETY: as above.
        let again = unsafe { merge(pages.0.add(from), now.as_ptr(), held.as_mut_ptr(), n, false) };
        assert_eq!(again, Merged { new: 0, dwords: 0, there: true });
    }
}

/// Title memory that is not there: what the card wrote stays to be
/// looked at again, or is taken, as asked; a page that only cannot be
/// written counts the same.
#[test]
fn a_merge_into_missing_memory_fails() {
    assert!(on());
    let pages = Pages::new(1 << 13);
    let mut old = 0;
    for (missing, take) in [(true, false), (true, true), (false, false), (false, true)] {
        for (at, len) in [(0usize, 4096usize), (4, 64), (7, 1), (40, 100)] {
            let was = vec![1u8; len];
            let mut now = was.clone();
            now[len - 1] = 9;
            let mut held = was.clone();
            // SAFETY: our allocation.
            let done = unsafe {
                if missing {
                    VirtualFree(pages.0.cast(), 4096, MEM_DECOMMIT);
                } else {
                    VirtualProtect(pages.0.cast(), 4096, PAGE_READONLY, &mut old);
                }
                let done = merge(pages.0.add(at), now.as_ptr(), held.as_mut_ptr(), len, take);
                VirtualAlloc(pages.0.cast(), 4096, MEM_COMMIT, PAGE_READWRITE);
                VirtualProtect(pages.0.cast(), 4096, PAGE_READWRITE, &mut old);
                done
            };
            assert!(!done.there);
            assert_eq!(held, if take { now.clone() } else { was.clone() }, "{missing} {take} {at} {len}");
            // Nothing to put: nothing fails.
            let mut held = now.clone();
            // SAFETY: as above; the addresses are no one's.
            let done = unsafe { merge(std::ptr::without_provenance_mut(0x1000 + at), now.as_ptr(), held.as_mut_ptr(), len, take) };
            assert!(done.there && done.new == 0);
        }
    }
}

/// [`merge`] beside threads that write other dwords of the same memory:
/// none of their writes is lost or undone.
#[test]
fn merges_undo_no_write_of_other_threads() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    assert!(on());
    let pages = Pages::new(1 << 12);
    let base = pages.0 as usize;
    // SAFETY: committed, aligned memory, only touched as atomics or by
    // the merge's stores of whole dwords.
    let dword = |i: usize| unsafe { &*((base + i * 4) as *const AtomicU32) };
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let writers: Vec<_> = (0..2usize)
            .map(|w| {
                let stop = &stop;
                s.spawn(move || {
                    let mut value = 1u32;
                    while !stop.load(Ordering::Relaxed) {
                        for i in (w..1024).step_by(4) {
                            // This thread's own dword: nobody else's value.
                            assert_eq!(dword(i).load(Ordering::Relaxed), value - 1, "dword {i}");
                            dword(i).store(value, Ordering::Relaxed);
                        }
                        value += 1;
                    }
                })
            })
            .collect();
        // The card's dwords: every fourth, from the third on.
        let mut held = vec![0u8; 4096];
        for round in 1..3000u32 {
            let mut now = held.clone();
            for i in (2..1024).step_by(4) {
                if i * 7 % 5 != round as usize % 5 {
                    now[i * 4..i * 4 + 4].copy_from_slice(&(round + i as u32).to_le_bytes());
                }
            }
            // SAFETY: the allocation, whole.
            let done = unsafe { merge(base as *mut u8, now.as_ptr(), held.as_mut_ptr(), 4096, false) };
            assert!(done.there);
            for i in (2..1024).step_by(4) {
                let expect = u32::from_le_bytes(now[i * 4..i * 4 + 4].try_into().unwrap());
                assert_eq!(dword(i).load(Ordering::Relaxed), expect);
            }
            for i in (3..1024).step_by(4) {
                assert_eq!(dword(i).load(Ordering::Relaxed), 0);
            }
        }
        stop.store(true, Ordering::Relaxed);
        for w in writers {
            w.join().expect("no write was undone");
        }
    });
}

#[test]
fn differences_are_seen_wherever_they_are() {
    for len in [0usize, 1, 31, 32, 33, 127, 128, 129, 200] {
        let a = vec![7u8; len];
        // SAFETY: the vectors' bytes.
        assert!(!unsafe { differ(a.as_ptr(), a.clone().as_ptr(), len) });
        for at in 0..len {
            let mut b = a.clone();
            b[at] = 8;
            // SAFETY: as above.
            assert!(unsafe { differ(a.as_ptr(), b.as_ptr(), len) }, "{len} {at}");
        }
    }
}

/// What the copies cost (ignored: a measurement): 64 KiB pieces of
/// scattered memory into a buffer the card reads, by the kernel and here.
#[test]
#[ignore]
fn copy_costs() {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    assert!(on());
    let Some(host) = crate::host::host() else { return };
    const N: usize = 128 << 20;
    let from = Pages::new(N);
    from.fill();
    let (_buffer, shared) = host.shared_buffer(N as u64).expect("a shared buffer");
    let plain = Pages::new(N);
    plain.fill();
    for (what, to) in [("a buffer the card reads", shared), ("plain memory", plain.0)] {
        for piece in [256usize, 4 << 10, 64 << 10] {
            let count = (N / piece).min(4096);
            // A different, scattered piece each time.
            let at = |i: usize| (i * 7919 % (N / piece)) * piece;
            let time = |f: &dyn Fn(usize) -> bool| {
                let mut best = f64::MAX;
                for _ in 0..5 {
                    let t = std::time::Instant::now();
                    for i in 0..count {
                        assert!(f(at(i)));
                    }
                    best = best.min(t.elapsed().as_secs_f64());
                }
                (best * 1e6 / count as f64, count as f64 * piece as f64 / best / 1e9)
            };
            // SAFETY: inside the buffers.
            let kernel = time(&|o| unsafe {
                let mut done = 0;
                ReadProcessMemory(GetCurrentProcess(), from.0.add(o).cast(), to.add(o).cast(), piece, &mut done) != 0
            });
            // SAFETY: as above.
            let here = time(&|o| unsafe { copy(to.add(o), from.0.add(o), piece) });
            // SAFETY: as above.
            let std_copy = time(&|o| unsafe {
                std::ptr::copy_nonoverlapping(from.0.add(o), to.add(o), piece);
                true
            });
            println!(
                "{piece:>6} bytes into {what}: kernel {:.2} us ({:.1} GB/s), rep movsb {:.2} us ({:.1} GB/s), memcpy {:.2} us ({:.1} GB/s)",
                kernel.0, kernel.1, here.0, here.1, std_copy.0, std_copy.1
            );
        }
    }
}

/// Missing, forbidden and read-only pages: the copy fails, at any byte,
/// and the process goes on.
#[test]
fn faults_are_failures() {
    assert!(on());
    let (a, b) = (Pages::new(3 << 12), Pages::new(3 << 12));
    a.fill();
    let mut old = 0;
    // SAFETY: pages of our allocations.
    unsafe {
        assert!(copy(b.0, a.0, b.1));
        VirtualProtect(a.0.add(4096).cast(), 4096, PAGE_NOACCESS, &mut old);
        assert!(!copy(b.0, a.0, b.1));
        assert!(copy(b.0, a.0, 4096));
        assert!(!copy(b.0, a.0.add(8191), 1));
        assert!(copy(b.0, a.0.add(8192), 4096));
        VirtualProtect(a.0.add(4096).cast(), 4096, PAGE_READWRITE, &mut old);
        VirtualFree(a.0.add(8192).cast(), 4096, MEM_DECOMMIT);
        assert!(!copy(b.0, a.0, b.1));
        VirtualProtect(b.0.add(4096).cast(), 4096, PAGE_READONLY, &mut old);
        assert!(!copy(b.0, a.0, 8192));
        assert!(!copy(std::ptr::without_provenance_mut(0x10), a.0, 4));
        assert!(!copy(b.0, std::ptr::without_provenance(usize::MAX - 7), 4));
        VirtualProtect(b.0.add(4096).cast(), 4096, PAGE_READWRITE, &mut old);
    }
    // Many faults, on several threads at once.
    let at = a.0 as usize;
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(move || {
                let mut to = [0u8; 64];
                for _ in 0..500 {
                    // SAFETY: a decommitted page and our own buffer.
                    assert!(!unsafe { copy(to.as_mut_ptr(), (at + 8192) as *const u8, 64) });
                }
            });
        }
    });
}
