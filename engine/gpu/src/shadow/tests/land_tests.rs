use super::{Card, PIECE, Shadow};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// A copy in the card's memory of title memory at `title` (its card side
/// is never looked at here), all bytes 0 on the CPU's side.
fn copy_of(title: usize, len: usize) -> Option<Shadow> {
    let (buffer, data) = crate::host::host()?.shared_buffer(len as u64)?;
    // SAFETY: the mapping, `len` bytes.
    unsafe { std::ptr::write_bytes(data, 0, len) };
    let mut shadow = Shadow::new(buffer.clone(), data, 0, title as u64, len as u64);
    shadow.card = Some(Card {
        staging: buffer,
        before: Mutex::default(),
        before_gpu: std::sync::atomic::AtomicU64::new(0),
        dirty: Mutex::default(),
        sent: Mutex::default(),
            touched: Default::default(),
        size: len as u64,
    });
    Some(shadow)
}

/// Memory that stays (`crate::watch` knows its addresses): private, or a
/// view noted as the runtime notes the title's.
fn memory(len: usize, view: bool) -> usize {
    use windows_sys::Win32::System::Memory as mem;
    // SAFETY: plain allocations of our own.
    unsafe {
        if !view {
            return mem::VirtualAlloc(std::ptr::null(), len, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE) as usize;
        }
        let section = mem::CreateFileMappingW(-1isize as _, std::ptr::null(), mem::PAGE_READWRITE, 0, len as u32, std::ptr::null());
        let at = mem::MapViewOfFile(section, mem::FILE_MAP_WRITE, 0, 0, len).Value as usize;
        crate::inplace::title_memory_section(section as usize, len as u64, true);
        crate::title_memory_remapped(at, len);
        crate::inplace::title_memory_view(at, len, section as usize, 0);
        at
    }
}

fn random(seed: &mut u64) -> usize {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    (*seed >> 8) as usize
}

/// [`Shadow::land_card`] leaves title memory and the CPU's side as
/// [`Shadow::card_merge_relay`] does, with title bytes that went up
/// behind the fetch, or have not gone up, left out.
#[test]
fn pieces_land_as_the_merge_one_by_one_lands_them() {
    assert!(crate::safecopy::on());
    const LEN: usize = 3 * PIECE;
    let mut seed = 0x1234_5678_9abcu64;
    for view in [false, true] {
        let (a, b) = (memory(LEN, view), memory(LEN, view));
        let (Some(one), Some(many)) = (copy_of(a, LEN), copy_of(b, LEN)) else {
            return; // no host GPU
        };
        // SAFETY: the two memories and the two mappings.
        let bytes = |at: usize| unsafe { std::slice::from_raw_parts_mut(at as *mut u8, LEN) };
        for round in 0..40u32 {
            let base: Vec<u8> = (0..LEN).map(|_| random(&mut seed) as u8).collect();
            let (mut held, mut title, mut now) = (base.clone(), base.clone(), base);
            let (mut dirty, mut sent) = (Vec::new(), Vec::new());
            for k in 0..120 {
                let at = random(&mut seed) % LEN / 4 * 4;
                let n = (random(&mut seed) % 900 / 4 * 4).min(LEN - at);
                match k % 4 {
                    0 | 1 => now[at..at + n].fill(random(&mut seed) as u8),
                    2 => title[at..at + n].fill(random(&mut seed) as u8),
                    // Title bytes put into the CPU's side meanwhile.
                    _ => {
                        let byte = random(&mut seed) as u8;
                        held[at..at + n].fill(byte);
                        title[at..at + n].fill(byte);
                        if k % 8 == 3 {
                            dirty.push((at, at + n));
                        } else {
                            sent.push((at, at + n, u64::from(round % 3) + (k as u64 / 16)));
                        }
                    }
                }
            }
            let after = 4u64;
            for (shadow, at) in [(&one, a), (&many, b)] {
                crate::watch::writing(at, LEN);
                bytes(at).copy_from_slice(&title);
                bytes(shadow.data as usize).copy_from_slice(&held);
                let card = shadow.card.as_ref().expect("made with one");
                *card.dirty.lock().unwrap() = dirty.clone();
                *card.sent.lock().unwrap() = super::SentRuns::of(&sent);
            }
            let pieces: Vec<(usize, usize)> = (0..3).map(|p| (p * PIECE + 16, (p + 1) * PIECE - 4)).collect();
            let mut scratch = Vec::new();
            for &(lo, hi) in &pieces {
                one.card_merge_relay(lo, &now[lo..hi], after, &mut scratch);
            }
            let addressed: Vec<(usize, usize, usize)> =
                pieces.iter().map(|&(lo, hi)| (lo, hi, now.as_ptr() as usize + lo)).collect();
            let mut notes = crate::land::Notes::default();
            // SAFETY: the fetched bytes are `now`'s.
            unsafe { many.land_card(&addressed, after, &mut notes) };
            notes.flush();
            assert!(bytes(a) == bytes(b), "title memory, round {round}");
            assert!(bytes(one.data as usize) == bytes(many.data as usize), "the CPU's side, round {round}");
        }
    }
}

/// Landings of one copy on several threads, the title writing dwords of
/// the same blocks, and the title's bytes put into the CPU's side under
/// the shaders' writes ([`Shadow::merge_under_writes`]), all at once. The
/// card has never received the title's bytes: what is fetched holds older
/// ones there, which must not be taken for shaders' writes.
#[test]
fn landings_and_title_bytes_going_in_keep_each_others_bytes() {
    assert!(crate::safecopy::on());
    const BLOCKS: usize = 16;
    const LEN: usize = BLOCKS * PIECE;
    let title = memory(LEN, true);
    let Some(shadow) = copy_of(title, LEN) else {
        return; // no host GPU
    };
    shadow.pending.lock().unwrap().push((title, title + LEN));
    shadow.has_pending.store(true, Ordering::Release);
    // SAFETY: committed, aligned; touched as atomics or by the landing's
    // stores of whole dwords.
    let dword = |i: usize| unsafe { &*((title + i * 4) as *const AtomicU32) };
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let writer = s.spawn(|| {
            let mut value = 1u32;
            while !stop.load(Ordering::Relaxed) {
                for i in (1..LEN / 4).step_by(4 * 331) {
                    assert_eq!(dword(i).swap(value, Ordering::Relaxed), value - 1, "dword {i}: undone");
                }
                value += 1;
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
            value - 1
        });
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let mut runs = usize::MAX;
                assert!(!shadow.merge_under_writes(title, LEN, &mut runs));
                // Sent up, as the preparing of a submission would: noted
                // with a number behind the fetches'.
                let card = shadow.card.as_ref().expect("made with one");
                let mut dirty = card.dirty.lock().unwrap();
                let mut sent = card.sent.lock().unwrap();
                sent.note(9, std::mem::take(&mut *dirty));
            }
        });
        let mut now = vec![0u8; LEN];
        for round in 1..200u32 {
            for block in (0..BLOCKS).filter(|b| (*b as u32 * 5 + round) % 3 != 0) {
                for i in (block * PIECE / 4 + 3..(block + 1) * PIECE / 4).step_by(4) {
                    now[i * 4..i * 4 + 4].copy_from_slice(&(round << 20 | i as u32).to_le_bytes());
                }
            }
            let at = now.as_ptr() as usize;
            crate::land::each(BLOCKS, &|b| {
                let mut notes = crate::land::Notes::default();
                let (lo, hi) = (b * PIECE, (b + 1) * PIECE);
                // SAFETY: the fetched bytes are `now`'s; each block once.
                unsafe { shadow.land_card(&[(lo, hi, at + lo)], 8, &mut notes) };
                notes.flush();
            });
            for i in (3..LEN / 4).step_by(4 * 53) {
                let expect = u32::from_le_bytes(now[i * 4..i * 4 + 4].try_into().expect("four"));
                assert_eq!(dword(i).load(Ordering::Relaxed), expect, "round {round} dword {i}");
            }
        }
        stop.store(true, Ordering::Relaxed);
        let last = writer.join().expect("nothing undone");
        assert!((1..LEN / 4).step_by(4 * 331).all(|i| dword(i).load(Ordering::Relaxed) == last));
    });
}
