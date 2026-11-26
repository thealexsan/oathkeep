use std::collections::BTreeSet;

use super::super::bits::{Blocks, Landed};
use super::super::fault::writable;
use super::super::guards::{REST_IN_TEST, published, put_count};
use super::super::protect::guard;
use super::super::*;
use super::{drop_views, two_views};

/// The title wrote the pages of a guarded texture through another view
/// of them: the fault at that view is the guard's to see, and the
/// guarded view's next check reports the pages written.
#[test]
fn a_write_through_another_view_is_reported_at_the_guarded_one() {
    const SIZE: usize = 2 * BLOCK;
    let (section, guarded, other) = two_views(SIZE);
    let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
    // The first check takes it in (and every view of its pages).
    assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
    assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
    // Writing through the other view faults into the handler, which
    // opens that view and the guarded one: the next check sees it.
    // SAFETY: the second view, read-write once the handler has opened it.
    unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
    assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
    assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
    drop_views(section, &[guarded, other], SIZE);
}

/// A view of guarded pages is mapped anew: it is writable, so the
/// guarded view checks its pages again (and guards the new view).
#[test]
fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
    const SIZE: usize = 2 * BLOCK;
    let (section, guarded, other) = two_views(SIZE);
    let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
    assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
    assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
    title_memory_remapped(other, SIZE);
    crate::inplace::title_memory_view(other, SIZE, section, 0);
    assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
    assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
    drop_views(section, &[guarded, other], SIZE);
}

/// A write of the runtime's own, told at one view of the pages
/// (`writing`): the other views' next checks report the pages written.
#[test]
fn a_write_told_at_one_view_is_reported_at_the_others() {
    const SIZE: usize = 2 * BLOCK;
    let (section, guarded, other) = two_views(SIZE);
    let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
    assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
    assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
    writing(other + BLOCK, BLOCK);
    assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
    assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
    // Half a page and the next: both blocks, not just the first's.
    writing(other + BLOCK / 2, BLOCK);
    assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
    drop_views(section, &[guarded, other], SIZE);
}

/// Both views of the pages are checked, each for a copy of its own
/// (the title shows one physical page at several addresses, and each
/// address has its host copy): a write through either is reported at
/// both, whichever is checked first. Guarding the pages for the first
/// view's check must not note the second view clean, or its copy would
/// keep the bytes from before the write.
#[test]
fn a_write_is_reported_at_every_checked_view() {
    const SIZE: usize = 2 * BLOCK;
    let (section, one, two) = two_views(SIZE);
    let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
    assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0), pages(one, 1)]));
    assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0), pages(two, 1)]), "the second view's first check");
    assert_eq!(guard(one, one + SIZE), Some(Vec::new()), "nothing written yet");
    assert_eq!(guard(two, two + SIZE), Some(Vec::new()), "nothing written yet");
    // SAFETY: the second view, read-write once the handler has opened it.
    unsafe { ((two + BLOCK + 8) as *mut u8).write(1) };
    assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 1)]));
    assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 1)]), "written through this view, checked second");
    // SAFETY: the first view, as above.
    unsafe { ((one + 8) as *mut u8).write(1) };
    assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0)]));
    assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0)]), "written through this view, checked second");
    assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
    assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
    drop_views(section, &[one, two], SIZE);
}

/// A write opens the view it goes through and no other: the others stay
/// read-only, are told as written, and take writes of their own.
#[test]
fn a_write_opens_the_view_it_goes_through() {
    let (section, one, two) = two_views(BLOCK);
    let page = |view: usize| view / PAGE;
    for view in [one, two] {
        assert_eq!(guard(view, view + BLOCK), Some(vec![page(view)]));
    }
    // SAFETY (all): a view, opened by the handler.
    unsafe { ((two + 8) as *mut u8).write(1) };
    assert!(writable(two) && !writable(one));
    unsafe { ((one + 8) as *mut u8).write(1) };
    assert!(writable(two) && writable(one));
    assert_eq!(guard(one, one + BLOCK), Some(vec![page(one)]));
    assert_eq!(guard(two, two + BLOCK), Some(vec![page(two)]));
    // Written at the view that was left as it was, after the other.
    unsafe { ((two + 8) as *mut u8).write(1) };
    assert_eq!(guard(one, one + BLOCK), Some(vec![page(one)]), "not clean, though never opened");
    assert!(!writable(two) && !writable(one));
    assert_eq!(guard(two, two + BLOCK), Some(vec![page(two)]));
    assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
    // Of a page that was told at one view, a kernel write to the other
    // finds it open.
    unsafe { ((two + 8) as *mut u8).write(1) };
    assert!(!writable(one));
    writing(one, 8);
    assert!(writable(one));
    drop_views(section, &[one, two], BLOCK);
}

/// A block the title has just written is not made read-only, and is told
/// as written at each asking, at every view of it, until it has rested.
#[test]
fn a_block_just_written_rests() {
    let (section, one, two) = two_views(BLOCK);
    let page = one / PAGE;
    let rests = |rests: bool| REST_IN_TEST.set(std::time::Duration::from_secs(if rests { 3600 } else { 0 }));
    assert_eq!(guard(one, one + BLOCK), Some(vec![page]));
    assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
    rests(true);
    assert_eq!(guard(one, one + BLOCK), Some(Vec::new()), "nothing written: as it was");
    // SAFETY (all): a view, opened by the handler.
    unsafe { ((two + 8) as *mut u8).write(1) };
    for _ in 0..3 {
        assert_eq!(guard(one, one + BLOCK), Some(vec![page]));
        assert!(writable(two), "no fault for the next write");
        assert!(writes(two, two + BLOCK, true).is_some() && !published(two, two + BLOCK) && !published(one, one + BLOCK));
        unsafe { ((two + 8) as *mut u8).write(1) };
    }
    rests(false);
    assert_eq!(guard(one, one + BLOCK), Some(vec![page]));
    assert!(!writable(two) && !writable(one));
    assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
    drop_views(section, &[one, two], BLOCK);
}

/// What is put in beside a guard is told at every view, once, and the
/// block stays read-only; until it has had its share.
#[test]
fn what_the_title_puts_in_beside_a_guard_is_told() {
    let (section, one, two) = two_views(BLOCK);
    crate::title_memory_section(section, BLOCK as u64, true);
    let page = |view: usize| view / PAGE;
    for view in [one, two] {
        assert_eq!(guard(view, view + BLOCK), Some(vec![page(view)]));
    }
    assert!(writes(one, one + BLOCK, true).is_some() && writes(two, two + BLOCK, true).is_some());
    put_count(one / BLOCK).store(0, std::sync::atomic::Ordering::Relaxed);
    let put = |byte: u8| loop {
        // SAFETY: a byte of the section's only page.
        let done = put_beside(one / BLOCK, 1, |delta| unsafe { ((one + 100).wrapping_add(delta) as *mut u8).write(byte) });
        // (Other tests hold changes of mapping.)
        if done != Beside::Cannot {
            return done;
        }
    };
    for round in 1..4 {
        assert_eq!(put(round), Beside::Done);
        // SAFETY: the view the title reads it by.
        assert_eq!(unsafe { ((two + 100) as *const u8).read_volatile() }, round);
        assert!(!writable(one) && !writable(two) && !published(one, one + BLOCK) && !published(two, two + BLOCK));
        for view in [two, one] {
            assert_eq!(guard(view, view + BLOCK), Some(vec![page(view)]));
            assert_eq!(guard(view, view + BLOCK), Some(Vec::new()));
        }
    }
    for _ in 0..crate::titlewrite::MOST {
        assert_eq!(put(9), Beside::Done);
    }
    assert_eq!(put(9), Beside::Full);
    // A refresh between gives it its share again.
    assert_eq!(guard(one, one + BLOCK), Some(vec![page(one)]));
    assert_eq!(put(9), Beside::Done);
    // Opened by a fault, it is told by that.
    assert_eq!(guard(one, one + BLOCK), Some(vec![page(one)]));
    // SAFETY: a view, opened by the handler.
    unsafe { ((one + 8) as *mut u8).write(1) };
    assert_eq!(put_beside(one / BLOCK, 1, |_| panic!("it is not guarded")), Beside::Cannot);
    assert_eq!(guard(one, one + BLOCK), Some(vec![page(one)]));
    assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
    drop_views(section, &[one, two], BLOCK);
    crate::title_memory_section_gone(section);
}

/// Stores that fault, and copies, as the title makes them: what is there
/// after, the block's guard and what the next asking says.
#[test]
fn stores_and_copies_of_the_title_leave_the_guard() {
    use std::sync::atomic::{AtomicU32, Ordering};
    unsafe extern "system-unwind" fn real(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
        // SAFETY: by the caller.
        unsafe { std::ptr::copy(from, to, n) };
        to
    }
    const SIZE: usize = 2 * BLOCK;
    let (section, one, two) = two_views(SIZE);
    crate::title_memory_section(section, SIZE as u64, true);
    let pages = |view: usize| vec![view / PAGE, view / PAGE + 1];
    assert_eq!(guard(one, one + SIZE), Some(pages(one)));
    assert_eq!(guard(two, two + SIZE), Some(pages(two)));
    let copy = crate::titlewrite::import("memcpy", real as *const () as usize);
    crate::titlewrite::ON_IN_TEST.set(true);
    let copy = if copy == real as *const () as usize {
        crate::titlewrite::copy_for_test(real as *const () as usize)
    } else {
        copy
    };
    // SAFETY: of that signature.
    let copy: unsafe extern "system-unwind" fn(*mut u8, *const u8, usize) -> *mut u8 = unsafe { std::mem::transmute(copy) };
    // No change of mapping meanwhile but the other tests' moments of it:
    // a store that meets one opens the block, as before.
    let mut kept = 0;
    let quiet = || {
        while title_memory_reading_now().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    for round in 1..200u32 {
        quiet();
        // SAFETY (all): words of the view.
        unsafe { ((one + 64) as *mut u32).write_volatile(round) };
        let was = unsafe { AtomicU32::from_ptr((two + 64) as *mut u32) }.fetch_sub(1, Ordering::SeqCst);
        assert_eq!((was, unsafe { ((one + 64) as *const u32).read_volatile() }), (round, round - 1));
        let bytes = vec![round as u8; 3000 + round as usize];
        // Over the end of the first block.
        assert_eq!(unsafe { copy((one + BLOCK - 1000) as *mut u8, bytes.as_ptr(), bytes.len()) } as usize, one + BLOCK - 1000);
        let there = unsafe { std::slice::from_raw_parts((two + BLOCK - 1000) as *const u8, bytes.len()) };
        assert!(there == bytes && unsafe { ((two + BLOCK + 2000 + round as usize) as *const u8).read_volatile() } != round as u8);
        kept += usize::from(!writable(one) && !writable(two));
        for view in [one, two] {
            assert_eq!(guard(view, view + SIZE), Some(pages(view)), "round {round}");
            assert_eq!(guard(view, view + SIZE), Some(Vec::new()), "round {round}");
        }
    }
    assert!(kept > 10, "the guard stayed {kept} times: {:?}", crate::titlewrite::DONE);
    // What is there already is not written: nothing to tell.
    for k in 0..10 {
        // SAFETY (all): as above.
        unsafe { ((one + 64) as *mut u32).write_volatile(198) };
        let same = unsafe { std::slice::from_raw_parts((one + BLOCK - 900) as *const u8, 1500) }.to_vec();
        unsafe { copy((one + BLOCK - 900) as *mut u8, same.as_ptr(), same.len()) };
        assert!(!writable(one) && !writable(two), "{k}");
    }
    assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
    // Many stores to a block: it is opened.
    quiet();
    for k in 0..20 {
        unsafe { ((one + 128 + k * 4) as *mut u32).write_volatile(7) };
    }
    assert!(writable(one));
    crate::titlewrite::ON_IN_TEST.set(false);
    drop_views(section, &[one, two], SIZE);
    crate::title_memory_section_gone(section);
}

/// Of a page taken before, what is taken again is the kernel's pages
/// written since; where they are not known, all of it.
#[test]
fn what_is_taken_again_is_what_was_written() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    // SAFETY: a fresh write-watched allocation, freed at the end.
    let p = unsafe {
        VirtualAlloc(std::ptr::null(), 4 * PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
    } as usize;
    assert_ne!(p, 0);
    let write = |at: usize| {
        // SAFETY: committed pages of ours.
        unsafe { ((p + at) as *mut u8).write_volatile(1) }
    };
    let tracked = track(p, 4 * PAGE, false).expect("watched");
    // (Other tests' memory was here.)
    (0..4).for_each(|page| write(page * PAGE + 5));
    assert_eq!(tracked.take(p, p + 4 * PAGE), [(p, p + 4 * PAGE)], "never taken");
    assert!(tracked.take(p, p + 4 * PAGE).is_empty());
    write(PAGE + 5000);
    write(PAGE + 8192);
    write(PAGE + 65535);
    write(2 * PAGE);
    assert_eq!(
        tracked.take(p, p + 4 * PAGE),
        [(p + PAGE + 4096, p + PAGE + 3 * 4096), (p + 2 * PAGE - 4096, p + 2 * PAGE + 4096)]
    );
    write(2 * PAGE + 70);
    // Two askings' writes are in what is taken after both.
    assert!(writes(p, p + 4 * PAGE, false).is_some());
    write(2 * PAGE + 9000);
    assert_eq!(tracked.take(p, p + 4 * PAGE), [(p + 2 * PAGE, p + 2 * PAGE + 4096), (p + 2 * PAGE + 8192, p + 2 * PAGE + 12288)]);
    // SAFETY: our allocation.
    unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
}

/// The frames pages were last landed in.
#[test]
fn landed_pages_are_known_by_their_frame() {
    static LANDED: Landed = Landed([const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; Landed::PIECES]);
    let landed = &LANDED;
    for page in [0, 5, Landed::PAGES - 1, Landed::PAGES, 7 * Landed::PAGES + 3] {
        assert!(!landed.since(page, 0, 100));
        landed.note(page, 100);
        assert!(landed.since(page, 100, 100) && landed.since(page, 0, 100) && landed.since(page, 90, 110));
        assert!(!landed.since(page, 101, 110));
        landed.note(page, 0x1_0000_0005);
        assert!(landed.since(page, 0x1_0000_0000, 0x1_0000_0007) && !landed.since(page, 0x1_0000_0006, 0x1_0000_0007));
        assert!(!landed.since(page + 1, 0, 0x1_0000_0007));
    }
    landed.note(usize::MAX / 4096, 1);
    assert!(!landed.since(usize::MAX / 4096, 0, 1));
}

/// The set of blocks against another of the same.
#[test]
fn blocks_are_a_set() {
    let (mut set, mut other) = (Blocks::new(), BTreeSet::new());
    let mut seed = 1u64;
    let mut next = |n: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize % n
    };
    for round in 0..40_000 {
        // About the ends of pieces and words.
        let block = [0, 63, Blocks::IN_PIECE, 3 * Blocks::IN_PIECE - 70][next(4)] + next(140);
        if next(3) == 0 {
            assert_eq!(set.remove(block), other.remove(&block));
        } else {
            assert_eq!(set.insert(block), other.insert(block));
        }
        assert_eq!(set.contains(block), other.contains(&block));
        if round % 64 == 0 {
            let a = [0, 63, Blocks::IN_PIECE, 3 * Blocks::IN_PIECE - 70][next(4)] + next(140);
            let b = a + [next(10), next(300), 2 * Blocks::IN_PIECE + next(100)][next(3)];
            assert!(set.range(a..b).eq(other.range(a..b).copied()), "{a}..{b}");
            assert!(set.range(a..).eq(other.range(a..).copied()) && set.range(..).eq(other.iter().copied()));
            assert!(set.range(a..=b).eq(other.range(a..=b).copied()));
            assert_eq!(set.len(), other.len());
        }
    }
    assert_eq!(set.range(5..usize::MAX).next(), other.range(5..).next().copied());
}

/// What is known of a block by its bit is so at every view of its page:
/// the write through one takes the bits of all.
#[test]
fn bits_go_at_every_view() {
    const SIZE: usize = 2 * BLOCK;
    let (section, one, two) = two_views(SIZE);
    let ask = |at: usize| writes(at, at + SIZE, true).expect("a view");
    for at in [one, two, one, two] {
        ask(at);
    }
    assert!(published(one, one + SIZE) && published(two, two + SIZE));
    let (before, _) = ask(two);
    // SAFETY: the first view, opened by the handler.
    unsafe { ((one + BLOCK + 8) as *mut u8).write(1) };
    assert!(published(one, one + BLOCK) && published(two, two + BLOCK), "the other block is as it was");
    assert!(!published(one + BLOCK, one + SIZE) && !published(two + BLOCK, two + SIZE));
    let told = |at: usize| ask(at).1.iter().find(|w| w.0 == (at + BLOCK) / PAGE).map_or(0, |w| w.1);
    assert!(told(two) > before && told(one) > before);
    drop_views(section, &[one, two], SIZE);
}

/// While the kernel writes through one view, the same page at another
/// is told as written at every asking, and the first is not guarded.
#[test]
fn what_the_kernel_writes_through_is_not_guarded_for_another_view() {
    let (section, one, two) = two_views(BLOCK);
    let page = one / PAGE;
    assert_eq!(guard(one, one + BLOCK), Some(vec![page]));
    assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
    let writing = kernel_writing(two, 100);
    for _ in 0..3 {
        assert_eq!(guard(one, one + BLOCK), Some(vec![page]), "the kernel may be writing it");
        assert!(writable(two));
    }
    assert!(!written_since_taken(one, 8), "by whom is not known");
    drop(writing);
    assert_eq!(guard(one, one + BLOCK), Some(vec![page]));
    assert!(!writable(two));
    assert_eq!(guard(one, one + BLOCK), Some(Vec::new()));
    // SAFETY: opened by the handler.
    unsafe { ((two + 8) as *mut u8).write(1) };
    assert_eq!(guard(one, one + BLOCK), Some(vec![page]));
    drop_views(section, &[one, two], BLOCK);
}

/// What shaders wrote, put in past the guards and told at one view, is
/// told at the others, whose copies have not got it; and so is what we
/// wrote while a submission's one asking stands.
#[test]
fn what_is_put_in_beside_the_guards_is_told_at_every_view() {
    let (section, one, two) = two_views(BLOCK);
    writes(two, two + BLOCK, true).expect("a view");
    let (before, _) = writes(two, two + BLOCK, true).expect("a view");
    assert!(published(two, two + BLOCK));
    landed_beside(&[one / 4096 + 3], &[(one + 3 * 4096, 16)]);
    let (_, told) = writes(two, two + BLOCK, true).expect("a view");
    assert!(told.iter().any(|w| w.0 == two / PAGE && w.1 > before), "{told:?} after {before}");
    assert!(landed_lately(two + 3 * 4096, 16, 1) && !landed_lately(two + 4 * 4096, 16, 1));
    drop_views(section, &[one, two], BLOCK);
}

/// A view is cut in two by the runtime: until that is done, its pages
/// count as written wherever else they are shown.
#[test]
fn a_view_being_split_leaves_its_pages_unguarded_elsewhere() {
    const SIZE: usize = 3 * BLOCK;
    let (section, one, two) = two_views(SIZE);
    let pages: Vec<usize> = (0..3).map(|k| one / PAGE + k).collect();
    assert_eq!(guard(one, one + SIZE), Some(pages.clone()));
    assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
    let changing = title_memory_remapping();
    title_memory_splitting(two, SIZE);
    title_memory_unmapping(two, SIZE);
    for _ in 0..2 {
        assert_eq!(guard(one, one + SIZE), Some(pages.clone()), "written through the part that stays");
    }
    // The first and last page stay mapped.
    for k in [0, 2] {
        title_memory_remapped(two + k * BLOCK, BLOCK);
        crate::inplace::title_memory_view(two + k * BLOCK, BLOCK, section, (k * BLOCK) as u64);
    }
    drop(changing);
    assert_eq!(guard(one, one + SIZE), Some(pages));
    assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
    assert!(!writable(two) && writable(two + BLOCK) && !writable(two + 2 * BLOCK));
    drop_views(section, &[one, two], SIZE);
}

/// A page shown in more than `MANY_VIEWS` views is guarded at each by
/// itself: the others stay writable, and nothing looks through them all.
#[test]
fn a_page_of_many_views_is_guarded_one_view_at_a_time() {
    let (section, one, two) = two_views(BLOCK);
    let filler = one + (1 << 45);
    for n in 0..MANY_VIEWS {
        crate::inplace::title_memory_view(filler + n * BLOCK, BLOCK, section, 0);
    }
    assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
    assert!(writable(two), "another view of it is left alone");
    // SAFETY: the guarded view, opened by the handler.
    unsafe { ((one + 8) as *mut u8).write(1) };
    assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
    title_memory_remapped(filler, MANY_VIEWS * BLOCK);
    drop_views(section, &[one, two], BLOCK);
}

/// A write or mapping that runs past the end of the address space (a
/// title's `fread` with a huge count) covers up to the end; the sum used
/// to overflow. Up there no test has guarded blocks.
#[test]
fn ranges_past_the_address_space_end_do_not_overflow() {
    let top = usize::MAX - 16;
    writing(top, PAGE);
    title_memory_remapped(top, usize::MAX);
}
