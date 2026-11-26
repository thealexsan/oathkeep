use super::*;
pub(super) use super::analyze::{elsewhere, holder, splittable_cuts};
pub(super) use super::forwarding::Forwarding;
pub(super) use super::levels::{assign_levels_direct, assign_levels_segtree};
pub(super) use super::plan::{CutQuery, plan, plan_inner};
pub(super) use super::record::{DONE, POOL, record};

mod forwarding;
mod integration;

use super::*;

pub(super) fn copy(dst: usize, src: usize, len: usize, asked: bool) -> Does {
    Does::Copy { dst, src, len, asked }
}

pub(super) fn fill(src: usize, len: usize, asked: bool) -> Does {
    Does::Fill { src, len, asked, to: true }
}

pub(super) fn stuck(src: usize, len: usize) -> Does {
    Does::Fill { src, len, asked: false, to: false }
}

/// What is done on the card, as (place, level), in stage 2.
pub(super) fn planned(does: &[Does]) -> Vec<(usize, u32)> {
    plan(does, 2, true, &mut None).unwrap()
}

/// Only what is asked for is taken, all of it, and a step that stands
/// in none of their ways stays where it is.
#[test]
fn steps_apart_from_one_another_are_taken() {
    let does = [
        copy(1000, 5000, 100, false),
        fill(2000, 64, true),
        Does::Refresh { shadow: 1, from: 0, to: 9000 },
        copy(3000, 6000, 100, true),
        fill(1000, 64, false),
        Does::Read { from: 3100, to: 4000, asked: false },
        stuck(3100, 5),
    ];
    assert_eq!(planned(&does), [(1, 0), (3, 0)]);
    // Two fills of the same bytes.
    assert_eq!(plan(&[fill(0, 64, true), fill(32, 64, true)], 1, true, &mut None), Ok(vec![(0, 0), (1, 0)]));
    assert_eq!(plan(&[fill(0, 64, false)], 1, true, &mut None), Ok(vec![]));
    // Ends that touch are apart.
    let does = [
        copy(1000, 5000, 100, true),
        copy(900, 9000, 100, false),
        copy(1100, 9000, 5, false),
        copy(4900, 9000, 100, false),
    ];
    assert_eq!(planned(&does), [(0, 0)]);
}

/// What depends on a taken step, or it on it, is taken too, on the level
/// the order of the steps asks for.
#[test]
fn steps_that_depend_on_one_another_are_taken_in_order() {
    let taken = copy(1000, 5000, 100, true);
    // Reads what it writes, behind and ahead of it.
    assert_eq!(planned(&[taken, fill(1099, 64, false)]), [(0, 0), (1, 1)]);
    assert_eq!(planned(&[fill(1099, 64, false), taken]), [(0, 0), (1, 1)]);
    assert_eq!(planned(&[taken, copy(7000, 1050, 10, false)]), [(0, 0), (1, 1)]);
    // Writes what it reads, and what it writes.
    assert_eq!(planned(&[copy(5099, 9000, 1, false), taken]), [(0, 0), (1, 1)]);
    assert_eq!(planned(&[taken, copy(5099, 9000, 1, false)]), [(0, 0), (1, 1)]);
    assert_eq!(planned(&[taken, copy(999, 9000, 2, false)]), [(0, 0), (1, 1)]);
    // The same list twice.
    assert_eq!(planned(&[taken, taken]), [(0, 0), (1, 1)]);
    // A chain, with steps apart from it in between, and two fills of one
    // copy on one level.
    let does = [
        copy(8000, 7000, 50, false),
        taken,
        copy(2000, 1010, 20, false),
        fill(90_000, 10, false),
        fill(2000, 20, false),
        fill(2010, 20, true),
        copy(1000, 2005, 5, false),
        fill(1000, 100, false),
    ];
    assert_eq!(planned(&does), [(1, 0), (2, 1), (4, 2), (5, 2), (6, 2), (7, 3)]);
    // A fill is in nobody's way.
    assert_eq!(planned(&[taken, fill(5000, 100, false)]), [(0, 0)]);
    // A copy ahead that a taken one only shares its source with.
    assert_eq!(planned(&[copy(7000, 5000, 10, false), taken]), [(1, 0)]);
}

/// A copy at its place in a list is in the way of what reads or writes its
/// bytes behind it when cross-list cutting looks (`forward = false`), not
/// of what did ahead of it.
#[test]
fn copies_at_their_place_stand_in_the_way_of_what_is_behind_them() {
    let placed = Does::Placed {
        dst: 2000,
        src: 3000,
        len: 64,
        cpu_src: false,
        waits: false,
    };
    let behind = |does: &[Does]| {
        let mut found = None;
        let refused = plan(does, 2, true, &mut found);
        assert!(refused.is_ok() || found.is_some() && refused == Err(BEHIND_PLACED));
        assert_eq!(
            plan(does, 2, false, &mut None).is_err(),
            does.len() > 1 && !matches!(does, [_, Does::Fill { src: 5000, .. }])
        );
        found
    };
    assert_eq!(behind(&[fill(2032, 64, true), placed]), None);
    assert_eq!(behind(&[placed, fill(2032, 64, true)]), Some((0, 1)));
    assert_eq!(behind(&[placed, fill(5000, 64, true)]), None);
    assert_eq!(behind(&[fill(2000, 8, true), placed, fill(2000, 8, true)]), Some((1, 2)));
    // One that writes what it copies from, and one that writes over it.
    assert_eq!(behind(&[copy(3000, 5000, 8, true), placed]), None);
    assert_eq!(behind(&[placed, copy(3000, 5000, 8, true)]), Some((0, 1)));
    assert_eq!(behind(&[copy(2060, 5000, 8, true), placed]), None);
    assert_eq!(behind(&[placed, copy(2060, 5000, 8, true)]), Some((0, 1)));
}

pub(super) fn placed_item(dst: usize, src: usize, len: usize) -> Does {
    Does::Placed {
        dst,
        src,
        len,
        cpu_src: false,
        waits: false,
    }
}

pub(super) fn plan_forwarded(does: &[Does]) -> Result<(Vec<(usize, u32)>, Forwarding), &'static str> {
    plan_inner(does, 2, true, true, &mut None, None)
}

pub(super) fn segs_of(fwd: &Forwarding, does: &[Does], i: usize) -> Vec<(usize, usize, usize)> {
    let mut buf = [(0usize, 0usize, 0usize); 1];
    fwd.segments_of(does, i, &mut buf).unwrap().0.to_vec()
}
