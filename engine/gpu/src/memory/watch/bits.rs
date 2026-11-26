use std::collections::BTreeMap;
use std::ops::{Bound, RangeBounds};

/// A bit per block of the address space, in pieces made when first needed
/// and never let go.
pub(super) struct Bits([std::sync::atomic::AtomicPtr<BitsPiece>; Bits::PIECES]);

/// The bits of 4 GiB.
type BitsPiece = [std::sync::atomic::AtomicU64; Bits::WORDS];

impl Bits {
    const WORDS: usize = 1 << 10;
    /// 128 TiB, what a process has.
    const PIECES: usize = 1 << 15;

    pub(super) const fn new() -> Bits {
        Bits([const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; Bits::PIECES])
    }

    fn piece(&self, word: usize) -> Option<&BitsPiece> {
        let piece = self.0.get(word / Bits::WORDS)?.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: null, or a piece made by `set`, which is never freed.
        unsafe { piece.as_ref() }
    }

    pub(super) fn set(&self, block: usize) {
        use std::sync::atomic::Ordering::{AcqRel, Acquire, SeqCst};
        let word = block / 64;
        let Some(place) = self.0.get(word / Bits::WORDS) else {
            return;
        };
        if place.load(Acquire).is_null() {
            let piece: Box<BitsPiece> = Box::new([const { std::sync::atomic::AtomicU64::new(0) }; Bits::WORDS]);
            let piece = Box::into_raw(piece);
            if place.compare_exchange(std::ptr::null_mut(), piece, AcqRel, Acquire).is_err() {
                // SAFETY: made just now, and another thread's is there.
                drop(unsafe { Box::from_raw(piece) });
            }
        }
        if let Some(piece) = self.piece(word) {
            piece[word % Bits::WORDS].fetch_or(1 << (block % 64), SeqCst);
        }
    }

    /// Sixty-four of them.
    pub(super) fn word(&self, word: usize) -> u64 {
        self.piece(word).map_or(0, |piece| piece[word % Bits::WORDS].load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Whether it was set, and is no more.
    pub(super) fn take(&self, block: usize) -> bool {
        self.any(block, block + 1) && {
            let (word, bit) = (block / 64, 1 << (block % 64));
            self.piece(word)
                .is_some_and(|piece| piece[word % Bits::WORDS].fetch_and(!bit, std::sync::atomic::Ordering::SeqCst) & bit != 0)
        }
    }

    pub(super) fn clear(&self, block: usize) {
        let word = block / 64;
        if let Some(piece) = self.piece(word) {
            // Ahead of the protection's change, and seen by whoever the
            // writer tells of its write.
            piece[word % Bits::WORDS].fetch_and(!(1 << (block % 64)), std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether a bit of `[first, last)` is set.
    pub(super) fn any(&self, first: usize, last: usize) -> bool {
        let mut block = first;
        while block < last {
            let word = block / 64;
            let until = ((word + 1) * 64).min(last);
            let mask = (u64::MAX >> (64 - (until - block))) << (block % 64);
            match self.piece(word) {
                Some(piece) => {
                    if piece[word % Bits::WORDS].load(std::sync::atomic::Ordering::SeqCst) & mask != 0 {
                        return true;
                    }
                    block = until;
                }
                // None of the piece's.
                None => block = (word / Bits::WORDS + 1) * Bits::WORDS * 64,
            }
        }
        false
    }

    /// Whether the bits of all of `[first, last)` are set (not so of none).
    pub(super) fn all(&self, first: usize, last: usize) -> bool {
        let mut block = first;
        while block < last {
            let word = block / 64;
            let until = ((word + 1) * 64).min(last);
            let mask = (u64::MAX >> (64 - (until - block))) << (block % 64);
            let Some(piece) = self.piece(word) else {
                return false;
            };
            if piece[word % Bits::WORDS].load(std::sync::atomic::Ordering::Acquire) & mask != mask {
                return false;
            }
            block = until;
        }
        first < last
    }
}

/// A set of blocks, a bit each: a fault, or a look at a range of thousands of
/// blocks, is a few words of it and no walk through a tree of every guarded
/// block there is.
pub(super) struct Blocks(BTreeMap<usize, Box<[u64; Blocks::WORDS]>>);

impl Blocks {
    /// The words of 4 GiB.
    const WORDS: usize = 1 << 10;
    pub(super) const IN_PIECE: usize = Blocks::WORDS * 64;

    pub(super) const fn new() -> Blocks {
        Blocks(BTreeMap::new())
    }

    pub(super) fn word(&self, word: usize) -> u64 {
        self.0.get(&(word / Blocks::WORDS)).map_or(0, |piece| piece[word % Blocks::WORDS])
    }

    pub(super) fn contains(&self, block: usize) -> bool {
        self.word(block / 64) >> (block % 64) & 1 != 0
    }

    /// Whether it was not there.
    pub(super) fn insert(&mut self, block: usize) -> bool {
        let piece = self.0.entry(block / Blocks::IN_PIECE).or_insert_with(|| Box::new([0; Blocks::WORDS]));
        let word = &mut piece[block % Blocks::IN_PIECE / 64];
        let was = *word;
        *word |= 1 << (block % 64);
        was != *word
    }

    /// Whether it was there.
    pub(super) fn remove(&mut self, block: usize) -> bool {
        let Some(piece) = self.0.get_mut(&(block / Blocks::IN_PIECE)) else {
            return false;
        };
        let word = &mut piece[block % Blocks::IN_PIECE / 64];
        let was = *word;
        *word &= !(1 << (block % 64));
        was != *word
    }

    pub(super) fn insert_all(&mut self, blocks: std::ops::Range<usize>) {
        for block in blocks {
            self.insert(block);
        }
    }

    /// Those of `range`, in order.
    pub(super) fn range(&self, range: impl RangeBounds<usize>) -> impl Iterator<Item = usize> + '_ {
        let first = match range.start_bound() {
            Bound::Included(&first) => first,
            Bound::Excluded(&before) => before.saturating_add(1),
            Bound::Unbounded => 0,
        };
        // (Exclusive. No block's number is near the top.)
        let last = match range.end_bound() {
            Bound::Included(&last) => last.saturating_add(1),
            Bound::Excluded(&last) => last,
            Bound::Unbounded => usize::MAX,
        }
        .max(first);
        self.0.range(first / Blocks::IN_PIECE..=last / Blocks::IN_PIECE).flat_map(move |(&piece, words)| {
            let base = piece * Blocks::IN_PIECE;
            let from = first.max(base) - base;
            let to = last.min(base + Blocks::IN_PIECE).saturating_sub(base);
            let words = if from < to { &words[from / 64..to.div_ceil(64)] } else { &[][..] };
            words.iter().enumerate().flat_map(move |(k, &bits)| {
                let at = (from / 64 + k) * 64;
                BitsOf(bits & in_word(at, from, to)).map(move |bit| base + at + bit)
            })
        })
    }

    pub(super) fn len(&self) -> usize {
        self.0.values().flat_map(|piece| piece.iter()).map(|word| word.count_ones() as usize).sum()
    }
}

/// Of the 64 blocks from `at` on, those in `[from, to)`, a bit each.
pub(super) fn in_word(at: usize, from: usize, to: usize) -> u64 {
    let low = from.saturating_sub(at).min(64);
    let high = to.saturating_sub(at).min(64);
    if low >= high { 0 } else { (u64::MAX >> (64 - (high - low))) << low }
}

/// The bits of a word that are set, by number.
pub(super) struct BitsOf(pub(super) u64);

impl Iterator for BitsOf {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        let bit = (self.0 != 0).then(|| self.0.trailing_zeros() as usize)?;
        self.0 &= self.0 - 1;
        Some(bit)
    }
}

/// The 4 KiB pages of title memory that what shaders wrote was put into,
/// and the frame it last was: in pieces made when first needed and never let
/// go. Some 20,000 pages a frame are noted by the relay and the landing
/// threads: in a map behind one lock that was 309 waits for it and 280 looks
/// at its table in the 40 s of a profile, as many as all else of the landings'
/// notes (run faults-5).
pub(super) struct Landed(pub(super) [std::sync::atomic::AtomicPtr<LandedPiece>; Landed::PIECES]);

/// Of 256 MiB: the frame (its low 31 bits and one), 0 for never.
type LandedPiece = [std::sync::atomic::AtomicU32; Landed::PAGES];

impl Landed {
    pub(super) const PAGES: usize = 1 << 16;
    /// 128 TiB, what a process has.
    pub(super) const PIECES: usize = 1 << 19;

    fn stamp(frame: u64) -> u32 {
        (frame as u32 & 0x7fff_ffff) + 1
    }

    pub(super) fn note(&self, page: usize, frame: u64) {
        use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
        let Some(place) = self.0.get(page / Landed::PAGES) else {
            return;
        };
        let mut piece = place.load(Acquire);
        if piece.is_null() {
            let made: Box<LandedPiece> = Box::new([const { std::sync::atomic::AtomicU32::new(0) }; Landed::PAGES]);
            let made = Box::into_raw(made);
            piece = match place.compare_exchange(std::ptr::null_mut(), made, AcqRel, Acquire) {
                Ok(_) => made,
                Err(there) => {
                    // SAFETY: made just now, and another thread's is there.
                    drop(unsafe { Box::from_raw(made) });
                    there
                }
            };
        }
        // SAFETY: a piece, which is never freed.
        let at = unsafe { &(&*piece)[page % Landed::PAGES] };
        let stamp = Landed::stamp(frame);
        // (Most are noted several times a frame: no write then.)
        if at.load(Relaxed) != stamp {
            at.store(stamp, Relaxed);
        }
    }

    /// Whether `page` was noted in a frame from `since` on, up to `now`.
    pub(super) fn since(&self, page: usize, since: u64, now: u64) -> bool {
        let Some(place) = self.0.get(page / Landed::PAGES) else {
            return false;
        };
        // SAFETY: null, or a piece, which is never freed.
        let Some(piece) = (unsafe { place.load(std::sync::atomic::Ordering::Acquire).as_ref() }) else {
            return false;
        };
        let stamp = piece[page % Landed::PAGES].load(std::sync::atomic::Ordering::Relaxed);
        stamp != 0 && u64::from(Landed::stamp(now).wrapping_sub(stamp) & 0x7fff_ffff) <= now.saturating_sub(since)
    }
}

pub(super) static LANDED: Landed = Landed([const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; Landed::PIECES]);

/// A page's number, spread.
#[derive(Default)]
pub(super) struct PageHash(u64);

impl std::hash::Hasher for PageHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(26).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}
