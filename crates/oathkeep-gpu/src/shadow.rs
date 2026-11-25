use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::{HostList, host};
use crate::watch::PAGE;

pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    start: u64,
    len: u64,
    copied: Mutex<Vec<u64>>,
    uncopied: AtomicU64,
    synced_at: AtomicU64,
    ever_written: AtomicBool,
    watched: AtomicU8,
    pending: Mutex<Vec<(usize, usize)>>,
    flights: Mutex<Flights>,
    before: Mutex<Option<Vec<u8>>>,
    kept: Mutex<(Vec<bool>, bool)>,
    used: AtomicU64,
    asked: [AtomicU64; 2],
    unsized_asks: AtomicU64,
    id: u64,
    card: Option<Card>,
    window: bool,
    place: Option<(crate::inplace::PlaceHeap, u64)>,
}

struct Card {
    staging: ID3D12Resource,
    before: Mutex<Option<ID3D12Resource>>,
    dirty: Mutex<Vec<(usize, usize)>>,
    sent: Mutex<Vec<(usize, usize, u64)>>,
    size: u64,
}

pub(crate) fn cards() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CARD_BUFFERS").is_none_or(|v| v != "0"))
}

fn card_limits() -> (u64, u64) {
    static LIMITS: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *LIMITS.get_or_init(|| {
        let mb = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default)
                << 20
        };
        let min_bytes = std::env::var("OATHKEEP_CARD_BUFFERS_MIN_KB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|kb| (kb << 10).max(64 << 10))
            .unwrap_or_else(|| mb("OATHKEEP_CARD_BUFFERS_MIN_MB", 1).max(1 << 20));
        (min_bytes, mb("OATHKEEP_CARD_BUFFERS_MB", 1024))
    })
}

static CARD_BYTES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

fn card_room_kept_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CARD_ROOM_FOR_BUFFERS").is_some_and(|v| v == "1"))
}

pub(crate) fn card_room_kept() -> u64 {
    if !card_room_kept_on() || !cards() {
        return 0;
    }
    card_limits()
        .1
        .saturating_sub(CARD_BYTES[0].load(Ordering::Relaxed))
}

static CARDED: Mutex<Vec<std::sync::Weak<Shadow>>> = Mutex::new(Vec::new());

pub(crate) static CARD_STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(crate) fn card_stats() -> [u64; 10] {
    let mut out = [0u64; 10];
    for (i, stat) in CARD_STATS.iter().enumerate() {
        out[i] = stat.swap(0, Ordering::Relaxed);
    }
    out[7] = CARDED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|w| w.strong_count() > 0)
        .count() as u64;
    out[8] = CARD_BYTES[0].load(Ordering::Relaxed) + WINDOW_CARD_BYTES.load(Ordering::Relaxed);
    out[9] = CARD_BYTES[1].load(Ordering::Relaxed);
    out
}

pub(crate) fn card_hash_bytes(size: u64) -> u64 {
    (size / PIECE as u64 + 2) * 16
}

fn card_for(size: u64) -> Option<ID3D12Resource> {
    if !cards() {
        return None;
    }
    let (least, most) = card_limits();
    if size < least || size >= 1 << 30 {
        return None;
    }
    let host = host()?;
    if !host.card_ready() {
        return None;
    }
    let tenths = if card_room_kept_on() { 9 } else { 8 };
    if let Some([budget, used, ..]) = host.video_memory()
        && used + size > budget / 10 * tenths
    {
        return None;
    }
    if CARD_BYTES[0].fetch_add(size, Ordering::Relaxed) + size > most {
        CARD_BYTES[0].fetch_sub(size, Ordering::Relaxed);
        return None;
    }
    let buffer = host.uav_buffer(size);
    if buffer.is_none() {
        CARD_BYTES[0].fetch_sub(size, Ordering::Relaxed);
    }
    buffer
}

fn window_card_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_CARD_WINDOWS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
            << 20
    })
}

static WINDOW_CARD_BYTES: AtomicU64 = AtomicU64::new(0);

fn window_card_for(size: u64) -> Option<ID3D12Resource> {
    let most = window_card_most();
    if most == 0 || !cards() {
        return None;
    }
    let host = host()?;
    if !host.card_ready() {
        return None;
    }
    if let Some([budget, used, ..]) = host.video_memory()
        && used + size > budget / 10 * 8
    {
        return None;
    }
    if WINDOW_CARD_BYTES.fetch_add(size, Ordering::Relaxed) + size > most {
        WINDOW_CARD_BYTES.fetch_sub(size, Ordering::Relaxed);
        return None;
    }
    let buffer = host.uav_buffer(size);
    if buffer.is_none() {
        WINDOW_CARD_BYTES.fetch_sub(size, Ordering::Relaxed);
    }
    buffer
}

pub(crate) fn card_dirty(sent_as: u64) -> Vec<(Arc<Shadow>, Vec<(usize, usize)>)> {
    let mut out = Vec::new();
    let mut carded = CARDED.lock().unwrap_or_else(|e| e.into_inner());
    let fetched = RELAY_FETCHED.load(Ordering::Acquire);
    carded.retain(|weak| {
        let Some(shadow) = weak.upgrade() else {
            return false;
        };
        let ranges = shadow
            .card
            .as_ref()
            .map(|c| {
                let mut dirty = c.dirty.lock().unwrap_or_else(|e| e.into_inner());
                let ranges = std::mem::take(&mut *dirty);
                if relay() {
                    let mut sent = c.sent.lock().unwrap_or_else(|e| e.into_inner());
                    sent.retain(|s| s.2 > fetched);
                    sent.extend(ranges.iter().map(|&(a, b)| (a, b, sent_as)));
                }
                ranges
            })
            .unwrap_or_default();
        if !ranges.is_empty() {
            out.push((shadow, joined(ranges)));
        }
        true
    });
    out
}

fn joined(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

pub(crate) fn joined_piece_runs(mut runs: Vec<CardSpan>) -> Vec<CardSpan> {
    runs.sort_unstable_by_key(|r| r.first);
    let mut out: Vec<CardSpan> = Vec::with_capacity(runs.len());
    for run in runs {
        match out.last_mut() {
            Some(last) if run.first <= last.first + last.pieces => {
                last.pieces = last.pieces.max(run.first + run.pieces - last.first);
                last.to = last.to.max(run.to);
            }
            _ => out.push(run),
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CardSpan {
    pub(crate) from: u32,
    pub(crate) to: u32,
    pub(crate) first: u32,
    pub(crate) pieces: u32,
    pub(crate) skew: u32,
}

fn card_span_of(start: usize, from: usize, to: usize) -> CardSpan {
    let pieces = pieces_of(start, from, to);
    CardSpan {
        from: from as u32,
        to: to as u32,
        first: pieces.start as u32,
        pieces: pieces.len() as u32,
        skew: (start % PIECE) as u32,
    }
}

fn card_piece_of(start: usize, len: usize, span: &CardSpan, i: u32) -> (usize, usize) {
    let (lo, hi) = piece_at(start, len, (span.first + i) as usize);
    (lo.max(span.from as usize), hi.min(span.to as usize))
}

#[cfg(test)]
mod card_tests {
    use super::{CardSpan, PIECE, card_piece_of, card_span_of, joined, joined_piece_runs};

    #[test]
    fn piece_runs_join_without_losing_or_adding_pieces() {
        let run = |first: u32, pieces: u32| CardSpan {
            from: first * 65536,
            to: (first + pieces) * 65536,
            first,
            pieces,
            skew: 7,
        };
        let runs = vec![
            run(40, 1),
            run(3, 2),
            run(4, 1),
            run(5, 3),
            run(40, 1),
            run(9, 1),
            run(41, 2),
            run(100, 1),
        ];
        let mut wanted: Vec<u32> = runs
            .iter()
            .flat_map(|r| r.first..r.first + r.pieces)
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        let joined = joined_piece_runs(runs);
        let got: Vec<u32> = joined
            .iter()
            .flat_map(|r| r.first..r.first + r.pieces)
            .collect();
        assert_eq!(got, wanted);
        assert_eq!(
            joined
                .iter()
                .map(|r| (r.first, r.pieces))
                .collect::<Vec<_>>(),
            [(3, 5), (9, 1), (40, 3), (100, 1)]
        );
        assert!(joined.iter().all(|r| r.skew == 7));
        assert!(joined_piece_runs(Vec::new()).is_empty());
    }

    fn shader_piece(span: &CardSpan, i: u32) -> (u32, u32) {
        let piece = span.first + i;
        let lo = piece * 65536;
        let lo = if lo > span.skew { lo - span.skew } else { 0 };
        let hi = (piece + 1) * 65536 - span.skew;
        (lo.max(span.from), hi.min(span.to))
    }

    #[test]
    fn the_shader_and_the_merge_cut_the_same_pieces() {
        for start in [
            0usize,
            4096,
            PIECE - 16,
            PIECE,
            3 * PIECE + 0x1f00,
            0xd_b790_df00,
        ] {
            let len = 5 * PIECE + 4096;
            for (from, to) in [
                (0, len),
                (0, 1),
                (16, PIECE),
                (PIECE - 4, PIECE + 4),
                (2 * PIECE, len),
                (12345, 4 * PIECE + 7),
            ] {
                let span = card_span_of(start, from, to);
                let mut at = from;
                for i in 0..span.pieces {
                    let (lo, hi) = card_piece_of(start, len, &span, i);
                    assert_eq!(lo, at, "{start:#x} {from} {to} piece {i}");
                    assert!(hi > lo && hi - lo <= PIECE);
                    assert_eq!(
                        shader_piece(&span, i),
                        (lo as u32, hi as u32),
                        "{start:#x} {from} {to} piece {i}"
                    );
                    at = hi;
                }
                assert_eq!(at, to, "{start:#x} {from} {to}");
            }
        }
    }

    #[test]
    fn ranges_join_where_they_touch() {
        assert_eq!(
            joined(vec![(8, 12), (0, 4), (4, 6), (10, 20), (30, 31)]),
            [(0, 6), (8, 20), (30, 31)]
        );
        assert!(joined(Vec::new()).is_empty());
    }
}

pub(crate) fn relay() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("OATHKEEP_RELAY").map_or(!cfg!(test), |v| v == "1" || v == "2") && !keeps()
    })
}

static PREPARED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn preparing(number: u64) {
    PREPARED.store(number, Ordering::Relaxed);
}

static RELAY_FETCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn relay_fetched(number: u64) {
    RELAY_FETCHED.fetch_max(number, Ordering::AcqRel);
}

#[derive(Default)]
struct Flights(Vec<((usize, usize), u32, u64)>);

impl Flights {
    fn declare(&mut self, range: (usize, usize), number: u64) {
        match self.0.iter_mut().find(|f| f.0 == range) {
            Some(f) => {
                if f.2 != number {
                    f.1 += 1;
                    f.2 = number;
                }
            }
            None => self.0.push((range, 1, number)),
        }
    }

    fn done(&mut self, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let mut free = Vec::new();
        for range in ranges {
            match self.0.iter().position(|f| f.0 == *range) {
                Some(at) => {
                    self.0[at].1 = self.0[at].1.saturating_sub(1);
                    if self.0[at].1 == 0 {
                        self.0.swap_remove(at);
                        free.push(*range);
                    }
                }
                None => free.push(*range),
            }
        }
        free
    }
}

fn merge_piece(now: &[u8], held: &mut [u8], title: &mut [u8]) -> bool {
    let mut differs = 0u8;
    for ((n, h), t) in now.iter().zip(held.iter_mut()).zip(title.iter_mut()) {
        let wrote = if *n != *h { 0xffu8 } else { 0 };
        let merged = (*t & !wrote) | (*n & wrote);
        differs |= merged ^ *t;
        *t = merged;
        *h = (*h & !wrote) | (*n & wrote);
    }
    differs != 0
}

fn outside(from: usize, to: usize, mut skip: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    skip.sort_unstable();
    let mut out = Vec::new();
    let mut at = from;
    for (a, b) in skip {
        if b <= at {
            continue;
        }
        if a >= to {
            break;
        }
        if a > at {
            out.push((at, a));
        }
        at = at.max(b);
    }
    if at < to {
        out.push((at, to));
    }
    out
}

fn word_runs(address: usize, read: &[u8], merged: &[u8], mut put: impl FnMut(usize, usize)) {
    let n = read.len().min(merged.len());
    let mut i = 0;
    while i < n {
        match first_difference(&read[i..n], &merged[i..n]) {
            Some(d) => i += d,
            None => break,
        }
        let first = i - ((address + i) & 3).min(i);
        let mut end = first;
        while end < n {
            let word = (((address + end) | 3) + 1 - address).min(n);
            if read[end..word] == merged[end..word] {
                break;
            }
            end = word;
        }
        put(first, end - first);
        i = end;
    }
}

fn merge_race_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_MERGE_RACE").is_some_and(|v| v == "1"))
}

struct MergeRace {
    frame: u64,
    now: [u64; 7],
    said: Vec<String>,
    total: [u64; 7],
    quiet: u64,
}

static MERGE_RACE: Mutex<MergeRace> = Mutex::new(MergeRace {
    frame: 0,
    now: [0; 7],
    said: Vec::new(),
    total: [0; 7],
    quiet: 0,
});

fn merge_race(
    read: Option<&[u8]>,
    copy: (u64, u64),
    address: usize,
    merged: &[u8],
    which: &'static str,
) {
    if !merge_race_on() {
        return;
    }
    let Some(read) = read else {
        return;
    };
    let n = read.len().min(merged.len());
    let (read, merged) = (&read[..n], &merged[..n]);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    word_runs(address, read, merged, |off, len| runs.push((off, len)));
    let whole = [(0, n)];
    let looked: &[(usize, usize)] = &whole;
    let mut again = vec![0u8; n];
    let (mut put_back, mut both, mut first) = (0u64, 0u64, None);
    if crate::host::read_into(address as *const u8, &mut again) {
        for &(off, len) in looked {
            for i in off..off + len {
                if again[i] == read[i] || again[i] == merged[i] {
                    continue;
                }
                if merged[i] == read[i] {
                    put_back += 1;
                    first.get_or_insert(i);
                } else {
                    both += 1;
                }
            }
        }
    }
    let frame = crate::frame::current();
    let mut race = MERGE_RACE.lock().unwrap_or_else(|e| e.into_inner());
    if race.frame != frame {
        race.quiet += 1;
        if race.now[4] > 0 || race.quiet >= 100 {
            tracing::info!(
                frame = race.frame,
                stretches = race.now[0],
                stretch_bytes = race.now[1],
                put_back = race.now[2],
                both_changed = race.now[3],
                stretches_hit = race.now[4],
                word_runs = race.now[5],
                word_run_bytes = race.now[6],
                stretches_since_start = race.total[0],
                stretch_bytes_since_start = race.total[1],
                put_back_since_start = race.total[2],
                both_changed_since_start = race.total[3],
                stretches_hit_since_start = race.total[4],
                word_runs_since_start = race.total[5],
                word_run_bytes_since_start = race.total[6],
                first = ?race.said,
                "merge race: title bytes that changed between a merge's read of title memory and its write-back of what shaders wrote, in this frame (OATHKEEP_MERGE_RACE=1)"
            );
            race.quiet = 0;
        }
        race.frame = frame;
        race.now = [0; 7];
        race.said.clear();
    }
    let hit = u64::from(put_back + both > 0);
    let run_bytes: u64 = runs.iter().map(|r| r.1 as u64).sum();
    let add = [
        1,
        n as u64,
        put_back,
        both,
        hit,
        runs.len() as u64,
        run_bytes,
    ];
    for (i, v) in add.iter().enumerate() {
        race.now[i] += v;
        race.total[i] += v;
    }
    if hit > 0 && race.said.len() < 8 {
        race.said.push(format!(
            "{which}: copy {:#x}+{:#x} stretch {address:#x}+{n:#x}: {put_back} put back (first at {:#x}), {both} both changed",
            copy.0,
            copy.1,
            address + first.unwrap_or(0)
        ));
    }
}

#[cfg(test)]
mod relay_tests {
    use super::{Flights, changed_runs, merge_piece, outside};

    #[test]
    fn a_range_stays_closed_until_every_flight_that_declared_it_is_done() {
        let (a, b) = ((0x1000, 0x2000), (0x1800, 0x3000));
        let mut flights = Flights::default();
        for _ in 0..3 {
            flights.declare(a, 7);
        }
        flights.declare(b, 7);
        flights.declare(a, 8);
        assert_eq!(flights.done(&[a, b]), vec![b]);
        flights.declare(a, 9);
        assert_eq!(flights.done(&[a]), vec![]);
        assert_eq!(flights.done(&[a]), vec![a]);
        assert!(flights.0.is_empty());
        flights.declare(a, 1);
        flights.declare(a, 2);
        assert_eq!(flights.done(&[a]), vec![]);
        assert_eq!(flights.done(&[a]), vec![a]);
        assert_eq!(flights.done(&[b]), vec![b]);
    }

    #[test]
    fn the_merge_stores_only_what_the_card_changed() {
        let held0: Vec<u8> = (0..200u32).map(|i| (i * 5 + 1) as u8).collect();
        let mut now = held0.clone();
        for at in [0usize, 3, 4, 5, 64, 199] {
            now[at] ^= 0x33;
        }
        let mut title = held0.clone();
        for at in [1usize, 5, 100] {
            title[at] = 0xee;
        }
        let title0 = title.clone();
        let mut held = held0.clone();
        assert!(merge_piece(&now, &mut held, &mut title));
        for at in 0..200 {
            if now[at] != held0[at] {
                assert_eq!(
                    (title[at], held[at]),
                    (now[at], now[at]),
                    "the card's byte at {at}"
                );
            } else {
                assert_eq!(
                    (title[at], held[at]),
                    (title0[at], held0[at]),
                    "untouched at {at}"
                );
            }
        }
        let (title1, held1) = (title.clone(), held.clone());
        assert!(!merge_piece(&now, &mut held, &mut title));
        assert_eq!((title, held), (title1, held1));
    }

    #[test]
    fn ranges_are_cut_out() {
        assert_eq!(outside(10, 50, vec![]), vec![(10, 50)]);
        assert_eq!(outside(10, 50, vec![(0, 10), (50, 60)]), vec![(10, 50)]);
        assert_eq!(
            outside(10, 50, vec![(30, 35), (0, 12), (34, 40), (48, 99)]),
            vec![(12, 30), (40, 48)]
        );
        assert_eq!(outside(10, 50, vec![(0, 99)]), vec![]);
    }

    #[test]
    fn two_flights_overlapping_ranges_and_a_cpu_write_in_between() {
        const N: usize = 64;
        let base: Vec<u8> = (0..N as u32).map(|i| i as u8).collect();
        let (mut copy, mut title) = (base.clone(), base.clone());
        let (a, b) = ((0usize, 40usize), (24usize, 64usize));
        let mut snap = base.clone();
        for at in (4..12).chain(30..34) {
            copy[at] = 0xa0;
        }
        for at in [8usize, 9, 20, 50] {
            title[at] = 0x77;
        }
        for (at, n) in changed_runs(&title, &snap) {
            copy[at..at + n].copy_from_slice(&title[at..at + n]);
            snap[at..at + n].copy_from_slice(&title[at..at + n]);
        }
        for at in 32..36 {
            copy[at] = 0xb0;
        }
        let seen = copy.clone();
        {
            let mut t = title[a.0..a.1].to_vec();
            assert!(merge_piece(&seen[a.0..a.1], &mut snap[a.0..a.1], &mut t));
            title[a.0..a.1].copy_from_slice(&t);
        }
        assert_eq!(copy, seen, "the merge stores nothing into the copy");
        for at in (4..8).chain(10..12).chain(30..32) {
            assert_eq!(title[at], 0xa0, "A's byte at {at}");
        }
        assert_eq!(
            (title[8], title[9], title[20], title[50]),
            (0x77, 0x77, 0x77, 0x77)
        );
        for at in 32..36 {
            assert_eq!(title[at], 0xb0, "B's byte so far at {at}");
        }
        for at in 36..44 {
            copy[at] = 0xb1;
        }
        title[40] = 0x55;
        for (at, n) in changed_runs(&title[b.0..b.1], &snap[b.0..b.1]) {
            let at = at + b.0;
            copy[at..at + n].copy_from_slice(&title[at..at + n]);
            snap[at..at + n].copy_from_slice(&title[at..at + n]);
        }
        let seen = copy.clone();
        {
            let mut t = title[b.0..b.1].to_vec();
            assert!(merge_piece(&seen[b.0..b.1], &mut snap[b.0..b.1], &mut t));
            title[b.0..b.1].copy_from_slice(&t);
        }
        for at in 32..36 {
            assert_eq!(title[at], 0xb0);
        }
        for at in (36..40).chain(41..44) {
            assert_eq!(title[at], 0xb1, "B's byte at {at}");
        }
        assert_eq!(title[40], 0x55, "the title wrote it last");
        assert_eq!(copy[a.0..b.1], title[a.0..b.1]);
        assert_eq!(snap[a.0..b.1], title[a.0..b.1]);
        let mut t = title.clone();
        assert!(!merge_piece(&copy, &mut snap, &mut t));
    }

    #[test]
    fn title_bytes_not_yet_on_the_card_are_not_taken_for_shader_writes() {
        const N: usize = 48;
        let base: Vec<u8> = (0..N as u32).map(|i| (i + 100) as u8).collect();
        let (mut card, mut cpu, mut title) = (base.clone(), base.clone(), base.clone());
        for at in 10..14 {
            card[at] = 0xa0;
        }
        title[12] = 0x77;
        title[30] = 0x78;
        let mut noted = Vec::new();
        for (at, n) in changed_runs(&title, &cpu) {
            cpu[at..at + n].copy_from_slice(&title[at..at + n]);
            noted.push((at, at + n, 5u64));
        }
        let fetched = card.clone();
        let fetched_after = 4u64;
        for &(a, b, _) in &noted {
            card[a..b].copy_from_slice(&cpu[a..b]);
        }
        let skip: Vec<(usize, usize)> = noted
            .iter()
            .filter(|s| s.2 > fetched_after)
            .map(|s| (s.0, s.1))
            .collect();
        let mut changed = false;
        for (a, b) in outside(0, N, skip) {
            let mut t = title[a..b].to_vec();
            changed |= merge_piece(&fetched[a..b], &mut cpu[a..b], &mut t);
            title[a..b].copy_from_slice(&t);
        }
        assert!(changed);
        for at in [10usize, 11, 13] {
            assert_eq!((title[at], cpu[at]), (0xa0, 0xa0));
        }
        assert_eq!((title[12], cpu[12], card[12]), (0x77, 0x77, 0x77));
        assert_eq!((title[30], cpu[30], card[30]), (0x78, 0x78, 0x78));
        assert_eq!(card, cpu);
        assert_eq!(card, title);
        let mut wrong_title = title.clone();
        let mut wrong_cpu = cpu.clone();
        merge_piece(&fetched, &mut wrong_cpu, &mut wrong_title);
        assert_eq!(wrong_title[30], base[30]);
    }
}

static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let most = n.saturating_mul(4).max(1 << 20);
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n && v.capacity() <= most)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

fn give_back(v: Vec<u8>) {
    const KEPT: usize = 512 << 20;
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
    pool.truncate(8);
    let (mut seen, mut total) = (0, 0usize);
    pool.retain(|v| {
        seen += 1;
        total = total.saturating_add(v.capacity());
        seen == 1 || total <= KEPT
    });
}

const WATCHED_KERNEL: u8 = 1;
const NOT_WATCHED: u8 = 2;
const WATCHED_GUARDED: u8 = 3;

const DIFF_CHUNK: usize = 32;

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    let len = a.len().min(b.len());
    let (a_chunks, a_tail) = a[..len].as_chunks::<DIFF_CHUNK>();
    let (b_chunks, b_tail) = b[..len].as_chunks::<DIFF_CHUNK>();
    let (offset, a, b) = match a_chunks.iter().zip(b_chunks).position(|(x, y)| x != y) {
        Some(i) => (i * DIFF_CHUNK, &a_chunks[i][..], &b_chunks[i][..]),
        None => (len - a_tail.len(), a_tail, b_tail),
    };
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .map(|i| offset + i)
}

fn last_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    let len = a.len().min(b.len());
    let (a_chunks, a_tail) = a[..len].as_chunks::<DIFF_CHUNK>();
    let (b_chunks, b_tail) = b[..len].as_chunks::<DIFF_CHUNK>();
    if let Some(i) = a_tail.iter().zip(b_tail).rposition(|(x, y)| x != y) {
        return Some(len - a_tail.len() + i);
    }
    let chunk = a_chunks.iter().zip(b_chunks).rposition(|(x, y)| x != y)?;
    a_chunks[chunk]
        .iter()
        .zip(&b_chunks[chunk])
        .rposition(|(x, y)| x != y)
        .map(|i| chunk * DIFF_CHUNK + i)
}

#[cfg(test)]
mod difference_tests {
    use super::{first_difference, last_difference};

    #[test]
    fn differences_are_found_as_byte_by_byte() {
        let base: Vec<u8> = (0..98u32).map(|i| (i * 7 + 3) as u8).collect();
        for len in [0, 1, 31, 32, 33, 63, 64, 65, 97] {
            let a = &base[1..1 + len];
            let same = a.to_vec();
            assert_eq!(first_difference(&same, a), None);
            assert_eq!(last_difference(&same, a), None);
            for at in 0..len {
                for other in at..len {
                    let mut b = a.to_vec();
                    b[at] ^= 0x80;
                    b[other] ^= 0x01;
                    let first = a.iter().zip(&b).position(|(x, y)| x != y);
                    let last = a.iter().zip(&b).rposition(|(x, y)| x != y);
                    assert_eq!(first_difference(a, &b), first, "{len} {at} {other}");
                    assert_eq!(last_difference(a, &b), last, "{len} {at} {other}");
                }
            }
        }
    }

    #[test]
    fn differences_ignore_bytes_past_the_shorter_slice() {
        let a = [0; 65];
        let mut b = [0; 67];
        b[65] = 1;
        assert_eq!(first_difference(&a, &b), None);
        assert_eq!(last_difference(&b, &a), None);
        b[32] = 1;
        b[64] = 1;
        assert_eq!(first_difference(&b, &a), Some(32));
        assert_eq!(last_difference(&a, &b), Some(64));
    }
}

const PIECE: usize = 64 << 10;

unsafe fn copy_wide(src: *const u8, dst: *mut u8, len: usize) {
    const PER_THREAD: usize = 16 << 20;
    let threads = (len / PER_THREAD).clamp(1, 6);
    if threads == 1 {
        // SAFETY: the caller's.
        unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
        return;
    }
    let (src, dst, each) = (src as usize, dst as usize, len.div_ceil(threads));
    std::thread::scope(|s| {
        for i in 0..threads {
            let at = (i * each).min(len);
            let n = each.min(len - at);
            s.spawn(move || {
                // SAFETY: a part of the caller's ranges.
                unsafe {
                    std::ptr::copy_nonoverlapping((src + at) as *const u8, (dst + at) as *mut u8, n)
                };
            });
        }
    });
}

pub(crate) fn wide_back() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WIDE_BACK").is_none_or(|v| v != "0"))
}

type Job = Box<dyn FnOnce() + Send>;

struct Workers {
    jobs: Mutex<std::collections::VecDeque<Job>>,
    more: std::sync::Condvar,
}

fn workers() -> &'static Workers {
    static WORKERS: std::sync::OnceLock<Workers> = std::sync::OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let workers = WORKERS.get_or_init(|| Workers {
        jobs: Mutex::new(std::collections::VecDeque::new()),
        more: std::sync::Condvar::new(),
    });
    STARTED.call_once(|| {
        let n = std::thread::available_parallelism()
            .map_or(4, |n| n.get() / 2)
            .clamp(2, 6);
        for i in 0..n {
            let _ = std::thread::Builder::new()
                .name(format!("oathkeep-wide-{i}"))
                .spawn(move || {
                    let workers = WORKERS.get().expect("made above");
                    loop {
                        let job = {
                            let mut jobs = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if let Some(job) = jobs.pop_front() {
                                    break job;
                                }
                                jobs = workers.more.wait(jobs).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        job();
                    }
                });
        }
    });
    workers
}

fn wide(jobs: Vec<Job>) {
    if jobs.len() < 2 {
        for job in jobs {
            job();
        }
        return;
    }
    let workers = workers();
    let left = Arc::new((Mutex::new(jobs.len()), std::sync::Condvar::new()));
    {
        let mut queue = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs {
            let left = left.clone();
            queue.push_back(Box::new(move || {
                job();
                let mut n = left.0.lock().unwrap_or_else(|e| e.into_inner());
                *n -= 1;
                if *n == 0 {
                    left.1.notify_all();
                }
            }));
        }
    }
    workers.more.notify_all();
    loop {
        let job = workers
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        match job {
            Some(job) => job(),
            None => break,
        }
    }
    let mut n = left.0.lock().unwrap_or_else(|e| e.into_inner());
    while *n > 0 {
        n = left.1.wait(n).unwrap_or_else(|e| e.into_inner());
    }
}

pub(crate) fn wide_run(jobs: Vec<Box<dyn FnOnce() + Send>>) {
    wide(jobs);
}

const WIDE_JOB: usize = 1 << 20;

static SNAPS: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());

pub(crate) fn take_snapshots() {
    let snaps = std::mem::take(&mut *SNAPS.lock().unwrap_or_else(|e| e.into_inner()));
    if snaps.is_empty() {
        return;
    }
    wide(copy_jobs(snaps));
}

fn copy_jobs(copies: Vec<(usize, usize, usize)>) -> Vec<Job> {
    let mut jobs: Vec<Job> = Vec::new();
    let mut group: Vec<(usize, usize, usize)> = Vec::new();
    let mut bytes = 0;
    let flush = |group: &mut Vec<(usize, usize, usize)>, jobs: &mut Vec<Job>| {
        let group = std::mem::take(group);
        if !group.is_empty() {
            jobs.push(Box::new(move || {
                for (src, dst, len) in group {
                    // SAFETY: the caller's ranges: a host copy's mapping and
                    // its snapshot, which do not overlap.
                    unsafe { std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, len) };
                }
            }));
        }
    };
    for (src, dst, len) in copies {
        let mut at = 0;
        while at < len {
            let n = (WIDE_JOB - bytes).min(len - at);
            group.push((src + at, dst + at, n));
            at += n;
            bytes += n;
            if bytes >= WIDE_JOB {
                flush(&mut group, &mut jobs);
                bytes = 0;
            }
        }
    }
    flush(&mut group, &mut jobs);
    jobs
}

pub(crate) fn find_all(work: &[(Arc<Shadow>, usize, usize)]) -> Vec<Vec<(usize, usize)>> {
    let scans = work
        .iter()
        .map(|(shadow, address, end)| {
            let (from, to) = shadow.inside(*address, end.saturating_sub(*address));
            match shadow
                .before
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                Some(before) => (
                    shadow.data as usize,
                    before.as_ptr() as usize,
                    shadow.start as usize,
                    from,
                    to,
                ),
                None => (0, 0, 0, 0, 0),
            }
        })
        .collect();
    find_pieces(scans)
}

fn find_pieces(scans: Vec<(usize, usize, usize, usize, usize)>) -> Vec<Vec<(usize, usize)>> {
    type Found = Vec<(usize, usize, Vec<(usize, usize)>)>;
    type Part = (usize, usize, usize, usize, usize, usize, usize);
    let found: Arc<Mutex<Found>> = Arc::default();
    let mut jobs: Vec<Job> = Vec::new();
    let mut group: Vec<Part> = Vec::new();
    let mut bytes = 0;
    let flush = |group: &mut Vec<Part>, jobs: &mut Vec<Job>| {
        let group = std::mem::take(group);
        if group.is_empty() {
            return;
        }
        let found = found.clone();
        jobs.push(Box::new(move || {
            let mut mine: Found = Vec::with_capacity(group.len());
            for (data, before, start, from, to, which, place) in group {
                let mut pieces = Vec::new();
                let mut at = from;
                while at < to {
                    let n = (PIECE - (start + at) % PIECE).min(to - at);
                    // SAFETY: inside the copy's mapping and its snapshot
                    // (which holds the copy's bytes at the copy's offsets);
                    // nothing writes either while a submission's results
                    // are compared.
                    let (now, old) = unsafe {
                        (
                            std::slice::from_raw_parts((data + at) as *const u8, n),
                            std::slice::from_raw_parts((before + at) as *const u8, n),
                        )
                    };
                    if now != old {
                        pieces.push((at, n));
                    }
                    at += n;
                }
                mine.push((which, place, pieces));
            }
            found.lock().unwrap_or_else(|e| e.into_inner()).extend(mine);
        }));
    };
    let count = scans.len();
    for (which, (data, before, start, from, to)) in scans.into_iter().enumerate() {
        let (mut at, mut place) = (from, 0);
        while at < to {
            let room = (WIDE_JOB - bytes).max(PIECE);
            let mut next = (at + room).min(to);
            if next < to {
                next -= (start + next) % PIECE;
                if next <= at {
                    next = (at + PIECE - (start + at) % PIECE).min(to);
                }
            }
            group.push((data, before, start, at, next, which, place));
            bytes += next - at;
            place += 1;
            at = next;
            if bytes >= WIDE_JOB {
                flush(&mut group, &mut jobs);
                bytes = 0;
            }
        }
    }
    flush(&mut group, &mut jobs);
    wide(jobs);
    let mut found = std::mem::take(&mut *found.lock().unwrap_or_else(|e| e.into_inner()));
    found.sort_unstable_by_key(|f| (f.0, f.1));
    let mut out = vec![Vec::new(); count];
    for (which, _, pieces) in found {
        out[which].extend(pieces);
    }
    out
}

static BACK_CLASS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(crate) fn back_line(frames: f64) {
    let c: Vec<f64> = BACK_CLASS
        .iter()
        .map(|c| c.swap(0, Ordering::Relaxed) as f64 / frames)
        .collect();
    if c.iter().all(|&v| v == 0.0) {
        return;
    }
    let mb = |v: f64| v / (1 << 20) as f64;
    tracing::info!(
        "shader writes looked for on the CPU per frame: {:.1} MB compared in {:.0} ranges of windows outside buffers ({:.1} MB \
         snapshotted), {:.1} MB in {:.0} ranges of title buffers' copies ({:.1} MB snapshotted), of them {:.1} MB in copies \
         of 1 MiB or more (OATHKEEP_CARD_BUFFERS_MIN_MB=1 would have the card look there)",
        mb(c[2]),
        c[5],
        mb(c[0]),
        mb(c[3]),
        c[6],
        mb(c[1]),
        mb(c[4]),
    );
}

#[cfg(test)]
mod wide_tests {
    use super::{Job, PIECE, WIDE_JOB, copy_jobs, find_pieces, wide};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn all_jobs_run_before_wide_returns() {
        for n in [0usize, 1, 2, 7, 300] {
            let ran = Arc::new(AtomicUsize::new(0));
            let jobs: Vec<Job> = (0..n)
                .map(|i| {
                    let ran = ran.clone();
                    Box::new(move || {
                        ran.fetch_add(i + 1, Ordering::Relaxed);
                    }) as Job
                })
                .collect();
            wide(jobs);
            assert_eq!(ran.load(Ordering::Relaxed), n * (n + 1) / 2, "{n} jobs");
        }
    }

    #[test]
    fn copies_cut_into_jobs_copy_the_same_bytes() {
        let len = 3 * WIDE_JOB + 12345;
        let src: Vec<u8> = (0..len).map(|i| (i * 31 + i / 977) as u8).collect();
        let mut dst = vec![0u8; len];
        let (s, d) = (src.as_ptr() as usize, dst.as_mut_ptr() as usize);
        let cuts = [
            0,
            1,
            1,
            4096,
            70_000,
            WIDE_JOB + 3,
            2 * WIDE_JOB + 3,
            2 * WIDE_JOB + 9,
            len,
        ];
        let copies: Vec<(usize, usize, usize)> = cuts
            .windows(2)
            .map(|w| (s + w[0], d + w[0], w[1] - w[0]))
            .collect();
        let jobs = copy_jobs(copies);
        assert!(jobs.len() >= 3);
        wide(jobs);
        assert!(src == dst);
    }

    #[test]
    fn pieces_found_wide_are_those_found_one_by_one() {
        let len = 5 * WIDE_JOB / 2 + 777;
        let old: Vec<u8> = (0..len).map(|i| (i * 7 + i / 513) as u8).collect();
        let mut now = old.clone();
        for at in [
            0usize,
            1,
            PIECE - 1,
            PIECE,
            3 * PIECE + 5,
            WIDE_JOB - 1,
            WIDE_JOB,
            WIDE_JOB + PIECE / 2,
            2 * WIDE_JOB + 9,
            len - 1,
        ] {
            now[at] ^= 0x5a;
        }
        for byte in &mut now[9 * PIECE - 40..9 * PIECE + 40] {
            *byte ^= 0x01;
        }
        let (data, before) = (now.as_ptr() as usize, old.as_ptr() as usize);
        let mut scans = Vec::new();
        let mut wanted = Vec::new();
        for start in [0usize, 4096, PIECE - 16, 0xd_b790_df00] {
            for (from, to) in [
                (0, len),
                (0, 1),
                (1, 2),
                (PIECE - 1, PIECE + 1),
                (3 * PIECE, 3 * PIECE),
                (12345, len - 1),
                (WIDE_JOB - 3, 2 * WIDE_JOB + 10),
            ] {
                scans.push((data, before, start, from, to));
                let mut pieces = Vec::new();
                let mut at = from;
                while at < to {
                    let n = (PIECE - (start + at) % PIECE).min(to - at);
                    if now[at..at + n] != old[at..at + n] {
                        pieces.push((at, n));
                    }
                    at += n;
                }
                wanted.push(pieces);
            }
        }
        scans.push((0, 0, 0, 0, 0));
        wanted.push(Vec::new());
        assert_eq!(find_pieces(scans), wanted);
    }
}

fn back_piece(
    host: &mut [u8],
    old: &[u8],
    title: &mut Vec<u8>,
    mask: &mut Vec<u8>,
    runs: &mut Vec<(usize, usize)>,
    read: impl FnOnce(usize, &mut [u8]) -> bool,
) -> Option<(usize, usize)> {
    runs.clear();
    let first = first_difference(host, old)?;
    let last = last_difference(host, old).unwrap_or(first);
    let n = last + 1 - first;
    let (host, old) = (&mut host[first..=last], &old[first..=last]);
    title.clear();
    title.resize(n, 0);
    if !read(first, title) {
        return None;
    }
    mask.clear();
    mask.resize(n, 0);
    let mut differs = 0u8;
    for (((t, h), o), m) in title
        .iter_mut()
        .zip(host.iter_mut())
        .zip(old)
        .zip(mask.iter_mut())
    {
        let gpu = if (*h != *o) & (*t == *o) { 0xffu8 } else { 0 };
        let merged = (*t & !gpu) | (*h & gpu);
        *m = merged ^ *t;
        differs |= *m;
        *t = merged;
        *h = merged;
    }
    if differs == 0 {
        return None;
    }
    let mut i = 0;
    while i < n {
        while i < n && mask[i] == 0 {
            i += 1;
        }
        let start = i;
        while i < n && mask[i] != 0 {
            i += 1;
        }
        if i > start {
            runs.push((start, i - start));
        }
    }
    Some((first, n))
}

static MERGED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(crate) fn merged_stats() -> [u64; 3] {
    [0, 1, 2].map(|i| MERGED[i].swap(0, Ordering::Relaxed))
}

fn changed_runs(now: &[u8], was: &[u8]) -> Vec<(usize, usize)> {
    let n = now.len().min(was.len());
    let mut runs = Vec::new();
    let mut i = 0;
    while i < n {
        while i + 8 <= n && now[i..i + 8] == was[i..i + 8] {
            i += 8;
        }
        while i < n && now[i] == was[i] {
            i += 1;
        }
        if i >= n {
            break;
        }
        let first = i;
        while i < n && now[i] != was[i] {
            i += 1;
        }
        runs.push((first, i - first));
    }
    runs
}

#[cfg(test)]
mod merge_tests {
    use super::{PIECE, back_piece, changed_runs};

    #[test]
    fn a_piece_put_back_reads_only_its_stretch() {
        let old: Vec<u8> = (0..PIECE).map(|i| (i * 7 + i / 251) as u8).collect();
        let mut title_memory = old.clone();
        title_memory[7] ^= 0x11;
        title_memory[40_000] ^= 0x22;
        let mut host = old.clone();
        for at in [5, 9] {
            host[at] ^= 0x33;
        }
        let gpu = host.clone();
        let mut scratch = Vec::new();
        let (mut mask, mut runs) = (Vec::new(), Vec::new());
        let mut asked = None;
        let read = |first: usize, into: &mut [u8]| {
            asked = Some((first, into.len()));
            into.copy_from_slice(&title_memory[first..first + into.len()]);
            true
        };
        assert_eq!(
            back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read),
            Some((5, 5))
        );
        assert_eq!(asked, Some((5, 5)));
        let mut wanted = title_memory[5..10].to_vec();
        wanted[0] = gpu[5];
        wanted[4] = gpu[9];
        assert_eq!(scratch, wanted);
        assert_eq!(host[5..10], wanted);
        assert_eq!(runs, [(0, 1), (4, 1)]);
        assert!(host[..5] == gpu[..5] && host[10..] == gpu[10..]);
        assert!(
            scratch.capacity() < PIECE / 2,
            "scratch of {} bytes for a stretch of 5",
            scratch.capacity()
        );

        let mut same = old.clone();
        assert_eq!(
            back_piece(
                &mut same,
                &old,
                &mut scratch,
                &mut mask,
                &mut runs,
                |_, _| unreachable!()
            ),
            None
        );
        let mut host = gpu.clone();
        assert_eq!(
            back_piece(
                &mut host,
                &old,
                &mut scratch,
                &mut mask,
                &mut runs,
                |_, _| false
            ),
            None
        );
        assert!(host == gpu);
        let mut title_memory = old.clone();
        title_memory[5] = gpu[5];
        title_memory[9] = gpu[9];
        let mut host = gpu.clone();
        let read = |first: usize, into: &mut [u8]| {
            into.copy_from_slice(&title_memory[first..first + into.len()]);
            true
        };
        assert_eq!(
            back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read),
            None
        );
        let mut host: Vec<u8> = old.iter().map(|b| !b).collect();
        assert_eq!(
            back_piece(
                &mut host,
                &old,
                &mut scratch,
                &mut mask,
                &mut runs,
                |_, into| into.len() == PIECE
            ),
            Some((0, PIECE))
        );
        assert_eq!(scratch.len(), PIECE);
    }

    #[test]
    fn a_byte_both_changed_keeps_the_titles_and_only_gpu_bytes_are_written() {
        let old = vec![1u8, 2, 3, 4, 5, 6];
        let mut host = old.clone();
        host[1] = 20;
        host[3] = 40;
        host[4] = 50;
        let mut title_memory = old.clone();
        title_memory[3] = 44;
        title_memory[5] = 66;
        let (mut scratch, mut mask, mut runs) = (Vec::new(), Vec::new(), Vec::new());
        let read = |first: usize, into: &mut [u8]| {
            into.copy_from_slice(&title_memory[first..first + into.len()]);
            true
        };
        assert_eq!(
            back_piece(&mut host, &old, &mut scratch, &mut mask, &mut runs, read),
            Some((1, 4))
        );
        assert_eq!(
            scratch,
            [20, 3, 44, 50],
            "the title's byte where both changed"
        );
        assert_eq!(host[1..5], [20, 3, 44, 50], "the copy takes the result");
        assert_eq!(runs, [(0, 1), (3, 1)], "the GPU's bytes only");
    }

    #[test]
    fn changed_runs_are_exactly_the_bytes_that_differ() {
        let was: Vec<u8> = (0..300u32).map(|i| (i * 13 + 5) as u8).collect();
        for changes in [
            vec![],
            vec![0],
            vec![299],
            vec![7, 8, 9],
            vec![7, 9],
            vec![
                15, 16, 17, 23, 24, 64, 65, 66, 67, 68, 69, 70, 71, 72, 298, 299,
            ],
        ] {
            let mut now = was.clone();
            for &at in &changes {
                now[at] ^= 0x41;
            }
            let runs = changed_runs(&now, &was);
            let mut put = was.clone();
            let mut held = Vec::new();
            for &(at, n) in &runs {
                assert!(n > 0);
                put[at..at + n].copy_from_slice(&now[at..at + n]);
                held.extend(at..at + n);
            }
            assert_eq!(put, now, "{changes:?}");
            assert_eq!(held, changes, "{changes:?}");
            assert!(
                runs.windows(2).all(|w| w[0].0 + w[0].1 < w[1].0),
                "{changes:?}"
            );
        }
        assert!(changed_runs(&[], &[]).is_empty());
    }
}

static LIVE: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Shadow {
    fn drop(&mut self) {
        if self.place.is_some() {
            crate::inplace::gone(self.window, self.len);
            return;
        }
        LIVE[0].fetch_sub(1, Ordering::Relaxed);
        LIVE[1].fetch_sub(self.len, Ordering::Relaxed);
        if self.kept.get_mut().unwrap_or_else(|e| e.into_inner()).1 {
            KEPT_BYTES.fetch_sub(self.len, Ordering::Relaxed);
        }
        let window = self.window;
        if let Some(card) = &mut self.card {
            if window {
                WINDOW_CARD_BYTES.fetch_sub(card.size, Ordering::Relaxed);
            } else {
                CARD_BYTES[0].fetch_sub(card.size, Ordering::Relaxed);
            }
            if card
                .before
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
            {
                CARD_BYTES[1].fetch_sub(card_hash_bytes(card.size), Ordering::Relaxed);
            }
        }
    }
}

fn keeps() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_KEEP_SNAPSHOTS").is_some_and(|v| v == "1"))
}

static KEPT_BYTES: AtomicU64 = AtomicU64::new(0);
fn kept_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_KEEP_SNAPSHOTS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(256)
            << 20
    })
}

fn pieces_of(start: usize, from: usize, to: usize) -> std::ops::Range<usize> {
    let first = start / PIECE;
    (start + from) / PIECE - first..(start + to - 1) / PIECE - first + 1
}

fn piece_at(start: usize, len: usize, piece: usize) -> (usize, usize) {
    let first = start / PIECE;
    let lo = ((first + piece) * PIECE).saturating_sub(start);
    let hi = ((first + piece + 1) * PIECE - start).min(len);
    (lo, hi)
}

#[cfg(test)]
mod piece_tests {
    use super::{PIECE, piece_at, pieces_of};

    #[test]
    fn pieces_cover_the_copy_as_the_compare_cuts_it() {
        for start in [
            0usize,
            1,
            4096,
            PIECE - 1,
            PIECE,
            PIECE + 77,
            5 * PIECE + 12345,
        ] {
            for len in [1usize, 100, PIECE - 1, PIECE, PIECE + 1, 3 * PIECE + 500] {
                let count = pieces_of(start, 0, len).end;
                let mut at = 0;
                for piece in 0..count {
                    let (lo, hi) = piece_at(start, len, piece);
                    assert_eq!(lo, at, "{start} {len} {piece}");
                    let n = (PIECE - (start + at) % PIECE).min(len - at);
                    assert_eq!(hi, at + n, "{start} {len} {piece}");
                    assert_eq!(pieces_of(start, lo, hi), piece..piece + 1);
                    assert_eq!(pieces_of(start, hi - 1, hi), piece..piece + 1);
                    at = hi;
                }
                assert_eq!(at, len, "{start} {len}");
                assert_eq!(pieces_of(start, 0, len), 0..count);
            }
        }
    }
}

fn stale_pages_in(
    base: usize,
    copied: &[u64],
    a: usize,
    e: usize,
    written: &[(usize, u64)],
) -> Vec<usize> {
    (a / PAGE..e.div_ceil(PAGE))
        .filter(|&page| page >= base)
        .filter(|&page| match copied.get(page - base) {
            None => false,
            Some(0) => true,
            Some(&at) => written
                .binary_search_by_key(&page, |&(p, _)| p)
                .is_ok_and(|i| written[i].1 > at),
        })
        .collect()
}

#[cfg(test)]
mod stale_tests {
    use super::*;

    #[test]
    fn pages_never_taken_are_stale_written_or_not() {
        let base = 0x100;
        let copied = [5, 0, 5, 5];
        let at = |page: usize| page * PAGE;
        let written = [(0x102, 6), (0x103, 4)];
        assert_eq!(
            stale_pages_in(base, &copied, at(0x100), at(0x104), &written),
            [0x101, 0x102]
        );
        assert_eq!(
            stale_pages_in(base, &copied, at(0x101) + 8, at(0x101) + 16, &[]),
            [0x101]
        );
        assert!(stale_pages_in(base, &copied, at(0x104), at(0x106), &[(0x104, 9)]).is_empty());
    }
}

static LET_GO: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

#[cfg(feature = "diagnostics")]
pub(crate) fn windows() -> [u64; 3] {
    let asked: u64 = {
        let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        loose
            .values()
            .map(|s| {
                let low = s.asked[0].load(Ordering::Relaxed);
                let high = s.asked[1].load(Ordering::Relaxed);
                high.saturating_sub(low)
            })
            .sum()
    };
    [
        LET_GO[0].load(Ordering::Relaxed),
        LET_GO[1].load(Ordering::Relaxed),
        asked,
    ]
}

#[cfg(feature = "diagnostics")]
pub(crate) fn window_use() -> [u64; 8] {
    let mut out = [0u64; 8];
    let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
    for s in loose.values() {
        let copied = s.copied.lock().unwrap_or_else(|e| e.into_inner());
        let pages = copied.iter().filter(|&&at| at != 0).count() as u64;
        out[0] += (pages * PAGE as u64).min(s.len);
        if crate::host::references(&s.buffer) > 1 {
            out[1] += 1;
            out[2] += s.len;
        }
        if Arc::strong_count(s) > 1 {
            out[3] += 1;
            out[4] += s.len;
        }
        if s.unsized_asks.load(Ordering::Relaxed) > 0 {
            out[5] += 1;
            out[6] += s.len;
        }
    }
    out[7] = window();
    out
}

pub(crate) fn let_go_idle() {
    const IDLE_FRAMES: u64 = 4;
    let gone: Vec<Arc<Shadow>> = {
        let mut map = LOOSE.write().unwrap_or_else(|e| e.into_inner());
        if map.is_empty() {
            return;
        }
        let idle_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
        let idle: Vec<(u64, u64)> = map
            .iter()
            .filter(|(_, s)| {
                Arc::strong_count(s) == 1
                    && crate::host::long_ago(s.used.load(Ordering::Relaxed), idle_ms, IDLE_FRAMES)
                    && crate::host::references(&s.buffer) == 1
            })
            .map(|(key, _)| *key)
            .collect();
        idle.into_iter()
            .filter_map(|key| map.remove(&key))
            .collect()
    };
    if !gone.is_empty() {
        let (went, bytes) = (gone.len(), gone.iter().map(|s| s.len).sum::<u64>());
        crate::host::trail(|| {
            format!(
                "sweep: {went} host copies of windows of title memory let go ({} KiB), held by no list and no slot",
                bytes >> 10
            )
        });
    }
    for shadow in gone {
        LET_GO[0].fetch_add(1, Ordering::Relaxed);
        LET_GO[1].fetch_add(shadow.len, Ordering::Relaxed);
        crate::host::retire(shadow);
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 6] {
    let (windows, window_bytes) = {
        let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        let mut seen = std::collections::HashSet::new();
        let bytes: u64 = loose
            .values()
            .filter(|s| seen.insert(Arc::as_ptr(s) as usize))
            .map(|s| s.len)
            .sum();
        (seen.len() as u64, bytes)
    };
    let (kept, kept_bytes) = {
        let pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        (
            pool.len() as u64,
            pool.iter().map(|v| v.capacity() as u64).sum::<u64>(),
        )
    };
    [
        LIVE[0].load(Ordering::Relaxed),
        LIVE[1].load(Ordering::Relaxed),
        windows,
        window_bytes,
        kept,
        kept_bytes,
    ]
}

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    pub(crate) fn span(&self) -> (u64, u64) {
        (self.start, self.len)
    }

    pub(crate) fn offset_in(&self, address: u64) -> Option<u64> {
        address.checked_sub(self.start).filter(|&at| at < self.len)
    }

    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    pub(crate) fn unwatched(&self) -> bool {
        self.watched.load(Ordering::Relaxed) == NOT_WATCHED
    }

    pub(crate) fn guarded(&self) -> bool {
        self.watched.load(Ordering::Relaxed) == WATCHED_GUARDED
    }

    pub(crate) fn ever_written(&self) -> bool {
        self.ever_written.load(Ordering::Relaxed)
    }

    pub(crate) fn pending_touches(&self, address: usize, len: usize) -> bool {
        let end = address.saturating_add(len);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|&(a, e)| a < end && e > address)
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start + self.place.as_ref().map_or(0, |place| place.1)
    }

    pub(crate) fn is_in_place(&self) -> bool {
        self.place.is_some()
    }

    fn placed(placed: crate::inplace::Placed, start: u64, len: u64, window: bool) -> Shadow {
        crate::host::name(&placed.buffer, || {
            format!("title memory {start:#x}+{len:#x} in place")
        });
        let mut shadow = Shadow::new(placed.buffer, start as *mut u8, placed.gpu, start, len);
        LIVE[0].fetch_sub(1, Ordering::Relaxed);
        LIVE[1].fetch_sub(len, Ordering::Relaxed);
        *shadow.copied.get_mut().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        *shadow.watched.get_mut() = NOT_WATCHED;
        shadow.window = window;
        shadow.place = Some((placed.heap, placed.bias));
        shadow
    }

    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        if self.place.is_some() {
            list.keep(&self.buffer);
            return;
        }
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Shadow {
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        LIVE[0].fetch_add(1, Ordering::Relaxed);
        LIVE[1].fetch_add(len, Ordering::Relaxed);
        Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            copied: Mutex::new(vec![0; pages as usize]),
            uncopied: AtomicU64::new(pages),
            synced_at: AtomicU64::new(0),
            ever_written: AtomicBool::new(false),
            watched: AtomicU8::new(0),
            pending: Mutex::default(),
            flights: Mutex::default(),
            before: Mutex::default(),
            kept: Mutex::default(),
            used: AtomicU64::new(crate::host::moment()),
            asked: [AtomicU64::new(u64::MAX), AtomicU64::new(0)],
            unsized_asks: AtomicU64::new(0),
            id: {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                NEXT.fetch_add(1, Ordering::Relaxed)
            },
            card: None,
            window: false,
            place: None,
        }
    }

    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        if self.place.is_some() {
            return;
        }
        self.ever_written.store(true, Ordering::Relaxed);
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    pub(crate) fn writing(&self, address: usize, len: usize) {
        if self.place.is_some() {
            return;
        }
        let range = (address, address + len);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if relay() {
            self.flights
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .declare(range, PREPARED.load(Ordering::Relaxed));
        }
        if pending.contains(&range) {
            return;
        }
        if self.card.is_some() {
            pending.push(range);
            return;
        }
        let mut parts = vec![self.inside(address, len)];
        for &(a, e) in pending.iter() {
            let (a, e) = self.inside(a, e - a);
            let mut rest = Vec::with_capacity(parts.len() + 1);
            for (from, to) in parts {
                if e <= from || a >= to {
                    rest.push((from, to));
                    continue;
                }
                if from < a {
                    rest.push((from, a));
                }
                if e < to {
                    rest.push((e, to));
                }
            }
            parts = rest;
        }
        pending.push(range);
        let taken: usize = parts
            .iter()
            .map(|&(from, to)| to.saturating_sub(from))
            .sum();
        BACK_CLASS[usize::from(!self.window)].fetch_add(taken as u64, Ordering::Relaxed);
        let total = self.len as usize;
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = before.get_or_insert_with(|| pooled(total));
        if keeps() {
            let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            if kept.0.is_empty() {
                kept.0 = vec![false; self.pieces(0, total.max(1)).end];
            }
            for (from, to) in parts {
                if from >= to {
                    continue;
                }
                for piece in self.pieces(from, to) {
                    if kept.0[piece] {
                        continue;
                    }
                    let (lo, hi) = self.piece(piece);
                    let whole = from <= lo && hi <= to;
                    let (lo, hi) = (lo.max(from), hi.min(to));
                    // SAFETY: as below; `hi <= total`.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            self.data.add(lo),
                            snapshot.as_mut_ptr().add(lo),
                            hi - lo,
                        );
                    }
                    kept.0[piece] = whole;
                }
            }
            return;
        }
        if wide_back() {
            let mut snaps = SNAPS.lock().unwrap_or_else(|e| e.into_inner());
            let (data, snapshot) = (self.data as usize, snapshot.as_mut_ptr() as usize);
            snaps.extend(
                parts
                    .into_iter()
                    .filter(|&(from, to)| from < to)
                    .map(|(from, to)| (data + from, snapshot + from, to - from)),
            );
            return;
        }
        for (from, to) in parts {
            if from < to {
                // SAFETY: the copy's mapping and the snapshot's allocation
                // (`pooled`) both hold `total` bytes, and `to <= total`.
                unsafe {
                    copy_wide(
                        self.data.add(from),
                        snapshot.as_mut_ptr().add(from),
                        to - from,
                    );
                }
            }
        }
    }

    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        self.copy_back_with(address, len, None)
    }

    pub(crate) fn copy_back_with(
        &self,
        address: usize,
        len: usize,
        found: Option<Vec<(usize, usize)>>,
    ) -> usize {
        if self.place.is_some() {
            return 0;
        }
        let snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        self.compared(from, to);
        if let Some(before) = &*snapshot
            && from < to
        {
            // SAFETY: `writing` set the snapshot's bytes of every declared
            // range, at the copy's offsets.
            let before =
                unsafe { std::slice::from_raw_parts(before.as_ptr().add(from), to - from) };
            #[cfg(feature = "diagnostics")]
            let scanning = std::time::Instant::now();
            let differing = match found {
                Some(found) => found,
                None => self.differing(from, to, before),
            };
            #[cfg(feature = "diagnostics")]
            let syncing = {
                use crate::copystat::{C, add};
                add(C::BackScanUs, scanning.elapsed().as_micros() as u64);
                add(C::BackDiffer, differing.len() as u64);
                std::time::Instant::now()
            };
            for &(at, n) in &differing {
                self.changed(at, at + n);
            }
            let (mut title, mut mask, mut runs) = (Vec::new(), Vec::new(), Vec::new());
            for (at, n) in differing {
                // SAFETY: inside the mapping; only the submitting thread
                // writes it.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &before[at - from..at - from + n];
                let read = |first: usize, title: &mut [u8]| {
                    crate::host::read_into((start + at + first) as *const u8, title)
                };
                let Some((first, n)) =
                    back_piece(host, old, &mut title, &mut mask, &mut runs, read)
                else {
                    continue;
                };
                let at = at + first;
                crate::watch::writing(start + at, n);
                for &(off, len) in &runs {
                    crate::host::write_mapped(
                        (start + at + off) as *mut u8,
                        title[off..].as_ptr(),
                        len,
                    );
                    written += len;
                }
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(
                crate::copystat::C::BackSyncUs,
                syncing.elapsed().as_micros() as u64,
            );
        }
        written
    }

    fn compared(&self, from: usize, to: usize) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackCompared, (to - from) as u64);
        let (bytes, ranges) = if self.window { (2, 5) } else { (3, 6) };
        BACK_CLASS[bytes].fetch_add((to - from) as u64, Ordering::Relaxed);
        BACK_CLASS[ranges].fetch_add(1, Ordering::Relaxed);
        if !self.window && self.len >= 1 << 20 {
            BACK_CLASS[4].fetch_add((to - from) as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn relay_compared(&self, address: usize, len: usize) {
        let (from, to) = self.inside(address, len);
        self.compared(from, to);
    }

    pub(crate) fn relay_merge(
        &self,
        at: usize,
        n: usize,
        now: &mut Vec<u8>,
        title: &mut Vec<u8>,
    ) -> usize {
        let start = self.start as usize;
        if self.card.is_some() || n == 0 || at + n > self.len as usize {
            return 0;
        }
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let Some(before) = snapshot.as_mut() else {
            return 0;
        };
        now.clear();
        now.reserve(n);
        // SAFETY: inside the copy's mapping (checked above), read into
        // room just reserved; the card may write the mapping meanwhile, and
        // each byte read is one it held at some moment.
        unsafe {
            std::ptr::copy_nonoverlapping(self.data.add(at), now.as_mut_ptr(), n);
            now.set_len(n);
        }
        // SAFETY: the snapshot's allocation holds the copy's length
        // (`writing`), and these offsets are in a range declared and not
        // done with, whose bytes it holds; nothing else touches them
        // without the lock held here.
        let held = unsafe { std::slice::from_raw_parts_mut(before.as_mut_ptr().add(at), n) };
        let Some(first) = first_difference(now, held) else {
            return 0;
        };
        let last = last_difference(now, held).unwrap_or(first);
        let (at, n) = (at + first, last + 1 - first);
        let (now, held) = (&now[first..=last], &mut held[first..=last]);
        title.clear();
        title.resize(n, 0);
        if !crate::host::read_into((start + at) as *const u8, title) {
            return 0;
        }
        let as_read = merge_race_on().then(|| title.clone());
        if !merge_piece(now, held, title) {
            return 0;
        }
        crate::watch::writing(start + at, n);
        merge_race(
            as_read.as_deref(),
            (self.start, self.len),
            start + at,
            &title[..],
            "pc relay",
        );
        crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
        n
    }

    fn differing(&self, from: usize, to: usize, before: &[u8]) -> Vec<(usize, usize)> {
        let (start, data) = (self.start as usize, self.data as usize);
        let scan = |a: usize, b: usize| -> Vec<(usize, usize)> {
            let mut found = Vec::new();
            let mut at = a;
            while at < b {
                let n = (PIECE - (start + at) % PIECE).min(b - at);
                // SAFETY: inside the mapping, which nothing writes while a
                // submission's results are compared.
                let host = unsafe { std::slice::from_raw_parts((data + at) as *const u8, n) };
                if host != &before[at - from..at - from + n] {
                    found.push((at, n));
                }
                at += n;
            }
            found
        };
        const PER_THREAD: usize = 16 << 20;
        let threads = ((to - from) / PER_THREAD).clamp(1, 8);
        if threads == 1 {
            return scan(from, to);
        }
        let cut = |i: usize| {
            if i == threads {
                return to;
            }
            let at = from + (to - from) / threads * i;
            (at - (start + at) % PIECE).max(from)
        };
        let scan = &scan;
        std::thread::scope(|s| {
            let parts: Vec<_> = (0..threads)
                .map(|i| {
                    let (a, b) = (cut(i), cut(i + 1));
                    s.spawn(move || scan(a, b))
                })
                .collect();
            parts
                .into_iter()
                .flat_map(|part| match part.join() {
                    Ok(found) => found,
                    Err(panic) => std::panic::resume_unwind(panic),
                })
                .collect()
        })
    }

    pub(crate) fn copy_back_done(&self, ranges: &[(usize, usize)]) {
        if self.place.is_some() {
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if relay() {
            let free = self
                .flights
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .done(ranges);
            pending.retain(|r| !free.contains(r));
        } else {
            pending.retain(|r| !ranges.contains(r));
        }
        if !pending.is_empty() || self.card.is_some() {
            return;
        }
        if keeps() {
            let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            if !kept.1 && self.len >= 1 << 20 {
                if KEPT_BYTES.fetch_add(self.len, Ordering::Relaxed) + self.len <= kept_most() {
                    kept.1 = true;
                } else {
                    KEPT_BYTES.fetch_sub(self.len, Ordering::Relaxed);
                }
            }
            if kept.1 {
                return;
            }
            kept.0.clear();
        }
        if let Some(bytes) = self.before.lock().unwrap_or_else(|e| e.into_inner()).take() {
            give_back(bytes);
        }
    }

    fn pieces(&self, from: usize, to: usize) -> std::ops::Range<usize> {
        pieces_of(self.start as usize, from, to)
    }

    fn piece(&self, piece: usize) -> (usize, usize) {
        piece_at(self.start as usize, self.len as usize, piece)
    }

    fn changed(&self, from: usize, to: usize) {
        if !keeps() || from >= to {
            return;
        }
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if kept.0.is_empty() {
            return;
        }
        for piece in self.pieces(from, to) {
            kept.0[piece] = false;
        }
    }

    pub(crate) fn writes_pending(&self) -> bool {
        !self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    pub(crate) fn stale_under_writes(&self, address: usize, len: usize) -> bool {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return false;
        }
        let busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|&&(a, e)| a < to && e > from)
            .map(|&(a, e)| (a.max(from), e.min(to)))
            .collect();
        if busy.is_empty() {
            return false;
        }
        if self.watched.load(Ordering::Relaxed) == NOT_WATCHED {
            return true;
        }
        let base = start / PAGE;
        let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        busy.into_iter()
            .any(|(a, e)| match crate::watch::asked(a, e) {
                Some((_, written)) => !stale_pages_in(base, &copied, a, e, &written).is_empty(),
                None => true,
            })
    }

    pub(crate) fn merge_under_writes(
        &self,
        address: usize,
        len: usize,
        runs_left: &mut usize,
    ) -> bool {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return false;
        }
        let busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|&&(a, e)| a < to && e > from)
            .map(|&(a, e)| (a.max(from), e.min(to)))
            .collect();
        if busy.is_empty() {
            return false;
        }
        let mode = self.watched.load(Ordering::Relaxed);
        if mode == NOT_WATCHED {
            return true;
        }
        let base = start / PAGE;
        let mut title = vec![0u8; PAGE];
        for (a, e) in busy {
            let Some((now, written)) = (match mode {
                WATCHED_GUARDED => crate::watch::writes_guarded(a, e),
                WATCHED_KERNEL => crate::watch::writes(a, e, true),
                _ => match crate::watch::writes_kind(a, e, true) {
                    Some((now, written, kind)) => {
                        self.watched.store(
                            match kind {
                                crate::watch::WatchKind::WriteWatch => WATCHED_KERNEL,
                                crate::watch::WatchKind::Guarded => WATCHED_GUARDED,
                            },
                            Ordering::Relaxed,
                        );
                        Some((now, written))
                    }
                    None => None,
                },
            }) else {
                return true;
            };
            let stale: Vec<usize> = {
                let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                stale_pages_in(base, &copied, a, e, &written)
            };
            for page in stale {
                let (lo, hi) = ((page * PAGE).max(a), ((page + 1) * PAGE).min(e));
                if lo >= hi {
                    continue;
                }
                let n = hi - lo;
                let title = &mut title[..n];
                let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
                if !crate::host::read_into(lo as *const u8, title) {
                    continue;
                }
                MERGED[0].fetch_add(1, Ordering::Relaxed);
                let at = lo - start;
                if self.card.is_some() {
                    // SAFETY: inside the mapping of the copy's CPU side,
                    // which only the submitting thread writes.
                    let held = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                    let runs = changed_runs(title, held);
                    if runs.len() > *runs_left {
                        return true;
                    }
                    *runs_left -= runs.len();
                    for (r, m) in runs {
                        held[r..r + m].copy_from_slice(&title[r..r + m]);
                        self.card_changed(at + r, at + r + m);
                        MERGED[1].fetch_add(1, Ordering::Relaxed);
                        MERGED[2].fetch_add(m as u64, Ordering::Relaxed);
                    }
                } else {
                    let Some(snapshot) = before.as_mut() else {
                        return true;
                    };
                    // SAFETY: the snapshot's allocation holds the copy's
                    // bytes at the copy's offsets (`writing`), and these
                    // are in a declared range.
                    let held =
                        unsafe { std::slice::from_raw_parts_mut(snapshot.as_mut_ptr().add(at), n) };
                    for (r, m) in changed_runs(title, held) {
                        // SAFETY: inside the copy's mapping. The card may be
                        // writing the copy beside these bytes; they are
                        // written here and nothing around them.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                title.as_ptr().add(r),
                                self.data.add(at + r),
                                m,
                            )
                        };
                        held[r..r + m].copy_from_slice(&title[r..r + m]);
                        MERGED[1].fetch_add(1, Ordering::Relaxed);
                        MERGED[2].fetch_add(m as u64, Ordering::Relaxed);
                    }
                }
                drop(before);
                if (page * PAGE).max(start) >= a && ((page + 1) * PAGE).min(end) <= e {
                    let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(at) = copied.get_mut(page - base) {
                        if *at == 0 && now != 0 {
                            self.uncopied.fetch_sub(1, Ordering::Release);
                        }
                        *at = now;
                    }
                }
            }
        }
        false
    }

    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        if self.place.is_some() {
            return 0;
        }
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        if !busy.is_empty() {
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                if a > at {
                    bytes += self.update_free(at, a);
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_free(at, to);
            }
            return bytes;
        }
        self.update_free(from, to)
    }

    fn update_free(&self, from: usize, to: usize) -> usize {
        let mode = self.watched.load(Ordering::Relaxed);
        if mode != NOT_WATCHED {
            let since = if self.uncopied.load(Ordering::Acquire) == 0 {
                self.synced_at.load(Ordering::Acquire)
            } else {
                0
            };
            let found = match mode {
                WATCHED_GUARDED => crate::watch::writes_guarded_since(from, to, since),
                WATCHED_KERNEL => crate::watch::writes(from, to, true),
                _ => match crate::watch::writes_kind(from, to, true) {
                    Some((now, written, kind)) => {
                        self.watched.store(
                            match kind {
                                crate::watch::WatchKind::WriteWatch => WATCHED_KERNEL,
                                crate::watch::WatchKind::Guarded => WATCHED_GUARDED,
                            },
                            Ordering::Relaxed,
                        );
                        Some((now, written))
                    }
                    None => None,
                },
            };
            if let Some((now, written)) = found {
                let copied = self.copy_stale(from, to, now, &written);
                self.refresh_check(from, to);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::WholeBytes, (to - from) as u64);
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self.copy(from, to);
        #[cfg(feature = "diagnostics")]
        crate::copystat::whole(from, to - from, started.elapsed().as_micros() as u64);
        to - from
    }

    fn refresh_check(&self, from: usize, to: usize) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_REFRESH_CHECK").is_some_and(|v| v == "1"))
        {
            return;
        }
        const SAMPLE: usize = 256;
        static DIFFERING: Mutex<Option<std::collections::HashMap<usize, u32>>> = Mutex::new(None);
        static SAID: AtomicU64 = AtomicU64::new(0);
        let start = self.start as usize;
        let mut title = [0u8; SAMPLE];
        let mut at = from;
        while at < to {
            let n = SAMPLE.min(to - at);
            if crate::host::read_into(at as *const u8, &mut title[..n]) {
                // SAFETY: `[from, to)` lies inside the buffer (`update`),
                // so these bytes are inside the copy's mapping.
                let copy = unsafe {
                    std::slice::from_raw_parts(self.data.add(at - start).cast_const(), n)
                };
                let mut map = DIFFERING.lock().unwrap_or_else(|e| e.into_inner());
                let map = map.get_or_insert_with(Default::default);
                if copy == &title[..n] {
                    map.remove(&at);
                } else {
                    let times = map.entry(at).or_insert(0);
                    *times += 1;
                    if *times == 3 {
                        let said = SAID.fetch_add(1, Ordering::Relaxed) + 1;
                        if said <= 64 || said.is_power_of_two() {
                            tracing::warn!(
                                said,
                                copy = self.id,
                                copy_start = format_args!("{start:#x}"),
                                copy_bytes = self.len,
                                address = format_args!("{at:#x}"),
                                refreshed = format_args!("{from:#x}..{to:#x}"),
                                on_the_card = self.card.is_some(),
                                window = self.window,
                                "a host copy of title memory is stale after its refresh, three times in a row:                                  the memory changed with nothing to tell of it (OATHKEEP_REFRESH_CHECK=1)"
                            );
                        }
                    }
                }
            }
            at = (at / PAGE + 1) * PAGE;
        }
    }

    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let base = self.start as usize / PAGE;
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let full = from <= start && to >= end;
        if self.uncopied.load(Ordering::Acquire) == 0 {
            if written.is_empty() {
                if full && now != 0 {
                    self.synced_at.fetch_max(now, Ordering::Release);
                }
                return 0;
            }
            let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            let mut bytes = 0;
            let mut run: Option<(usize, usize)> = None;
            for &(page, write) in written {
                if page < base || page - base >= copied.len() {
                    continue;
                }
                let at = &mut copied[page - base];
                if write == crate::watch::HOT_WRITE && *at != 0 {
                    if let Some((first, prev)) = run.take() {
                        bytes += self
                            .copy((first * PAGE).max(from), ((prev + 1) * PAGE).min(to));
                    }
                    if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                        *at = now;
                    }
                    bytes += self.copy_hot((page * PAGE).max(from), ((page + 1) * PAGE).min(to));
                } else if write > *at {
                    if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                        *at = now;
                    }
                    match run {
                        Some((first, prev)) if page == prev + 1 => run = Some((first, page)),
                        Some((first, prev)) => {
                            bytes += self
                                .copy((first * PAGE).max(from), ((prev + 1) * PAGE).min(to));
                            run = Some((page, page));
                        }
                        None => run = Some((page, page)),
                    }
                }
            }
            if let Some((first, prev)) = run {
                bytes += self.copy((first * PAGE).max(from), ((prev + 1) * PAGE).min(to));
            }
            if full && now != 0 {
                self.synced_at.fetch_max(now, Ordering::Release);
            }
            return bytes;
        }
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        let (mut bytes, mut run): (usize, Option<usize>) = (0, None);
        let mut newly_copied = 0u64;
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = &mut copied[page - base];
            if write == crate::watch::HOT_WRITE && *at != 0 {
                if let Some(first) = run.take() {
                    bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
                }
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    *at = now;
                }
                bytes += self.copy_hot((page * PAGE).max(from), ((page + 1) * PAGE).min(to));
            } else if *at == 0 || write > *at {
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    if *at == 0 && now != 0 {
                        newly_copied += 1;
                    }
                    *at = now;
                }
                run.get_or_insert(page);
            } else if let Some(first) = run.take() {
                bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
            }
        }
        if newly_copied > 0 {
            self.uncopied.fetch_sub(newly_copied, Ordering::Release);
        }
        if let Some(first) = run {
            bytes += self.copy((first * PAGE).max(from), ((last + 1) * PAGE).min(to));
        }
        if full && now != 0 && self.uncopied.load(Ordering::Acquire) == 0 {
            self.synced_at.fetch_max(now, Ordering::Release);
        }
        bytes
    }

    fn copy_hot(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        if from >= to {
            return 0;
        }
        let len = to - from;
        if len > PAGE {
            return self.copy(from, to);
        }
        thread_local! {
            static HOT_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        HOT_BUF.with(|cell| {
            let mut buf = cell.borrow_mut();
            if buf.len() < len {
                buf.resize(PAGE, 0);
            }
            let slice = &mut buf[..len];
            if !crate::host::read_into(from as *const u8, slice) {
                return self.copy(from, to);
            }
            let off = from - start;
            // SAFETY: inside the write-back shared buffer mapping (`from..to` is within `start..end`).
            let held = unsafe { std::slice::from_raw_parts_mut(self.data.add(off), len) };
            let Some(first) = first_difference(slice, held) else {
                return 0;
            };
            let last = last_difference(slice, held).unwrap_or(first);
            let n = last + 1 - first;
            held[first..=last].copy_from_slice(&slice[first..=last]);
            self.changed(off + first, off + first + n);
            self.card_changed(off + first, off + first + n);
            n
        })
    }

    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        crate::host::copy_mapped(at, from as *const u8, to - from, false);
        self.changed(from - start, to - start);
        self.card_changed(from - start, to - start);
        to - from
    }

    pub(crate) fn carded(&self) -> bool {
        self.card.is_some()
    }

    fn card_changed(&self, from: usize, to: usize) {
        let Some(card) = &self.card else {
            return;
        };
        if from >= to {
            return;
        }
        let mut dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
        match dirty.last_mut() {
            Some(last) if from <= last.1 && to >= last.0 => {
                *last = (last.0.min(from), last.1.max(to))
            }
            _ => dirty.push((from, to)),
        }
    }

    pub(crate) fn card_sides(&self) -> Option<(&ID3D12Resource, Option<ID3D12Resource>, u64)> {
        let card = self.card.as_ref()?;
        let before = card
            .before
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Some((&card.staging, before, card.size))
    }

    pub(crate) fn card_make_before(
        &self,
        make: impl FnOnce(u64) -> Option<ID3D12Resource>,
    ) -> bool {
        let Some(card) = &self.card else {
            return false;
        };
        let mut before = card.before.lock().unwrap_or_else(|e| e.into_inner());
        if before.is_some() {
            return false;
        }
        *before = make(card.size);
        if before.is_some() {
            CARD_BYTES[1].fetch_add(card_hash_bytes(card.size), Ordering::Relaxed);
        }
        before.is_some()
    }

    pub(crate) fn card_span(&self, address: usize, end: usize) -> Option<CardSpan> {
        let (from, to) = self.inside(address, end.saturating_sub(address));
        (from < to).then(|| card_span_of(self.start as usize, from, to))
    }

    pub(crate) fn card_run(&self, from: usize, to: usize) -> CardSpan {
        let to = to.min(self.len as usize);
        card_span_of(self.start as usize, from.min(to.saturating_sub(1)), to)
    }

    pub(crate) fn card_piece(&self, span: &CardSpan, i: u32) -> (usize, usize) {
        card_piece_of(self.start as usize, self.len as usize, span, i)
    }

    pub(crate) fn walls_merged(&self, fetched: usize, written: usize) {
        if !crate::host::walls_trace() {
            return;
        }
        static SEEN: Mutex<(u64, BTreeMap<u64, (u64, u32, u64, u64)>)> =
            Mutex::new((0, BTreeMap::new()));
        let frame = crate::frame::current();
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.0 != frame {
            let buffers: Vec<String> = seen
                .1
                .iter()
                .map(|(at, v)| {
                    format!(
                        "{at:#x}+{:#x}: {} pieces, {} fetched, {} written",
                        v.0, v.1, v.2, v.3
                    )
                })
                .take(32)
                .collect();
            if !buffers.is_empty() {
                tracing::info!(
                    frame = seen.0,
                    count = seen.1.len(),
                    buffers = ?buffers,
                    "walls trace: shader-written card buffers merged into title memory in this frame"
                );
            }
            seen.1.clear();
            seen.0 = frame;
        }
        let entry = seen.1.entry(self.start).or_insert((self.len, 0, 0, 0));
        entry.1 += 1;
        entry.2 += fetched as u64;
        entry.3 += written as u64;
    }

    pub(crate) fn card_merge(&self, at: usize, gpu: &[u8], title: &mut Vec<u8>) -> usize {
        let start = self.start as usize;
        if self.card.is_none() || at + gpu.len() > self.len as usize {
            return 0;
        }
        // SAFETY: inside the mapping (checked above); only the submitting
        // thread writes it.
        let old = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), gpu.len()) };
        let Some(first) = first_difference(gpu, old) else {
            return 0;
        };
        let last = last_difference(gpu, old).unwrap_or(first);
        let (at, n) = (at + first, last + 1 - first);
        let (gpu, old) = (&gpu[first..=last], &mut old[first..=last]);
        title.clear();
        title.resize(n, 0);
        let mut written = 0;
        if crate::host::read_into((start + at) as *const u8, title) {
            let as_read = merge_race_on().then(|| title.clone());
            let mut differs = 0u8;
            for ((t, g), o) in title.iter_mut().zip(gpu).zip(old.iter_mut()) {
                let wrote = if *g != *o { 0xffu8 } else { 0 };
                let merged = (*t & !wrote) | (*g & wrote);
                differs |= merged ^ *t;
                *t = merged;
                *o = merged;
            }
            if differs != 0 {
                crate::watch::writing(start + at, n);
                merge_race(
                    as_read.as_deref(),
                    (self.start, self.len),
                    start + at,
                    &title[..],
                    "card",
                );
                crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
                written = n;
            }
        } else {
            old.copy_from_slice(gpu);
        }
        self.card_changed(at, at + n);
        written
    }

    pub(crate) fn card_merge_relay(
        &self,
        at: usize,
        gpu: &[u8],
        fetched_after: u64,
        title: &mut Vec<u8>,
    ) -> usize {
        let start = self.start as usize;
        let Some(card) = &self.card else {
            return 0;
        };
        if at + gpu.len() > self.len as usize {
            return 0;
        }
        let _side = self.before.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: inside the mapping (checked above). The offsets are in a
        // range declared and not done with, which no refresh writes; what
        // does write it (the title's bytes put in under shader writes)
        // holds the lock held here.
        let held = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), gpu.len()) };
        let Some(first) = first_difference(gpu, held) else {
            return 0;
        };
        let last = last_difference(gpu, held).unwrap_or(first);
        let (at, n) = (at + first, last + 1 - first);
        let (gpu, held) = (&gpu[first..=last], &mut held[first..=last]);
        let skip: Vec<(usize, usize)> = {
            let dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
            let sent = card.sent.lock().unwrap_or_else(|e| e.into_inner());
            dirty
                .iter()
                .copied()
                .chain(
                    sent.iter()
                        .filter(|s| s.2 > fetched_after)
                        .map(|s| (s.0, s.1)),
                )
                .filter(|&(a, b)| a < at + n && b > at)
                .collect()
        };
        title.clear();
        title.resize(n, 0);
        let mapped = crate::host::read_into((start + at) as *const u8, title);
        let as_read = (mapped && merge_race_on()).then(|| title.clone());
        let mut changed = false;
        for (a, b) in outside(at, at + n, skip) {
            let (a, b) = (a - at, b - at);
            changed |= merge_piece(&gpu[a..b], &mut held[a..b], &mut title[a..b]);
        }
        if !mapped || !changed {
            return 0;
        }
        crate::watch::writing(start + at, n);
        merge_race(
            as_read.as_deref(),
            (self.start, self.len),
            start + at,
            &title[..],
            "card relay",
        );
        crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
        n
    }
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

static BUFFERS: RwLock<BTreeMap<(u64, usize), Entry>> = RwLock::new(BTreeMap::new());

fn buffers_read() -> std::sync::RwLockReadGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.read().unwrap_or_else(|e| e.into_inner())
}

fn buffers_write() -> std::sync::RwLockWriteGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.write().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers_write().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
}

pub(crate) fn unregister(resource: usize, start: u64) {
    buffers_write().remove(&(start, resource));
}

#[cfg(feature = "diagnostics")]
pub(crate) fn extent(address: u64) -> Option<(u64, u64)> {
    buffers_read()
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end > address)
        .map(|(k, e)| (k.0, e.end - k.0))
}

static INPLACE_EVICTED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

pub(crate) fn take_in_place_evicted() -> Vec<(u64, u64)> {
    let mut list = INPLACE_EVICTED.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *list)
}

pub(crate) fn in_place_let_go(at: u64, len: u64) -> u64 {
    let end = at.saturating_add(len);
    let mut gone: Vec<Arc<Shadow>> = Vec::new();
    {
        let mut map = LOOSE.write().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<(u64, u64)> = map
            .range((at.saturating_sub(WINDOW), 0)..(end, 0))
            .filter(|(key, shadow)| shadow.place.is_some() && key.0 + key.1 > at)
            .map(|(key, _)| *key)
            .collect();
        gone.extend(keys.into_iter().filter_map(|key| map.remove(&key)));
    }
    {
        let mut map = buffers_write();
        for (_, entry) in map.range_mut(..(end, 0)).rev().take(256) {
            if entry.end > at && entry.shadow.as_ref().is_some_and(|s| s.place.is_some()) {
                gone.extend(entry.shadow.take());
            }
        }
    }
    let n = gone.len() as u64;
    if !gone.is_empty() {
        let mut evicted = INPLACE_EVICTED.lock().unwrap_or_else(|e| e.into_inner());
        for shadow in &gone {
            evicted.push((shadow.start, shadow.start.saturating_add(shadow.len)));
        }
    }
    for shadow in gone {
        crate::host::retire(shadow);
    }
    n
}

pub(crate) fn spans() -> Vec<(u64, u64, bool)> {
    let mut spans: Vec<(u64, u64, bool)> = buffers_read()
        .iter()
        .map(|(key, entry)| (key.0, entry.end - key.0, false))
        .collect();
    let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
    spans.extend(loose.keys().map(|&(start, len)| (start, len, true)));
    spans
}

pub(crate) fn find_existing(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    {
        let map = buffers_read();
        if let Some((_, entry)) = map
            .range(..=(address, usize::MAX))
            .rev()
            .take(64)
            .find(|(_, e)| e.end >= end)
            && let Some(shadow) = &entry.shadow
        {
            return Some(shadow.clone());
        }
    }
    let map = LOOSE.read().unwrap_or_else(|e| e.into_inner());
    let from = address.saturating_sub(WINDOW);
    map.range((from, 0)..=(address, u64::MAX))
        .find(|((start, bytes), _)| *start <= address && *start + *bytes >= end)
        .map(|(_, s)| s.clone())
}

pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    lookup(address, len, false)
}

pub(crate) fn find_any(address: u64, len: u64) -> Option<Arc<Shadow>> {
    if !fine() {
        return find(address, len);
    }
    lookup(address, len, true)
}

fn lookup(address: u64, len: u64, any: bool) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    {
        let map = buffers_read();
        let found = map
            .range(..=(address, usize::MAX))
            .rev()
            .take(64)
            .find(|(_, e)| e.end >= end);
        match found {
            Some((_, entry)) if entry.shadow.is_some() => return entry.shadow.clone(),
            None => {
                drop(map);
                return loose(address, end, len <= 1, any);
            }
            Some(_) => {}
        }
    }
    let mut map = buffers_write();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end, len <= 1, any);
    };
    let entry = map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        let size = len.next_multiple_of(1 << 16);
        static INPLACE_BIG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let inplace_big = *INPLACE_BIG
            .get_or_init(|| std::env::var_os("OATHKEEP_INPLACE_BIG").is_none_or(|v| v != "0"));
        let big = !inplace_big && cards() && size >= card_limits().0;
        if !big && let Some(placed) = crate::inplace::place_for(key.0, len, false) {
            entry.shadow = Some(Arc::new(Shadow::placed(placed, key.0, len, false)));
            return entry.shadow.clone();
        }
        let (buffer, data) = host()?.shared_buffer(size)?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        let (buffer, card) = match card_for(size) {
            Some(on_card) => {
                crate::host::name(&on_card, || {
                    format!("title buffer {:#x}+{len:#x} on the card", key.0)
                });
                tracing::info!(
                    start = format_args!("{:#x}", key.0),
                    len,
                    "a buffer's host copy is in the card's memory"
                );
                let card = Card {
                    staging: buffer,
                    before: Mutex::default(),
                    dirty: Mutex::default(),
                    sent: Mutex::default(),
                    size,
                };
                (on_card, Some(card))
            }
            None => (buffer, None),
        };
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        let mut shadow = Shadow::new(buffer, data, gpu, key.0, len);
        shadow.card = card;
        let shadow = Arc::new(shadow);
        if shadow.card.is_some() {
            CARDED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::downgrade(&shadow));
        }
        entry.shadow = Some(shadow);
    }
    entry.shadow.clone()
}

const WINDOW: u64 = 4 << 20;

fn window() -> u64 {
    static GRID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *GRID.get_or_init(|| {
        let kb = std::env::var("OATHKEEP_WINDOW_KB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok());
        grid(kb)
    })
}

fn grid(kb: Option<u64>) -> u64 {
    match kb {
        Some(kb) if kb.is_power_of_two() && (64..=4096).contains(&kb) => kb << 10,
        _ => WINDOW,
    }
}

fn window_of(address: u64, end: u64, grid: u64) -> (u64, u64) {
    let start = address & !(grid - 1);
    (start, end.next_multiple_of(grid) - start)
}

fn tails() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WINDOW_TAILS").is_some_and(|v| v == "1"))
}

const TAIL_STEP: u64 = 256 << 10;

fn tail_of(address: u64, end: u64) -> (u64, u64) {
    let (cell, len) = window_of(address, end, WINDOW);
    let start = address & !(TAIL_STEP - 1);
    (start, cell + len - start)
}

fn holder(windows: impl Iterator<Item = (u64, u64)>, address: u64, end: u64) -> Option<(u64, u64)> {
    windows
        .filter(|&(start, bytes)| start <= address && start + bytes >= end)
        .max_by_key(|&(start, bytes)| (bytes, std::cmp::Reverse(start)))
}

fn fine() -> bool {
    tails() || window() < WINDOW
}

#[cfg(feature = "diagnostics")]
pub(crate) fn window_cover() -> u64 {
    let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
    cover(loose.keys().copied())
}

#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
fn cover(windows: impl Iterator<Item = (u64, u64)>) -> u64 {
    let (mut covered, mut reached) = (0, 0);
    for (start, bytes) in windows {
        let end = start + bytes;
        if end > reached {
            covered += end - start.max(reached);
            reached = end;
        }
    }
    covered
}

fn longest_tail(
    windows: impl Iterator<Item = (u64, u64)>,
    address: u64,
    end: u64,
) -> Option<(u64, u64)> {
    let (cell, len) = window_of(address, end, WINDOW);
    let step = address & !(TAIL_STEP - 1);
    windows
        .filter(|&(start, bytes)| start >= cell && start <= step && start + bytes == cell + len)
        .min()
}

static LOOSE: RwLock<BTreeMap<(u64, u64), Arc<Shadow>>> = RwLock::new(BTreeMap::new());

fn loose(address: u64, end: u64, unsized_ask: bool, any: bool) -> Option<Arc<Shadow>> {
    let (cell, _) = window_of(address, end, WINDOW);
    let search_from = address.saturating_sub(WINDOW);
    let named = {
        let grid_size = if unsized_ask { WINDOW } else { window() };
        let grid = window_of(address, end, grid_size);
        if crate::inplace::level() >= 3 && !crate::inplace::section_run_in_place(grid.0, grid.1) {
            if let Some((rs, rl, next_mapped)) =
                crate::inplace::section_run_around(address, end, grid_size)
                && (!unsized_ask || (rs + rl).saturating_sub(address) >= (256 << 10) || !next_mapped)
            {
                (rs, rl)
            } else if !unsized_ask {
                let page = window_of(address, end, 64 << 10);
                if page != grid && crate::inplace::section_run_in_place(page.0, page.1) {
                    page
                } else {
                    grid
                }
            } else {
                grid
            }
        } else {
            grid
        }
    };
    let pick = |map: &BTreeMap<(u64, u64), Arc<Shadow>>| {
        if unsized_ask && crate::inplace::level() >= 3 {
            let there = map
                .range((search_from, 0)..=(address, u64::MAX))
                .filter(|((start, bytes), _)| {
                    *start <= address
                        && *start + *bytes >= end
                        && (*start + *bytes).saturating_sub(address) >= (256 << 10)
                })
                .map(|(key, _)| *key);
            if let Some(found) = holder(there, address, end) {
                return found;
            }
            if tails() && !crate::inplace::section_run_in_place(named.0, named.1) {
                let there = map
                    .range((cell, 0)..=(address, u64::MAX))
                    .map(|(key, _)| *key);
                return longest_tail(there, address, end).unwrap_or_else(|| tail_of(address, end));
            }
            named
        } else if unsized_ask && tails() {
            let there = map
                .range((cell, 0)..=(address, u64::MAX))
                .map(|(key, _)| *key);
            longest_tail(there, address, end).unwrap_or_else(|| tail_of(address, end))
        } else if (fine() || crate::inplace::level() >= 3) && !unsized_ask {
            let _ = any;
            let there = map
                .range((search_from, 0)..=(address, u64::MAX))
                .map(|(key, _)| *key);
            holder(there, address, end).unwrap_or(named)
        } else {
            named
        }
    };
    let note = |shadow: &Shadow| {
        shadow.used.store(crate::host::moment(), Ordering::Relaxed);
        shadow.asked[0].fetch_min(address, Ordering::Relaxed);
        shadow.asked[1].fetch_max(end, Ordering::Relaxed);
        if unsized_ask {
            shadow.unsized_asks.fetch_add(1, Ordering::Relaxed);
        }
    };
    {
        let map = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        let key = pick(&map);
        if let Some(shadow) = map.get(&key) {
            note(shadow);
            return Some(shadow.clone());
        }
    }
    let mut map = LOOSE.write().unwrap_or_else(|e| e.into_inner());
    let (start, len) = pick(&map);
    if let Some(shadow) = map.get(&(start, len)) {
        note(shadow);
        return Some(shadow.clone());
    }
    if let Some(placed) = crate::inplace::place_for(start, len, true) {
        let shadow = Arc::new(Shadow::placed(placed, start, len, true));
        note(&shadow);
        map.insert((start, len), shadow.clone());
        return Some(shadow);
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    let (buffer, card) = match window_card_for(len) {
        Some(on_card) => {
            crate::host::name(&on_card, || {
                format!("title memory {start:#x}+{len:#x} on the card")
            });
            let card = Card {
                staging: buffer,
                before: Mutex::default(),
                dirty: Mutex::default(),
                sent: Mutex::default(),
                size: len,
            };
            (on_card, Some(card))
        }
        None => (buffer, None),
    };
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let mut shadow = Shadow::new(buffer, data, gpu, start, len);
    shadow.window = true;
    shadow.card = card;
    let shadow = Arc::new(shadow);
    if shadow.card.is_some() {
        CARDED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::downgrade(&shadow));
    }
    note(&shadow);
    map.insert((start, len), shadow.clone());
    Some(shadow)
}

#[cfg(test)]
mod window_tests {
    use super::{WINDOW, cover, grid, holder, longest_tail, tail_of, window_of};

    #[test]
    fn an_ask_takes_the_longest_window_that_holds_it() {
        let cell = 0xd_1200_0000u64;
        let (ask, end) = (cell + 0x9_0000, cell + 0xb_0000);
        assert_eq!(holder([].into_iter(), ask, end), None);
        let there = [
            (cell, 0x4_0000),
            (cell + 0x8_0000, 0x2_0000),
            (cell + 0x8_0000, 0x4_0000),
            (cell + 0x4_0000, 0x10_0000),
            (cell + 0xa_0000, 0x40_0000),
        ];
        assert_eq!(
            holder(there.into_iter(), ask, end),
            Some((cell + 0x4_0000, 0x10_0000))
        );
        let two = [(cell + 0x8_0000, 0x8_0000), (cell + 0x4_0000, 0x8_0000)];
        assert_eq!(
            holder(two.into_iter(), ask, end),
            Some((cell + 0x4_0000, 0x8_0000))
        );
    }

    #[test]
    fn overlapping_windows_cover_their_memory_once() {
        assert_eq!(cover([].into_iter()), 0);
        let apart = [(0x10_0000u64, 0x4_0000u64), (0x20_0000, 0x4_0000)];
        assert_eq!(cover(apart.into_iter()), 0x8_0000);
        let over = [
            (0x10_0000u64, 0x10_0000u64),
            (0x14_0000, 0x4_0000),
            (0x1c_0000, 0x8_0000),
            (0x24_0000, 0x4_0000),
        ];
        assert_eq!(cover(over.into_iter()), 0x18_0000);
    }

    #[test]
    fn tails_run_to_the_cells_end_and_the_longest_is_taken() {
        let cell = 0xd_1200_0000u64;
        let ask = cell + 0x12_3456;
        assert_eq!(
            tail_of(ask, ask + 1),
            (cell + 0x10_0000, WINDOW - 0x10_0000)
        );
        assert_eq!(tail_of(cell + 0xffff, cell + 0x1_0000), (cell, WINDOW));
        let end = cell + WINDOW;
        let tail = |from: u64| (cell + from, WINDOW - from);
        assert_eq!(longest_tail([].into_iter(), ask, ask + 1), None);
        assert_eq!(
            longest_tail([tail(0x20_0000)].into_iter(), ask, ask + 1),
            None
        );
        let there = [
            (cell - WINDOW, WINDOW),
            (cell, 0x4_0000),
            tail(0x8_0000),
            tail(0x10_0000),
            (cell + 0x10_0000, 0x4_0000),
            tail(0x20_0000),
        ];
        assert_eq!(
            longest_tail(there.into_iter(), ask, ask + 1),
            Some(tail(0x8_0000))
        );
        let (start, bytes) = tail(0x8_0000);
        assert!(start <= ask && start + bytes == end);
        let with_whole = [(cell, WINDOW), tail(0x8_0000)];
        assert_eq!(
            longest_tail(with_whole.into_iter(), ask, ask + 1),
            Some((cell, WINDOW))
        );
    }

    #[test]
    fn the_grid_is_a_power_of_two_or_the_default() {
        assert_eq!(grid(None), WINDOW);
        assert_eq!(grid(Some(4096)), WINDOW);
        assert_eq!(grid(Some(1024)), 1 << 20);
        assert_eq!(grid(Some(256)), 256 << 10);
        assert_eq!(grid(Some(64)), 64 << 10);
        for bad in [0, 32, 100, 1000, 8192] {
            assert_eq!(grid(Some(bad)), WINDOW, "{bad}");
        }
    }

    #[test]
    fn windows_hold_their_ask_on_the_grid() {
        let (address, end) = (0xd_1234_5678u64, 0xd_1234_5678u64 + 0x9000);
        assert_eq!(window_of(address, end, WINDOW), (0xd_1200_0000, 4 << 20));
        assert_eq!(window_of(address, end, 1 << 20), (0xd_1230_0000, 1 << 20));
        assert_eq!(
            window_of(address, end, 256 << 10),
            (0xd_1234_0000, 256 << 10)
        );
        let end = address + 0x4_0000;
        assert_eq!(
            window_of(address, end, 256 << 10),
            (0xd_1234_0000, 512 << 10)
        );
        for g in [64u64 << 10, 256 << 10, 1 << 20, WINDOW] {
            let (start, len) = window_of(address, end, g);
            assert!(start <= address && start + len >= end);
            assert!(start % g == 0 && len % g == 0);
        }
    }
}

pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers_read()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}
