use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::Ordering;

/// Bytes of a block; pieces of more than half of one are not pooled (with
/// blocks made ahead, [`super::ahead`]: of more than one).
pub(super) const BLOCK: u64 = 64 << 20;
/// Blocks with nothing in them that are kept.
pub(super) const SPARE: usize = 2;
/// The same while the card has room ([`super::room`]): 4 GiB of them.
pub(super) const SPARE_KEPT: usize = 64;
/// The same of blocks of staging buffers, which are the PC's memory: run
/// crash-1 held 5.2 GB of them.
pub(super) const STAGING_KEPT: usize = 8;

/// Milliseconds until a piece given back is used again. (It was three
/// frames, a tenth of a second while the title shows a loading screen at 30
/// a second, with up to 18 submissions in the air.)
pub(super) const REST: u64 = 2500;

/// Submissions begun and finished ([`crate::host::submissions`]), and how
/// many blocks to make ahead (0: every block is ripe at once).
#[derive(Clone, Copy)]
pub(super) struct Ripe {
    pub(super) started: u64,
    pub(super) finished: u64,
    pub(super) ahead: usize,
}

impl Ripe {
    pub(super) const ALL: Ripe = Ripe { started: 0, finished: 0, ahead: 0 };
}

/// The free ranges of a block: start and length, never two that touch.
pub(super) struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    pub(super) fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]))
    }

    /// The lowest place for `len` bytes at a multiple of `align`.
    pub(super) fn take(&mut self, len: u64, align: u64) -> Option<u64> {
        let (start, size, at) = self.0.iter().find_map(|(&start, &size)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })?;
        self.0.remove(&start);
        if at > start {
            self.0.insert(start, at - start);
        }
        if at + len < start + size {
            self.0.insert(at + len, start + size - at - len);
        }
        Some(at)
    }

    pub(super) fn give(&mut self, mut start: u64, mut len: u64) {
        if let Some((&before, &size)) = self.0.range(..start).next_back()
            && before + size == start
        {
            self.0.remove(&before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.0.remove(&(start + len)) {
            len += size;
        }
        self.0.insert(start, len);
    }

    pub(super) fn whole(&self, size: u64) -> bool {
        self.0.get(&0) == Some(&size)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    pub(super) block: usize,
    pub(super) start: u64,
    pub(super) len: u64,
}

pub(super) struct Block<B> {
    pub(super) memory: B,
    pub(super) free: Ranges,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made (zeros).
    pub(super) used_to: u64,
    /// The newest submission begun when it was made ([`Ripe`]).
    pub(super) made: u64,
    /// When that was (ms).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    pub(super) since: u64,
}

pub(super) struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    pub(super) blocks: Vec<Option<Block<B>>>,
    /// Pieces given back, and when each may be used again.
    pub(super) resting: VecDeque<(u64, Piece)>,
    /// The empty blocks it was last told to keep, and the most it keeps.
    pub(super) spare: usize,
    pub(super) most: usize,
    pub(super) retry_growth_at: u64,
}

impl<B: Clone> Pool<B> {
    pub(super) const fn new(most: usize) -> Pool<B> {
        Pool { blocks: Vec::new(), resting: VecDeque::new(), spare: 0, most, retry_growth_at: 0 }
    }

    /// A piece of `len` bytes and the memory it lies in, and whether that
    /// part was ever used before; `make` makes a block when none has room.
    #[cfg(test)]
    pub(super) fn take(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        self.take_ripe_capped(len, align, now, Ripe::ALL, None, make)
    }

    /// Like [`Pool::take`], refusing new block creation once `blocks() >= cap`.
    pub(super) fn take_capped(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        max_blocks: Option<usize>,
        make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        self.take_ripe_capped(len, align, now, Ripe::ALL, max_blocks, make)
    }

    /// The same from a block that is ripe, if one has room; `make` makes one
    /// ahead too, when few are empty.
    #[cfg(test)]
    pub(super) fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        self.take_ripe_capped(len, align, now, ripe, None, make)
    }

    /// Like [`Pool::take_ripe`], refusing new block creation once `blocks() >= cap`
    /// without calling `make` or arming `retry_growth_at`.
    pub(super) fn take_ripe_capped(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        mut ripe: Ripe,
        max_blocks: Option<usize>,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        if max_blocks == Some(0) {
            ripe.ahead = 0;
        }
        // With blocks made ahead, what would be an allocation of its own (and
        // new when the card first reads it) lies in one of those too.
        if len == 0 || len > if ripe.ahead > 0 { BLOCK } else { BLOCK / 2 } {
            return None;
        }
        self.rested(now);
        let mut found = None;
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if let Some(block) = block
                && ripe.is(block)
                && let Some(start) = block.free.take(len, align)
            {
                found = Some((i, start));
                break;
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            // The one that has been there the longest.
            let mut unripe: Vec<(u64, usize)> =
                self.blocks.iter().enumerate().filter_map(|(i, b)| Some((b.as_ref()?.made, i))).collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        // A failed 64 MiB block must not trigger another driver call for each
        // small texture. Existing free ranges remain usable during backoff.
        let (i, start) = match found {
            Some(found) => found,
            None => {
                if max_blocks.is_some_and(|cap| self.blocks() >= cap) {
                    return None;
                }
                let memory = self.grow(now, &mut make)?;
                let i = self.add(memory, ripe.started);
                (i, self.blocks[i].as_mut()?.free.take(len, align)?)
            }
        };
        if ripe.ahead > 0 {
            if !ripe.is(self.blocks[i].as_ref()?) {
                let n = super::UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
                }
            }
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count();
            if empty < ripe.ahead
                && !max_blocks.is_some_and(|cap| self.blocks() >= cap)
                && let Some(memory) = self.grow(now, &mut make)
            {
                self.add(memory, ripe.started);
            }
        }
        let block = self.blocks[i].as_mut()?;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    fn grow(&mut self, now: u64, make: &mut impl FnMut() -> Option<B>) -> Option<B> {
        if now < self.retry_growth_at {
            return None;
        }
        let block = make();
        if block.is_none() {
            self.retry_growth_at = now.saturating_add(500);
        }
        block
    }

    pub(super) fn add(&mut self, memory: B, made: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] = Some(Block { memory, free: Ranges::new(BLOCK), used_to: 0, made, since: crate::host::now_ms() });
        i
    }

    pub(super) fn give(&mut self, piece: Piece, now: u64) {
        self.resting.push_back((now + REST, piece));
    }

    /// Makes the pieces that rested long enough free, and lets go of empty
    /// blocks past [`SPARE`].
    pub(super) fn rested(&mut self, now: u64) {
        self.reclaim(now, super::SPARES.load(Ordering::Relaxed), false);
    }

    pub(super) fn reclaim(&mut self, now: u64, limit: usize, sweep: bool) {
        let mut any = false;
        while let Some(&(ready, piece)) = self.resting.front()
            && ready <= now
        {
            self.resting.pop_front();
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                any |= block.free.whole(BLOCK);
            }
        }
        // (Without a piece given back too, once there is less room.)
        let most = limit.min(self.most);
        let previous = std::mem::replace(&mut self.spare, most);
        if sweep || any || most < previous {
            let mut spare = 0;
            for block in &mut self.blocks {
                if block.as_ref().is_some_and(|b| b.free.whole(BLOCK)) {
                    spare += 1;
                    if spare > most {
                        *block = None;
                    }
                }
            }
        }
    }

    pub(super) fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }
}
