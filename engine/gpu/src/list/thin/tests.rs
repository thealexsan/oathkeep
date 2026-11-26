use super::*;
use stream::{COMPUTE_CONSTANTS, Plain, RECORD, VIEWPORTS, Word, at, bytes_of, record};

/// Every method of the tables that records is written down too: what is
/// left in both tables alike records nothing.
#[test]
fn every_recording_method_is_written_down() {
    const NOTHING: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 28, 29, 33, 34, 73, 99, 124];
    let mine: Vec<usize> = MINE.iter().map(|m| m.0).collect();
    for (slot, _) in METHODS.iter().chain(MORE_METHODS.iter()) {
        assert!(mine.contains(slot) != NOTHING.contains(slot), "slot {slot}");
    }
    for &slot in &mine {
        assert!(
            METHODS.iter().chain(MORE_METHODS.iter()).any(|m| m.0 == slot),
            "slot {slot} is written down and not there"
        );
    }
}

/// Arguments come out of a stream as they went in.
#[test]
fn a_stream_gives_back_what_was_written() {
    let mut f = Front::default();
    f.recording = Some(Arc::new(Recording::new(0)));
    f.put(Plain::DRAW_INDEXED as u64, &[3, 1, 0, Word::word(-7i32), 5]);
    let viewport = [1.0f32, 2.0, 3.0, 4.0, 0.0, 1.0];
    f.put_with(VIEWPORTS, &[1], &[bytes_of(viewport.as_ptr(), 6)]);
    f.put_with(COMPUTE_CONSTANTS, &[4 | 2 << 32, 3], &[bytes_of([7u32, 8, 9].as_ptr(), 3)]);
    let s = std::mem::take(&mut f.words);
    assert_eq!((s[0] & 0xffff, s[0] >> 16), (Plain::DRAW_INDEXED as u64, 5));
    assert_eq!(<i32 as Word>::unword(s[4]), -7);
    let w = &s[6..];
    assert_eq!((w[0] & 0xffff, w[0] >> 16, w[1]), (VIEWPORTS, 4, 1));
    // SAFETY: what was written just now.
    assert_eq!(unsafe { *at::<[f32; 6]>(&w[1..], 1, 1) }, viewport);
    let w = &w[5..];
    assert_eq!((w[0] & 0xffff, w[0] >> 16), (COMPUTE_CONSTANTS, 4));
    // SAFETY: as above.
    assert_eq!(unsafe { *at::<[u32; 3]>(&w[1..], 2, 3) }, [7, 8, 9]);
    assert!(at::<u32>(&w[1..], 2, 0).is_null());
}

/// A view's record is the one at the call, also if its slot is written
/// again right after.
#[test]
fn a_target_is_the_one_at_the_call() {
    let mut slot = [0x5256_5476u64, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    let kept = record(slot.as_ptr() as u64);
    slot[1] = 99;
    assert_eq!(kept[1], 2);
    assert_eq!(record(0), [0; RECORD]);
}
