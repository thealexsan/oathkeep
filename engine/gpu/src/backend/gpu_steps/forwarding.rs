use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlacedMap {
    dst_start: usize,
    dst_end: usize,
    src_start: usize,
}

/// Sorted non-overlapping active [`PlacedMap`] set with an `O(1)` bounding-box
/// `[min_dst, max_dst)` guard so steps outside placed destinations pay two
/// integer comparisons.
pub(super) struct ActivePlaced {
    entries: Vec<PlacedMap>,
    min_dst: usize,
    max_dst: usize,
}

impl Default for ActivePlaced {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            min_dst: usize::MAX,
            max_dst: 0,
        }
    }
}

/// Compact per-step forwarded read descriptor: avoids allocating per-step
/// vectors during [`plan_inner`], and materializes multi-segment reads only
/// when a step actually straddles an active [`PlacedMap`] boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fwd {
    /// Not a positive-length [`Does::Copy`] or takeable [`Does::Fill`].
    None,
    /// Unforwarded: reads its raw `(src, src + len)`.
    Raw,
    /// Fully covered by one active [`PlacedMap`]: reads `(mapped_src, mapped_src + len)`.
    Single(usize),
    /// Straddles active [`PlacedMap`] boundaries: `count` segments at `start` in `seg_pool`.
    Multi { start: u32, count: u32 },
}

impl ActivePlaced {
    #[inline]
    fn refresh_bounds(&mut self) {
        if let (Some(first), Some(last)) = (self.entries.first(), self.entries.last()) {
            self.min_dst = first.dst_start;
            self.max_dst = last.dst_end;
        } else {
            self.min_dst = usize::MAX;
            self.max_dst = 0;
        }
    }

    #[inline]
    fn push_coalesced(out: &mut Vec<PlacedMap>, dst_start: usize, dst_end: usize, src_start: usize) {
        if dst_start >= dst_end || dst_start == src_start {
            return;
        }
        if let Some(last) = out.last_mut()
            && last.dst_end == dst_start
            && last.src_start + (last.dst_end - last.dst_start) == src_start
        {
            last.dst_end = dst_end;
            return;
        }
        out.push(PlacedMap {
            dst_start,
            dst_end,
            src_start,
        });
    }

    fn coalesce_around(&mut self, from: usize, until: usize) {
        if self.entries.len() < 2 || from >= self.entries.len() {
            return;
        }
        let begin = from.max(1);
        let limit = until.min(self.entries.len());
        if begin >= limit {
            return;
        }
        let mut write = begin - 1;
        for read in begin..limit {
            let cur = self.entries[read];
            let prev = &mut self.entries[write];
            if prev.dst_end == cur.dst_start && prev.src_start + (prev.dst_end - prev.dst_start) == cur.src_start {
                prev.dst_end = cur.dst_end;
            } else {
                write += 1;
                self.entries[write] = cur;
            }
        }
        let kept_end = write + 1;
        if kept_end < limit {
            self.entries.drain(kept_end..limit);
        }
    }

    /// Resolves `[from, from + len)` against active placed destinations.
    #[inline]
    pub(super) fn resolve(&self, from: usize, len: usize, seg_pool: &mut Vec<(usize, usize, usize)>) -> Fwd {
        let to = from + len;
        if from >= self.max_dst || self.min_dst >= to {
            return Fwd::Raw;
        }
        let idx = self.entries.partition_point(|m| m.dst_end <= from);
        if idx == self.entries.len() || self.entries[idx].dst_start >= to {
            return Fwd::Raw;
        }
        let first = self.entries[idx];
        if first.dst_start <= from && to <= first.dst_end {
            let mapped = first.src_start + (from - first.dst_start);
            return if mapped == from { Fwd::Raw } else { Fwd::Single(mapped) };
        }
        let start_idx = seg_pool.len();
        let push_seg = |pool: &mut Vec<(usize, usize, usize)>, off: usize, src: usize, n: usize| {
            if n == 0 {
                return;
            }
            if pool.len() > start_idx
                && let Some(last) = pool.last_mut()
                && last.0 + last.2 == off
                && last.1 + last.2 == src
            {
                last.2 += n;
                return;
            }
            pool.push((off, src, n));
        };
        let mut cur = from;
        for &m in &self.entries[idx..] {
            if m.dst_start >= to {
                break;
            }
            if cur < m.dst_start {
                let gap = m.dst_start - cur;
                push_seg(seg_pool, cur - from, cur, gap);
                cur = m.dst_start;
            }
            let hi = to.min(m.dst_end);
            let n = hi - cur;
            let mapped = m.src_start + (cur - m.dst_start);
            push_seg(seg_pool, cur - from, mapped, n);
            cur = hi;
        }
        if cur < to {
            push_seg(seg_pool, cur - from, cur, to - cur);
        }
        let count = seg_pool.len() - start_idx;
        if count == 1 {
            let (_, mapped, _) = seg_pool.pop().unwrap_or((0, from, len));
            if mapped == from { Fwd::Raw } else { Fwd::Single(mapped) }
        } else {
            Fwd::Multi {
                start: start_idx as u32,
                count: count as u32,
            }
        }
    }

    /// Splits or removes active placed mappings overwritten by a [`Does::Copy`]
    /// into `[dst, dst + len)` so subsequent steps lock onto the copy instead
    /// of forwarding past it.
    pub(super) fn invalidate(&mut self, dst: usize, len: usize) {
        let to = dst + len;
        if dst >= self.max_dst || self.min_dst >= to {
            return;
        }
        let start = self.entries.partition_point(|m| m.dst_end <= dst);
        if start == self.entries.len() || self.entries[start].dst_start >= to {
            return;
        }
        let end = start + self.entries[start..].partition_point(|m| m.dst_start < to);
        let left = (self.entries[start].dst_start < dst).then(|| PlacedMap {
            dst_start: self.entries[start].dst_start,
            dst_end: dst,
            src_start: self.entries[start].src_start,
        });
        let right = (self.entries[end - 1].dst_end > to).then(|| {
            let last = self.entries[end - 1];
            PlacedMap {
                dst_start: to,
                dst_end: last.dst_end,
                src_start: last.src_start + (to - last.dst_start),
            }
        });
        self.entries.splice(start..end, left.into_iter().chain(right));
        self.refresh_bounds();
    }

    /// Records `Placed { dst, src, len }` by chaining `[src, src + len)` through
    /// any currently active mappings and installing the resulting mappings over
    /// `[dst, dst + len)`.
    pub(super) fn put_placed(&mut self, dst: usize, src: usize, len: usize, scratch: &mut Vec<PlacedMap>) {
        if len == 0 || dst == src {
            return;
        }
        scratch.clear();
        let src_end = src + len;
        if src >= self.max_dst || self.min_dst >= src_end {
            scratch.push(PlacedMap {
                dst_start: dst,
                dst_end: dst + len,
                src_start: src,
            });
        } else {
            let idx = self.entries.partition_point(|m| m.dst_end <= src);
            if idx == self.entries.len() || self.entries[idx].dst_start >= src_end {
                scratch.push(PlacedMap {
                    dst_start: dst,
                    dst_end: dst + len,
                    src_start: src,
                });
            } else if self.entries[idx].dst_start <= src && src_end <= self.entries[idx].dst_end {
                let mapped_src = self.entries[idx].src_start + (src - self.entries[idx].dst_start);
                if mapped_src != dst {
                    scratch.push(PlacedMap {
                        dst_start: dst,
                        dst_end: dst + len,
                        src_start: mapped_src,
                    });
                }
            } else {
                let mut cur = src;
                for &m in &self.entries[idx..] {
                    if m.dst_start >= src_end {
                        break;
                    }
                    if cur < m.dst_start {
                        let n = m.dst_start - cur;
                        let d_lo = dst + (cur - src);
                        Self::push_coalesced(scratch, d_lo, d_lo + n, cur);
                        cur = m.dst_start;
                    }
                    let hi = src_end.min(m.dst_end);
                    let n = hi - cur;
                    let d_lo = dst + (cur - src);
                    let mapped_src = m.src_start + (cur - m.dst_start);
                    Self::push_coalesced(scratch, d_lo, d_lo + n, mapped_src);
                    cur = hi;
                }
                if cur < src_end {
                    let n = src_end - cur;
                    let d_lo = dst + (cur - src);
                    Self::push_coalesced(scratch, d_lo, d_lo + n, cur);
                }
            }
        }
        let dst_end = dst + len;
        if dst >= self.max_dst {
            for &piece in scratch.iter() {
                Self::push_coalesced(&mut self.entries, piece.dst_start, piece.dst_end, piece.src_start);
            }
            self.refresh_bounds();
            return;
        }
        if dst_end <= self.min_dst && !scratch.is_empty() {
            let inserted = scratch.len();
            self.entries.splice(0..0, scratch.iter().copied());
            self.coalesce_around(0, inserted + 1);
            self.refresh_bounds();
            return;
        }
        let start = self.entries.partition_point(|m| m.dst_end <= dst);
        let end = start + self.entries[start..].partition_point(|m| m.dst_start < dst_end);
        let left = (start < end && self.entries[start].dst_start < dst).then(|| PlacedMap {
            dst_start: self.entries[start].dst_start,
            dst_end: dst,
            src_start: self.entries[start].src_start,
        });
        let right = (start < end && self.entries[end - 1].dst_end > dst_end).then(|| {
            let last = self.entries[end - 1];
            PlacedMap {
                dst_start: dst_end,
                dst_end: last.dst_end,
                src_start: last.src_start + (dst_end - last.dst_start),
            }
        });
        let inserted = usize::from(left.is_some()) + scratch.len() + usize::from(right.is_some());
        self.entries
            .splice(start..end, left.into_iter().chain(scratch.iter().copied()).chain(right));
        self.coalesce_around(start.saturating_sub(1), start + inserted + 1);
        self.refresh_bounds();
    }
}

/// Forwarded read resolution across a submission's steps. Empty when `!forward`
/// or when no active [`Does::Placed`] steps exist.
pub(super) struct Forwarding {
    pub(super) steps: Vec<Fwd>,
    pub(super) seg_pool: Vec<(usize, usize, usize)>,
}

impl Forwarding {
    pub(super) fn build(does: &[Does], forward: bool, has_placed: bool) -> Self {
        if !forward || !has_placed {
            return Self {
                steps: Vec::new(),
                seg_pool: Vec::new(),
            };
        }
        let mut steps = vec![Fwd::None; does.len()];
        let mut seg_pool = Vec::new();
        let mut active = ActivePlaced::default();
        let mut scratch = Vec::new();
        for (i, d) in does.iter().enumerate() {
            match *d {
                Does::Copy { dst, src, len, .. } => {
                    if len > 0 {
                        steps[i] = active.resolve(src, len, &mut seg_pool);
                        active.invalidate(dst, len);
                    }
                }
                Does::Fill { src, len, to: true, .. } => {
                    if len > 0 {
                        steps[i] = active.resolve(src, len, &mut seg_pool);
                    }
                }
                Does::Placed { dst, src, len, .. } => {
                    if len > 0 && dst != src {
                        active.put_placed(dst, src, len, &mut scratch);
                    }
                }
                _ => {}
            }
        }
        Self { steps, seg_pool }
    }

    /// Zero-allocation view of `(offset_in_step, src_address, bytes)` segments
    /// and write range for step `i`.
    #[inline]
    pub(super) fn segments_of<'a>(
        &'a self,
        does: &[Does],
        i: usize,
        single_buf: &'a mut [(usize, usize, usize); 1],
    ) -> Option<(&'a [(usize, usize, usize)], (usize, usize))> {
        let (r, w) = reads_writes(&does[i])?;
        let len = r.1 - r.0;
        if self.steps.is_empty() || len == 0 {
            single_buf[0] = (0, r.0, len);
            return Some((&single_buf[..], w));
        }
        match self.steps[i] {
            Fwd::None | Fwd::Raw => {
                single_buf[0] = (0, r.0, len);
                Some((&single_buf[..], w))
            }
            Fwd::Single(mapped) => {
                single_buf[0] = (0, mapped, len);
                Some((&single_buf[..], w))
            }
            Fwd::Multi { start, count } => {
                let s = start as usize;
                Some((&self.seg_pool[s..s + count as usize], w))
            }
        }
    }
}
