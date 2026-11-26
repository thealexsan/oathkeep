use std::collections::BTreeMap;

use super::super::WHOLE;
use super::super::journal::{Journal, normalize_ahead};

#[test]
fn the_parts_of_a_page_written_since_it_was_taken() {
    let mut j = Journal {
        written: BTreeMap::new(),
        parts: std::collections::HashMap::with_hasher(std::hash::BuildHasherDefault::new()),
        ahead: Vec::new(),
        ahead_asked: 0,
        kept: false,
    };
    assert_eq!(j.parts_since(7, 0), WHOLE);
    assert_eq!(j.note(7, WHOLE, 3), 0);
    // Parts 0 and 5 at 4, part 5 again at 6: at 2 the whole page is due.
    assert_eq!(j.note(7, 0b10_0001, 4), 3);
    assert_eq!(j.note(7, 0b10_0000, 6), 4);
    assert_eq!(j.parts_since(7, 2), WHOLE);
    assert_eq!(j.parts_since(7, 3), 0b10_0001);
    assert_eq!(j.parts_since(7, 4), 0b10_0000);
    assert_eq!(j.parts_since(7, 5), 0b10_0000);
    assert_eq!(j.parts_since(7, 6), 0);
    // Written whole: all of it, whoever took it before.
    assert_eq!(j.note(7, WHOLE, 8), 6);
    assert_eq!(j.parts_since(7, 7), WHOLE);
    // A page never written before: what was not named was never written.
    assert_eq!(j.note(9, 0b100, 9), 0);
    assert_eq!(j.parts_since(9, 5), 0b100);
}

#[test]
fn normalize_ahead_and_has_ahead_merge_and_binary_search() {
    let mut ahead = vec![(30, 40), (10, 20), (20, 25), (12, 18), (50, 60)];
    normalize_ahead(&mut ahead);
    assert_eq!(ahead, vec![(10, 25), (30, 40), (50, 60)]);

    let j = Journal {
        written: BTreeMap::new(),
        parts: std::collections::HashMap::with_hasher(std::hash::BuildHasherDefault::new()),
        ahead,
        ahead_asked: 1,
        kept: true,
    };
    assert!(j.has_ahead(10, 25));
    assert!(j.has_ahead(15, 22));
    assert!(j.has_ahead(30, 40));
    assert!(j.has_ahead(55, 56));
    assert!(!j.has_ahead(10, 26));
    assert!(!j.has_ahead(25, 30));
    assert!(!j.has_ahead(0, 10));
    assert!(!j.has_ahead(59, 61));
}
