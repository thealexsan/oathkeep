use super::{BLOCK, JOB, count, stat};

/// A piece to land: the copy's number, the title address, its bytes.
pub(crate) type Piece = (u64, usize, usize);

/// `pieces` in the order they are landed: by the 64 KiB block of title
/// memory they lie in, those in one block in the order given (each is
/// within one block). Where one copy alone has pieces in a block, those
/// that overlap or touch are one piece: a landing's flights declare parts
/// of the same memory, and what overlapped came down and was merged twice.
/// Each with the number of the piece given that it begins with.
pub(crate) fn ordered(pieces: &[Piece]) -> Vec<(usize, usize, usize)> {
    let mut order: Vec<usize> = (0..pieces.len()).collect();
    order.sort_by_key(|&i| pieces[i].1 / BLOCK);
    let mut out: Vec<(usize, usize, usize)> = Vec::with_capacity(order.len());
    for group in order.chunk_by_mut(|&a, &b| pieces[a].1 / BLOCK == pieces[b].1 / BLOCK) {
        let alone = group.len() > 1 && group.iter().all(|&i| pieces[i].0 == pieces[group[0]].0);
        if !alone {
            out.extend(group.iter().map(|&i| (i, pieces[i].1, pieces[i].2)));
            continue;
        }
        group.sort_by_key(|&i| pieces[i].1);
        let begun = out.len();
        for &i in group.iter() {
            let (_, at, len) = pieces[i];
            match out[begun..].last_mut() {
                Some(last) if at <= last.1 + last.2 => {
                    let end = (last.1 + last.2).max(at + len);
                    count(stat::TWICE, (last.2 + len - (end - last.1)) as u64);
                    last.2 = end - last.1;
                }
                _ => out.push((i, at, len)),
            }
        }
    }
    out
}

/// Pieces (title address, bytes; in [`ordered`]'s order) as jobs: runs of
/// about [`JOB`] bytes that end where a block ends.
pub(crate) fn jobs(pieces: &[(usize, usize)]) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let (mut from, mut bytes) = (0, 0);
    for (i, &(at, len)) in pieces.iter().enumerate() {
        let new_block = i == 0 || pieces[i - 1].0 / BLOCK != at / BLOCK;
        if bytes >= JOB && new_block {
            out.push(from..i);
            (from, bytes) = (i, 0);
        }
        bytes += len;
    }
    if from < pieces.len() {
        out.push(from..pieces.len());
    }
    out
}

/// What earlier fetches brought, for as long as a flight they ran behind
/// has not landed: per fetch, the value the host's fence had been given
/// behind the last flight sent before it, and by copy (its number) and
/// piece the offsets it brought.
///
/// A fetch on the drawing queue runs behind everything sent before it, so
/// it brings what those flights' shaders wrote too, and the merge of it put
/// that into title memory. Their own landings flag the same pieces and
/// fetched them again, to find every byte merged. A piece that a fetch held
/// whole, behind the flight that flags it, has nothing more of that flight.
/// (Bytes left out of that merge, the title's that went up behind the
/// fetch, went up behind that flight too, and stand over what it wrote.)
#[derive(Default)]
pub(super) struct FastHasher(u64);

impl std::hash::Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64);
    }
}

type FastMap<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<FastHasher>>;

/// Fetched byte intervals within one `(copy, block)`, storing up to two
/// disjoint intervals inline without heap allocation and coalescing touching
/// or overlapping ranges so disjoint sub-ranges in the same 64 KiB block are
/// never forgotten.
#[derive(Clone, Default)]
pub(super) struct BlockSpans {
    inline: [(usize, usize); 2],
    len: u8,
    extra: Vec<(usize, usize)>,
}

impl BlockSpans {
    fn insert(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        if self.extra.is_empty() {
            match self.len {
                0 => {
                    self.inline[0] = (from, to);
                    self.len = 1;
                    return;
                }
                1 => {
                    let (a, b) = self.inline[0];
                    if from <= b && to >= a {
                        self.inline[0] = (a.min(from), b.max(to));
                    } else if to < a {
                        self.inline = [(from, to), (a, b)];
                        self.len = 2;
                    } else {
                        self.inline[1] = (from, to);
                        self.len = 2;
                    }
                    return;
                }
                _ => {
                    for slot in &mut self.inline {
                        if slot.0 <= from && to <= slot.1 {
                            return;
                        }
                    }
                    self.extra.reserve(4);
                    self.extra.push(self.inline[0]);
                    self.extra.push(self.inline[1]);
                }
            }
        }
        self.extra.push((from, to));
        self.extra.sort_unstable();
        let mut w = 0usize;
        for i in 0..self.extra.len() {
            let (a, b) = self.extra[i];
            if w > 0 && a <= self.extra[w - 1].1 {
                self.extra[w - 1].1 = self.extra[w - 1].1.max(b);
            } else {
                self.extra[w] = (a, b);
                w += 1;
            }
        }
        self.extra.truncate(w);
        if self.extra.len() <= 2 {
            self.len = self.extra.len() as u8;
            for (i, &span) in self.extra.iter().enumerate() {
                self.inline[i] = span;
            }
            self.extra.clear();
        }
    }

    #[inline]
    fn holds(&self, from: usize, to: usize) -> bool {
        if !self.extra.is_empty() {
            let idx = self.extra.partition_point(|&(a, _)| a <= from);
            return idx > 0 && self.extra[idx - 1].1 >= to;
        }
        (0..usize::from(self.len)).any(|i| self.inline[i].0 <= from && to <= self.inline[i].1)
    }
}

#[derive(Default)]
pub(crate) struct Had(pub(super) Vec<(u64, FastMap<(u64, usize), BlockSpans>)>);

impl Had {
    /// Flights from the one behind which `fence` is signalled on are landed
    /// next: what is of no use to them is dropped.
    pub(crate) fn landing(&mut self, fence: u64) {
        self.0.retain(|had| had.0 >= fence);
    }

    /// A fetch that ran behind every flight up to `fence` brought (copy,
    /// title address, bytes) `pieces`. They are merged before the next ask.
    pub(crate) fn fetched(&mut self, fence: u64, pieces: impl Iterator<Item = Piece>) {
        if let Some((_, existing)) = self.0.last_mut().filter(|(f, _)| *f == fence) {
            for (copy, at, len) in pieces {
                existing.entry((copy, at / BLOCK)).or_default().insert(at, at + len);
            }
            return;
        }
        let mut all: FastMap<(u64, usize), BlockSpans> = FastMap::default();
        for (copy, at, len) in pieces {
            all.entry((copy, at / BLOCK)).or_default().insert(at, at + len);
        }
        if !all.is_empty() {
            self.0.push((fence, all));
        }
    }

    /// Whether a fetch behind the flight of `fence` brought all of the
    /// piece.
    pub(crate) fn holds(&self, fence: u64, (copy, at, len): Piece) -> bool {
        self.0.iter().rev().any(|(behind, all)| {
            *behind >= fence && all.get(&(copy, at / BLOCK)).is_some_and(|had| had.holds(at, at + len))
        })
    }
}
