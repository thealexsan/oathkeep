use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::buffer_views::{buffer_view_ok, raw_window, twin_window};
use super::diag::{off_elements, off_elements_for, view_kind, view_stride};
use super::flips::{
    Flip, NEXT_PENDING_FLIP, PENDING_LIST_COUNT, PENDING_LIST_FLIPS, PendingListFlip, SHORT,
    SHORT_IN_A_ROW, neutral_for, remove_pending_list_flip, take_pending_list_flip,
};
use super::kin::{Kin, changed_records, kin, same_placed_texture};
use super::make::{constant_bytes, from_base, make};
use super::null_views::{null_for, null_uav_desc};
use super::queue::{TABLED_FRAMES, TABLED_GRACE, TextureQueue};
use super::sets::Slots;
use super::spans::{Rule, Spans, join};
use super::staged::{srv_desc, staged};
use super::*;

mod queue;
mod spans;
mod twins;
mod views;

#[test]
fn changed_records_are_those_that_differ_one_by_one() {
    for (step, count) in [(32usize, 128usize), (32, 2048), (16, 77), (32, 5), (32, 17)] {
        let mut seed = 7u64;
        let mut random = move || {
            seed = seed.wrapping_mul(0x5851_F42D_4C95_7F2D).wrapping_add(0x1405_7B7E_F767_814F);
            (seed >> 33) as usize
        };
        let mut last: Vec<u8> = (0..step * count).map(|_| random() as u8).collect();
        let mut now = last.clone();
        let mut differ = Vec::new();
        for slot in 0..count {
            if random() % 9 == 0 {
                now[slot * step + random() % step] ^= 1 + (random() % 255) as u8;
                differ.push(1000 + slot as u32);
            }
        }
        let mut out = Vec::new();
        changed_records(&mut last, &now, step, 1000, &mut out);
        assert_eq!(out, differ);
        assert_eq!(last, now);
        out.clear();
        changed_records(&mut last, &now, step, 1000, &mut out);
        assert!(out.is_empty());
    }
}

#[test]
fn pending_list_flips_promote_once_and_clean_up() {
    let heap = 0xdead_beefusize;
    let id1 = NEXT_PENDING_FLIP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id2 = NEXT_PENDING_FLIP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut pending = PENDING_LIST_FLIPS.lock().unwrap_or_else(|e| e.into_inner());
        pending.push(PendingListFlip {
            id: id1,
            origin: (1, 1),
            flip: Flip { after: 0, heap, index: 7, handle: 0, key: 11, late: Some(1) },
        });
        pending.push(PendingListFlip {
            id: id2,
            origin: (2, 1),
            flip: Flip { after: 0, heap, index: 8, handle: 0, key: 12, late: Some(1) },
        });
        PENDING_LIST_COUNT.store(pending.len(), std::sync::atomic::Ordering::Release);
    }
    let taken = take_pending_list_flip(id1).expect("id1 should be present");
    assert_eq!(taken.index, 7);
    assert!(take_pending_list_flip(id1).is_none(), "second take must be a no-op");
    remove_pending_list_flip(id2);
    assert!(take_pending_list_flip(id2).is_none(), "removed entry must be gone");
}

#[test]
fn deferred_flip_closure_keeps_pending_entry_alive_until_called_or_dropped() {
    let heap = 0xcafe_babeusize;
    let id1 = NEXT_PENDING_FLIP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id2 = NEXT_PENDING_FLIP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut pending = PENDING_LIST_FLIPS.lock().unwrap_or_else(|e| e.into_inner());
        for (id, idx) in [(id1, 10), (id2, 11)] {
            pending.push(PendingListFlip {
                id,
                origin: (3, 1),
                flip: Flip { after: 0, heap, index: idx, handle: 0, key: 20, late: Some(1) },
            });
        }
        PENDING_LIST_COUNT.store(pending.len(), std::sync::atomic::Ordering::Release);
    }
    struct TestGuard(u64);
    impl TestGuard {
        fn id(&self) -> u64 {
            self.0
        }
    }
    impl Drop for TestGuard {
        fn drop(&mut self) {
            remove_pending_list_flip(self.0);
        }
    }
    let make_cb = |id: u64| -> Box<dyn Fn() -> Option<u32> + Send + 'static> {
        let guard = TestGuard(id);
        Box::new(move || take_pending_list_flip(guard.id()).map(|f| f.index))
    };
    let cb1 = make_cb(id1);
    let cb2 = make_cb(id2);
    assert_eq!(cb1(), Some(10));
    assert_eq!(cb1(), None);
    drop(cb2);
    assert!(take_pending_list_flip(id2).is_none(), "dropping uncalled closure cleans up entry");
}
