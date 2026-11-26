use super::{Flights, changed_runs, merge_piece, outside};

/// A range two flights declared is free only when both are done, in
/// either order; a submission that declares it many times counts once.
#[test]
fn a_range_stays_closed_until_every_flight_that_declared_it_is_done() {
    let (a, b) = ((0x1000, 0x2000), (0x1800, 0x3000));
    let mut flights = Flights::default();
    for _ in 0..3 {
        flights.declare(a, 7);
    }
    flights.declare(b, 7);
    flights.declare(a, 8);
    // Flight 7 lands: `a` is still flight 8's, `b` was only flight 7's.
    assert_eq!(flights.done(&[a, b]), vec![b]);
    // Declared again meanwhile by a third.
    flights.declare(a, 9);
    assert_eq!(flights.done(&[a]), vec![]);
    assert_eq!(flights.done(&[a]), vec![a]);
    assert!(flights.0.is_empty());
    // The later flight first: the same.
    flights.declare(a, 1);
    flights.declare(a, 2);
    assert_eq!(flights.done(&[a]), vec![]);
    assert_eq!(flights.done(&[a]), vec![a]);
    // A range never counted is free at once.
    assert_eq!(flights.done(&[b]), vec![b]);
    // `done_or_clear` returns `None` iff all active flights on the shadow finished.
    flights.declare(a, 10);
    flights.declare(b, 10);
    flights.declare(a, 11);
    assert_eq!(flights.done_or_clear(&[a, b]), Some(vec![b]));
    assert_eq!(flights.done_or_clear(&[a]), None);
}

/// The merge moves exactly the bytes the card changed, into title
/// memory and into the reference, and stores nothing else.
#[test]
fn the_merge_stores_only_what_the_card_changed() {
    let held0: Vec<u8> = (0..200u32).map(|i| (i * 5 + 1) as u8).collect();
    let mut now = held0.clone();
    for at in [0usize, 3, 4, 5, 64, 199] {
        now[at] ^= 0x33;
    }
    // The title wrote other bytes meanwhile, and one the card wrote too.
    let mut title = held0.clone();
    for at in [1usize, 5, 100] {
        title[at] = 0xee;
    }
    let title0 = title.clone();
    let mut held = held0.clone();
    assert!(merge_piece(&now, &mut held, &mut title));
    for at in 0..200 {
        if now[at] != held0[at] {
            assert_eq!((title[at], held[at]), (now[at], now[at]), "the card's byte at {at}");
        } else {
            assert_eq!((title[at], held[at]), (title0[at], held0[at]), "untouched at {at}");
        }
    }
    // Again with nothing new: nothing changes and nothing is reported.
    let (title1, held1) = (title.clone(), held.clone());
    assert!(!merge_piece(&now, &mut held, &mut title));
    assert_eq!((title, held), (title1, held1));
}

#[test]
fn ranges_are_cut_out() {
    assert_eq!(outside(10, 50, vec![]), vec![(10, 50)]);
    assert_eq!(outside(10, 50, vec![(0, 10), (50, 60)]), vec![(10, 50)]);
    assert_eq!(outside(10, 50, vec![(30, 35), (0, 12), (34, 40), (48, 99)]), vec![(12, 30), (40, 48)]);
    assert_eq!(outside(10, 50, vec![(0, 99)]), vec![]);
}

/// A model of a copy in the PC's memory under the relay: the card
/// writes `copy` directly; `snap` is the snapshot; `title` is title
/// memory. Flight A and flight B declare overlapping ranges and both
/// are in the air; the title's CPU writes in between and its bytes are
/// put in beside the flights' (`merge_under_writes`: into the copy and
/// the snapshot); A lands while B is still writing; then B lands.
#[test]
fn two_flights_overlapping_ranges_and_a_cpu_write_in_between() {
    const N: usize = 64;
    let base: Vec<u8> = (0..N as u32).map(|i| i as u8).collect();
    let (mut copy, mut title) = (base.clone(), base.clone());
    // A declares [0, 40), B declares [24, 64): each byte's snapshot is
    // taken when the first range that holds it is declared.
    let (a, b) = ((0usize, 40usize), (24usize, 64usize));
    let mut snap = base.clone();
    // The card runs A: writes 4..12 and 30..34.
    for at in (4..12).chain(30..34) {
        copy[at] = 0xa0;
    }
    // The title's CPU writes 8..10 (over A's bytes), 20 and 50, after
    // A's refresh: the next submission's preparing finds them against
    // the snapshot and puts them in, into the copy and the snapshot.
    for at in [8usize, 9, 20, 50] {
        title[at] = 0x77;
    }
    for (at, n) in changed_runs(&title, &snap) {
        copy[at..at + n].copy_from_slice(&title[at..at + n]);
        snap[at..at + n].copy_from_slice(&title[at..at + n]);
    }
    // The card starts B: it has written 32..36 so far, over A's 32..34.
    for at in 32..36 {
        copy[at] = 0xb0;
    }
    // A lands (B still on the card): its range is merged. The copy is
    // read once and not written.
    let seen = copy.clone();
    {
        let mut t = title[a.0..a.1].to_vec();
        assert!(merge_piece(&seen[a.0..a.1], &mut snap[a.0..a.1], &mut t));
        title[a.0..a.1].copy_from_slice(&t);
    }
    assert_eq!(copy, seen, "the merge stores nothing into the copy");
    // A's bytes are in title memory, except where the title wrote
    // after A's shaders did (8, 9: the title's); B's first bytes came
    // along; nothing of B's range beyond A's was touched.
    for at in (4..8).chain(10..12).chain(30..32) {
        assert_eq!(title[at], 0xa0, "A's byte at {at}");
    }
    assert_eq!((title[8], title[9], title[20], title[50]), (0x77, 0x77, 0x77, 0x77));
    for at in 32..36 {
        assert_eq!(title[at], 0xb0, "B's byte so far at {at}");
    }
    // The card finishes B: 36..44.
    for at in 36..44 {
        copy[at] = 0xb1;
    }
    // The title's CPU writes 40 after B's shader did: found against
    // the snapshot at the next preparing, which is before B lands.
    title[40] = 0x55;
    for (at, n) in changed_runs(&title[b.0..b.1], &snap[b.0..b.1]) {
        let at = at + b.0;
        copy[at..at + n].copy_from_slice(&title[at..at + n]);
        snap[at..at + n].copy_from_slice(&title[at..at + n]);
    }
    // B lands.
    let seen = copy.clone();
    {
        let mut t = title[b.0..b.1].to_vec();
        assert!(merge_piece(&seen[b.0..b.1], &mut snap[b.0..b.1], &mut t));
        title[b.0..b.1].copy_from_slice(&t);
    }
    // B's results are there, never replaced by A's older ones.
    for at in 32..36 {
        assert_eq!(title[at], 0xb0);
    }
    for at in (36..40).chain(41..44) {
        assert_eq!(title[at], 0xb1, "B's byte at {at}");
    }
    assert_eq!(title[40], 0x55, "the title wrote it last");
    // In the declared ranges the copy, the snapshot and title memory
    // now agree byte for byte: nothing is left to hand back, and
    // nothing was lost.
    assert_eq!(copy[a.0..b.1], title[a.0..b.1]);
    assert_eq!(snap[a.0..b.1], title[a.0..b.1]);
    // A third merge finds nothing.
    let mut t = title.clone();
    assert!(!merge_piece(&copy, &mut snap, &mut t));
}

/// A model of a copy in the card's memory under the relay: `card` is
/// the card's side, `cpu` the CPU's side, `title` title memory. The
/// title's changed bytes go into the CPU's side at once and up to the
/// card with a later send; pieces fetched before that send must not
/// take the card's older bytes there for something the card wrote.
#[test]
fn title_bytes_not_yet_on_the_card_are_not_taken_for_shader_writes() {
    const N: usize = 48;
    let base: Vec<u8> = (0..N as u32).map(|i| (i + 100) as u8).collect();
    let (mut card, mut cpu, mut title) = (base.clone(), base.clone(), base.clone());
    // Flight A's shaders write 10..14 on the card.
    for at in 10..14 {
        card[at] = 0xa0;
    }
    // The title's CPU writes 12 (over A's) and 30; the preparing of
    // the next submission puts both into the CPU's side and notes them
    // to go up (send 5). They are not on the card yet.
    title[12] = 0x77;
    title[30] = 0x78;
    let mut noted = Vec::new();
    for (at, n) in changed_runs(&title, &cpu) {
        cpu[at..at + n].copy_from_slice(&title[at..at + n]);
        noted.push((at, at + n, 5u64));
    }
    // A's pieces are fetched after send 4, before send 5: the fetch
    // holds A's bytes and the card's old bytes at 12 and 30.
    let fetched = card.clone();
    let fetched_after = 4u64;
    // Send 5 goes up behind it.
    for &(a, b, _) in &noted {
        card[a..b].copy_from_slice(&cpu[a..b]);
    }
    // The merge leaves out what went up after the fetch.
    let skip: Vec<(usize, usize)> = noted.iter().filter(|s| s.2 > fetched_after).map(|s| (s.0, s.1)).collect();
    let mut changed = false;
    for (a, b) in outside(0, N, skip) {
        let mut t = title[a..b].to_vec();
        changed |= merge_piece(&fetched[a..b], &mut cpu[a..b], &mut t);
        title[a..b].copy_from_slice(&t);
    }
    assert!(changed);
    // A's bytes arrived; the title's two writes stand, in title memory
    // and on both sides of the copy.
    for at in [10usize, 11, 13] {
        assert_eq!((title[at], cpu[at]), (0xa0, 0xa0));
    }
    assert_eq!((title[12], cpu[12], card[12]), (0x77, 0x77, 0x77));
    assert_eq!((title[30], cpu[30], card[30]), (0x78, 0x78, 0x78));
    assert_eq!(card, cpu);
    assert_eq!(card, title);
    // Without the leaving out, the card's old byte at 30 would have
    // been taken for a shader write and put over the title's.
    let mut wrong_title = title.clone();
    let mut wrong_cpu = cpu.clone();
    merge_piece(&fetched, &mut wrong_cpu, &mut wrong_title);
    assert_eq!(wrong_title[30], base[30]);
}
