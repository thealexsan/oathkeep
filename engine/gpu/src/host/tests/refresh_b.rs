use super::super::*;
use super::refresh_a::*;
use crate::shadow::Shadow;
use std::sync::Arc;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};


/// Everything at once: the title writes its memory on two threads, the
/// submitting thread puts its bytes in under shader writes
/// (`Shadow::merge_under_writes`), refreshes copies that overlap (two in
/// the card's memory, one not), sends up what changed and declares what
/// the next flight's shaders write, and what flights wrote is landed
/// meanwhile (`crate::land`, beside the guards). When all of it stands
/// still and the copies are refreshed once more, each of them, and the
/// card's side of those that have one, hold what title memory holds, and
/// title memory what was written last.
///
/// Into the first copy shaders write besides through a view nobody
/// declared, words the title does not write: nothing that goes up ever
/// puts an older value over those.
///
/// (It fails in the first rounds if `landed_beside` notes nothing, or
/// only takes the block out of the guards' clean ones and leaves its bit,
/// or if a fault leaves the bit; in some thousand rounds,
/// `OATHKEEP_TEST_ROUNDS`, if what is landed is noted before it is in.)
#[test]
fn landings_title_writes_and_light_refreshes_at_once() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    assert!(crate::safecopy::on());
    const LEN: usize = 24 * PAGE;
    let rounds: usize = std::env::var("OATHKEEP_TEST_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
    let region = title(LEN, true);
    let dword = |at: usize| unsafe { &*((region + at) as *const AtomicU32) };
    let carded = [Carded::new(region, LEN, 3), Carded::new(region + 4 * PAGE + 1000, LEN - 6 * PAGE - 4, 5)];
    let plain = {
        let (start, len) = (region + 700, LEN - 9000);
        let data = memory(len);
        (Shadow::unbacked(start as u64, data, len as u64, false), start, len, data as usize)
    };
    // What was last written so, by offset.
    let mut undeclared = std::collections::HashMap::new();
    let exact = std::env::var_os("OATHKEEP_SEND_CHANGED").is_some_and(|v| v != "0");
    let stop = AtomicBool::new(false);
    // Also when an assertion fails on one of the threads.
    struct Stop<'a>(&'a AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    // Held shared by who writes title memory, alone while it is looked at.
    let still = std::sync::RwLock::new(());
    let (fly, flights) = std::sync::mpsc::channel::<(usize, Vec<(usize, usize)>, u32)>();
    std::thread::scope(|s| {
        for t in 0..2usize {
            let (stop, still) = (&stop, &still);
            s.spawn(move || {
                let mut random = Random(0x7777_1234_0000_0001 + t as u64 * 0x1111);
                let mut value = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    {
                        let _moving = still.read().unwrap();
                        for _ in 0..3 {
                            value += 1;
                            // Of each eight dwords, one a thread.
                            let at = random.below(LEN / 32) * 32 + 4 * (1 + t);
                            dword(at).store(value, Ordering::Relaxed);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_micros(20));
                }
            });
        }
        // Lands flights.
        {
            let (carded, stop) = (&carded, &stop);
            s.spawn(move || {
                let _stop = Stop(stop);
                while let Ok((which, ranges, value)) = flights.recv() {
                    let c = &carded[which];
                    // Fetched: the card's bytes, behind every send so far.
                    let (after, now) = {
                        let card = c.card.lock().unwrap();
                        (card.0, card.2.clone())
                    };
                    // By piece: 64 KiB of title memory.
                    let mut pieces = Vec::new();
                    for &(from, to) in &ranges {
                        let mut at = from;
                        while at < to {
                            let next = ((c.start + at) / PAGE * PAGE + PAGE - c.start).min(to);
                            pieces.push((at, next, now.as_ptr() as usize + at));
                            at = next;
                        }
                    }
                    let parts: Vec<_> = pieces.chunks(3).collect();
                    crate::land::each(parts.len(), &|i| {
                        let mut notes = crate::land::Notes::default();
                        // SAFETY: the bytes of `now`; each piece once.
                        unsafe { c.shadow.land_card(parts[i], after, &mut notes) };
                        notes.flush();
                    });
                    for &(from, to) in &ranges {
                        for at in (from..to).filter(|at| (c.start - region + at) % 32 == 4 * c.class) {
                            // (The next fetch may have brought more.)
                            assert!(dword(c.start - region + at).load(Ordering::Relaxed) >= value, "not landed");
                        }
                    }
                    let done: Vec<_> = ranges.iter().map(|&(a, b)| (c.start + a, c.start + b)).collect();
                    c.shadow.copy_back_done(&done);
                    c.card.lock().unwrap().1 = after;
                    c.flying.store(false, Ordering::Release);
                }
            });
        }
        let _stop = Stop(&stop);
        let mut random = Random(0xABCD_EF01_2345_6789);
        let check = |round: usize, random: &mut Random, quiet: bool| {
            for c in &carded {
                if c.shadow.writes_pending() {
                    let mut runs = usize::MAX;
                    assert!(!c.shadow.merge_under_writes(c.start, c.len, &mut runs), "round {round}");
                }
            }
            let mut ranges = Vec::new();
            for (shadow, start, len) in
                carded.iter().map(|c| (&c.shadow, c.start, c.len)).chain([(&plain.0, plain.1, plain.2)])
            {
                // All of it, or a part; often the same part.
                let (from, n) = match random.below(3) {
                    _ if quiet => (0, len),
                    0 => (0, len),
                    1 => (len / 3 / 4 * 4, len / 2 / 4 * 4),
                    _ => {
                        let from = random.below(len / 4) * 4;
                        (from, (4 + random.below((len - from) / 4) * 4).min(len - from))
                    }
                };
                ranges.push((shadow.clone(), start + from, n));
            }
            submission(&ranges, 1 + random.below(3));
            carded.iter().for_each(Carded::send);
        };
        let mut value = 1u32;
        for round in 0..rounds {
            check(round, &mut random, false);
            if exact {
                let c = &carded[0];
                let mut card = c.card.lock().unwrap();
                for _ in 0..40 {
                    value += 1;
                    let at = random.below(LEN / 32) * 32 + 4 * 6;
                    card.2[at..at + 4].copy_from_slice(&value.to_le_bytes());
                    undeclared.insert(at, value);
                }
            }
            // The flight of this submission: its shaders' writes.
            for (which, c) in carded.iter().enumerate() {
                if c.flying.swap(true, Ordering::Acquire) {
                    continue;
                }
                value += 1;
                let mut ranges: Vec<(usize, usize)> = Vec::new();
                let first = random.below(6) * PAGE / 2;
                for k in 0..2 {
                    let from = first + k * 9 * PAGE + 4 * random.below(9000);
                    let to = (from + 4 * (1 + random.below(7 * PAGE / 4))).min(c.len);
                    if from < to {
                        ranges.push((from, to));
                    }
                }
                for &(from, to) in &ranges {
                    c.shadow.writing(c.start + from, to - from);
                }
                {
                    let mut card = c.card.lock().unwrap();
                    for &(from, to) in &ranges {
                        for at in (from..to).filter(|at| (c.start - region + at) % 32 == 4 * c.class) {
                            card.2[at..at + 4].copy_from_slice(&value.to_le_bytes());
                        }
                    }
                }
                fly.send((which, ranges, value)).unwrap();
            }
            if round % 20 != 19 {
                continue;
            }
            // Still: no flight in the air, no title write.
            let _still = still.write().unwrap();
            while carded.iter().any(|c| c.flying.load(Ordering::Acquire)) {
                assert!(!stop.load(Ordering::Relaxed), "the landing thread failed");
                std::thread::yield_now();
            }
            check(round, &mut random, true);
            // SAFETY: the test's own memory, written by nobody now.
            let bytes = |at: usize, len: usize| unsafe { std::slice::from_raw_parts(at as *const u8, len) };
            let first = |a: &[u8], b: &[u8]| a.iter().zip(b).position(|(x, y)| x != y);
            for c in &carded {
                let (held, there) = (bytes(c.data, c.len), bytes(c.start, c.len));
                assert_eq!(first(held, there), None, "round {round}: a card copy's CPU side (class {})", c.class);
                let card = c.card.lock().unwrap();
                let mut side = card.2.clone();
                if c.class == carded[0].class {
                    // (They may have been landed meanwhile, by a flight
                    // that declared their range, and may not.)
                    for (&at, &value) in &undeclared {
                        let on_card = u32::from_le_bytes(side[at..at + 4].try_into().unwrap());
                        assert_eq!(on_card, value, "round {round}: an older value at {at:#x}");
                        side[at..at + 4].copy_from_slice(&there[at..at + 4]);
                    }
                }
                assert_eq!(first(&side, there), None, "round {round}: a card copy's card side (class {})", c.class);
            }
            assert_eq!(first(bytes(plain.3, plain.2), bytes(plain.1, plain.2)), None, "round {round}: the other copy");
        }
        drop(fly);
    });
}
