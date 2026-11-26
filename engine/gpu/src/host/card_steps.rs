//! Steps of a submission that read title memory flights in the air may
//! still write, done on the card in the flights' order instead of on the
//! CPU after a wait for them (`OATHKEEP_CARD_STEPS`, see [`stage`]).

use std::sync::Arc;

use super::*;
use crate::shadow::Shadow;

/// The stage (`OATHKEEP_CARD_STEPS`; off by default): `1` constant buffers
/// of descriptor tables (`Step::Fill` with a slot of its own), `2` the
/// title's buffer copies (`Step::Copy`) too, with the steps of the same
/// submission that depend on them. Only with the relay's second stage
/// (`relay_exact`).
///
/// Why: eleven dispatches a frame (the skinning passes, in two early
/// submissions) write through a view of the title's whole 174 MB heap, at
/// places they read out of job records in buffers: no bound of what they
/// write is known before they ran (frame capture replay2: 17af15c4015cb4b6,
/// 55679a0547649d0b, 89d08e561897d4e5, fdd56d97a806bcca and three more,
/// all through one view at offset 5920 of the title's bindless table). The
/// title's ring of constants lies in the same heap. One constant buffer of
/// 480 bytes and one copy of 20 KiB a frame, at a new place of the ring
/// each time, waited for these passes to land: 25-30 ms of a 105 ms frame,
/// the submitting thread standing (run record-1; with `OATHKEEP_FILL_TITLE=1`
/// in run relaycopy-1 the copy alone took over the wait).
///
/// What is done: a list of ours goes to the card in front of the
/// submission's lists, in the same call. It copies the bytes read from the
/// host copy that the flights write into a buffer of ours, and from there
/// to the destination's host copy (a copy), or the fill's descriptor slot
/// is pointed at that buffer (a fill; the slot is the fill's alone, the
/// root signatures say such descriptors are volatile, and the list that
/// names it has not gone to the card).
///
/// Why that is the value the CPU would have read after the wait. After the
/// wait title memory holds, for every byte read, what the title wrote, or
/// what the last of the flights that wrote it wrote (the landings put it
/// there, in order). The host copy on the card holds the same when our list
/// runs: every flight that may write one of these bytes writes this same
/// host copy ([`holder`]: a write through another host copy, a read-back
/// or a value written behind a flight there, and the old wait is taken),
/// they are all ahead of our list on the queue, and the title's own bytes
/// are brought up to date by a refresh step of our list, which is prepared
/// like any other list's (also under flights' declared writes:
/// `stale_under_flights`, which settles if it cannot). Nothing of this
/// submission writes these bytes earlier than our list: the steps are done
/// in front of all its lists in either case, and among themselves in the
/// order they stand in wherever one depends on another: [`plan`] takes
/// every copy and fill of the submission that writes what a taken one
/// reads or writes, or reads what a taken one writes. Every draw that
/// reads a buffer view of the heap relies on the same refresh and the same
/// order; what is new is only that a constant buffer and a copy do too.
/// Where a page is not mapped the host copy may hold other bytes than the
/// CPU's copy would have left: refused.
///
/// For a copy's destination our list declares a shader write, exactly the
/// bytes copied, after a refresh of them: from there on it is what any
/// shader's write is. The relay compares, and puts what changed into title
/// memory before the title is told anything of this flight (its
/// bottom-of-pipe writes and fences come after, in order); CPU steps of
/// later submissions that read it find it declared and wait, or are done
/// here. Refused: a step of the submission that cannot be taken (a texture,
/// vertices expanded, a fill of memory of another kind) reads the
/// destination behind the copy, or a refresh brings another host copy of it
/// up to date ([`plan`], [`elsewhere`]). While such a copy is in the air a
/// submission that refreshes another host copy of its destination, or
/// reads it in a step that is not checked otherwise, waits for it
/// ([`copied_read`]); with the copy done on the CPU it was there at once.
///
/// What stage 2 does not see: code of ours that reads title memory away
/// from a submission's steps (the pass over textures picked by index, while
/// a list is recorded). With the CPU's copy such a read of the destination
/// found the bytes at once; now it finds them when the flight has landed.
/// The landing tells the guards (`crate::watch::landing`), so a texture
/// made from older bytes is made again. This is why stage 2 is its own.
pub(super) fn stage() -> u32 {
    static STAGE: OnceLock<u32> = OnceLock::new();
    *STAGE.get_or_init(|| match std::env::var("OATHKEEP_CARD_STEPS").ok().as_deref() {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    })
}

/// Whether a copy at its place in a list ([`place`]) is only in the way of
/// what is done on the card in front of the lists if that stands behind it in
/// them (on by default; `OATHKEEP_PLACED_ORDER=0`: wherever it stands), and a
/// submission where it does is cut behind the copy's list ([`cut_for`]).
///
/// The fills of constant buffers read the title's ring (7,500 a frame in the
/// opening), and one copy of a few hundred bytes a frame writes into it at its
/// place in a list. The whole submission was refused for it, and stood 40 to
/// 60 ms until the flights had landed (run n19).
fn placed_order() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("OATHKEEP_PLACED_ORDER") {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_PLACED_ORDER").is_none_or(|v| v != "0"))
}

/// Why [`plan`] refuses for [`place`].
const BEHIND_PLACED: &str = "a copy at its place in a list writes what it reads, or reads or writes what it writes";

/// The bytes of our buffer between reading and writing: what one submission
/// may do this way (each place read once, however many fills take it).
const BETWEEN: u64 = 4 << 20;

/// The most steps of a submission done this way.
const MOST: usize = 12288;

/// How a fill's bytes get where its list reads them, if the card brings
/// them.
#[derive(Clone)]
pub(super) enum FillTo {
    /// A descriptor slot that shows the fill's memory, and nothing else does
    /// (title heap, slot): it is made to show our buffer instead.
    Slot(usize, u32),
    /// A buffer the card can write, filled from its beginning.
    Buffer(ID3D12Resource),
}

/// Title memory something in the air writes: the host copy it goes through
/// (`None`: none, the bytes reach title memory when the flight lands), from,
/// to, and whether it is the destination of a copy done here.
pub(super) struct Writer {
    shadow: Option<Arc<Shadow>>,
    from: usize,
    to: usize,
    copy: bool,
}

/// What `items` of the line write.
pub(super) fn writers(items: &[Late]) -> Vec<Writer> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Late::Run(flight) => {
                for (shadow, ranges) in &flight.back {
                    out.extend(ranges.iter().map(|&(from, to)| Writer {
                        shadow: Some(shadow.clone()),
                        from,
                        to,
                        copy: false,
                    }));
                }
                for &i in &flight.order {
                    for &(_, at, [row, rows, pitch]) in &flight.lists[i].host().deferred().backs {
                        out.push(Writer {
                            shadow: None,
                            from: at,
                            to: at + rows.saturating_sub(1) * pitch + row,
                            copy: false,
                        });
                    }
                }
                out.extend(flight.own.iter().flat_map(Taken::copied));
            }
            Late::Write(at, _, bytes) => out.push(Writer {
                shadow: None,
                from: *at,
                to: *at + *bytes,
                copy: false,
            }),
            Late::Signal(..) => {}
        }
    }
    out
}

/// The destinations of copies done here that have not landed.
pub(super) fn copies(line: &Line) -> Vec<Writer> {
    if stage() < 2 {
        return Vec::new();
    }
    let flying = line.line.iter().filter_map(|l| match l {
        Late::Run(flight) => flight.own.as_ref(),
        _ => None,
    });
    let landing = line.landing.iter().filter(|w| w.copy).map(|w| Writer {
        shadow: w.shadow.clone(),
        ..*w
    });
    flying.flat_map(Taken::copied).chain(landing).collect()
}

/// Whether `runs` refresh another host copy of a destination in `copies`,
/// or read it in a step that flights are not waited for otherwise
/// (`Step::Host`): that reads title memory, where the copy is not yet.
pub(super) fn copied_read(copies: &[Writer], runs: &[&HostList]) -> bool {
    if copies.is_empty() {
        return false;
    }
    let mut min_from = usize::MAX;
    let mut max_to = 0usize;
    let mut only_shadow: Option<usize> = None;
    let mut single_shadow = true;
    for w in copies {
        if w.from < w.to {
            min_from = min_from.min(w.from);
            max_to = max_to.max(w.to);
        }
        match (&w.shadow, only_shadow) {
            (Some(s), None) if single_shadow => only_shadow = Some(Arc::as_ptr(s) as usize),
            (Some(s), Some(prev)) if single_shadow && Arc::as_ptr(s) as usize == prev => {}
            _ => {
                single_shadow = false;
                only_shadow = None;
            }
        }
    }
    if min_from >= max_to {
        return false;
    }
    runs.iter().any(|list| {
        list.deferred().copies.iter().any(|step| match step {
            Step::Refresh(shadow, at, len) => {
                let end = *at + *len;
                if *at >= max_to || min_from >= end {
                    return false;
                }
                if let Some(common) = only_shadow
                    && Arc::as_ptr(shadow) as usize == common
                {
                    return false;
                }
                copies
                    .iter()
                    .any(|w| w.from < end && *at < w.to && w.shadow.as_ref().is_none_or(|s| !Arc::ptr_eq(s, shadow)))
            }
            Step::Host(_, (from, to)) => {
                *from < max_to && min_from < *to && copies.iter().any(|w| w.from < *to && *from < w.to)
            }
            Step::Plane((at, len), _) => {
                let end = at.saturating_add(*len);
                *at < max_to && min_from < end && copies.iter().any(|w| w.from < end && *at < w.to)
            }
            _ => false,
        })
    })
}

/// Inserts `[from, to)` into sorted, non-overlapping, non-touching `ranges` in
/// `O(log R)` without reallocating.
fn insert_range(ranges: &mut Vec<(usize, usize)>, from: usize, to: usize) {
    if from >= to {
        return;
    }
    if let Some(last) = ranges.last_mut() {
        if from > last.1 {
            ranges.push((from, to));
            return;
        }
        if from >= last.0 {
            last.1 = last.1.max(to);
            return;
        }
    } else {
        ranges.push((from, to));
        return;
    }
    let start = ranges.partition_point(|r| r.1 < from);
    if start == ranges.len() {
        ranges.push((from, to));
        return;
    }
    if to < ranges[start].0 {
        ranges.insert(start, (from, to));
        return;
    }
    let end = start + ranges[start..].partition_point(|r| r.0 <= to);
    let merged_from = from.min(ranges[start].0);
    let merged_to = to.max(ranges[end - 1].1);
    ranges[start] = (merged_from, merged_to);
    if end > start + 1 {
        ranges.drain(start + 1..end);
    }
}

/// What a step of a submission does to title memory, as the plan sees it.
/// `asked`: flights in the air stop it from being done on the CPU now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Does {
    Copy { dst: usize, src: usize, len: usize, asked: bool },
    /// `to`: its memory can be given the bytes on the card.
    Fill { src: usize, len: usize, asked: bool, to: bool },
    /// Another read on the CPU (a texture, vertices expanded).
    Read { from: usize, to: usize, asked: bool },
    /// A refresh of a host copy (named by its address).
    #[cfg(test)]
    Refresh { shadow: usize, from: usize, to: usize },
    /// A copy that its list does on the card ([`place`]).
    /// `cpu_src`: the list copies from upload scratch filled on the CPU.
    /// `waits`: flights in the air write that CPU source.
    Placed { dst: usize, src: usize, len: usize, cpu_src: bool, waits: bool },
    /// Anything, anywhere.
    Run,
}

#[inline]
fn meet(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The bytes `d` reads and writes as a copy or a fill (empty ranges for
/// what it does not).
#[inline]
fn reads_writes(d: &Does) -> Option<((usize, usize), (usize, usize))> {
    match *d {
        Does::Copy { dst, src, len, .. } => Some(((src, src + len), (dst, dst + len))),
        Does::Fill { src, len, .. } => Some(((src, src + len), (0, 0))),
        _ => None,
    }
}

#[inline]
fn touched(ranges: &[(usize, usize)], r: (usize, usize)) -> bool {
    touches(ranges, r.0, r.1)
}

/// One active non-overlapping `[dst_start, dst_end) -> src_start` mapping
/// produced by earlier [`Does::Placed`] steps in the same submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlacedMap {
    dst_start: usize,
    dst_end: usize,
    src_start: usize,
}

/// Sorted non-overlapping active [`PlacedMap`] set with an `O(1)` bounding-box
/// `[min_dst, max_dst)` guard so steps outside placed destinations pay two
/// integer comparisons.
struct ActivePlaced {
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
enum Fwd {
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
    fn resolve(&self, from: usize, len: usize, seg_pool: &mut Vec<(usize, usize, usize)>) -> Fwd {
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
    fn invalidate(&mut self, dst: usize, len: usize) {
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
    fn put_placed(&mut self, dst: usize, src: usize, len: usize, scratch: &mut Vec<PlacedMap>) {
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
struct Forwarding {
    steps: Vec<Fwd>,
    seg_pool: Vec<(usize, usize, usize)>,
}

impl Forwarding {
    fn build(does: &[Does], forward: bool, has_placed: bool) -> Self {
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
    fn segments_of<'a>(
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

/// Bottom-up iterative range-chmax / range-max segment tree over compressed
/// interval endpoints for `O(N log N)` level assignment.
struct LevelSegTree {
    size: usize,
    sub: Vec<u32>,
    lazy: Vec<u32>,
}

impl LevelSegTree {
    fn new(num_leaves: usize) -> Self {
        let size = num_leaves.max(1).next_power_of_two();
        Self {
            size,
            sub: vec![0; 2 * size],
            lazy: vec![0; 2 * size],
        }
    }

    #[inline]
    fn query_max(&self, l0: usize, r0: usize) -> u32 {
        if l0 >= r0 {
            return 0;
        }
        let mut ans = 0u32;
        let (mut l, mut r) = (l0 + self.size, r0 + self.size);
        while l < r {
            if l & 1 == 1 {
                ans = ans.max(self.sub[l]);
                l += 1;
            }
            if r & 1 == 1 {
                r -= 1;
                ans = ans.max(self.sub[r]);
            }
            l >>= 1;
            r >>= 1;
        }
        let mut pl = (l0 + self.size) >> 1;
        let mut pr = (r0 - 1 + self.size) >> 1;
        while pl > 0 {
            ans = ans.max(self.lazy[pl]);
            pl >>= 1;
        }
        while pr > 0 {
            ans = ans.max(self.lazy[pr]);
            pr >>= 1;
        }
        ans
    }

    #[inline]
    fn chmax_range(&mut self, l0: usize, r0: usize, val: u32) {
        if l0 >= r0 || val == 0 {
            return;
        }
        let (mut l, mut r) = (l0 + self.size, r0 + self.size);
        while l < r {
            if l & 1 == 1 {
                self.lazy[l] = self.lazy[l].max(val);
                self.sub[l] = self.sub[l].max(val);
                l += 1;
            }
            if r & 1 == 1 {
                r -= 1;
                self.lazy[r] = self.lazy[r].max(val);
                self.sub[r] = self.sub[r].max(val);
            }
            l >>= 1;
            r >>= 1;
        }
        let mut pl = (l0 + self.size) >> 1;
        let mut pr = (r0 - 1 + self.size) >> 1;
        while pl > 0 {
            self.sub[pl] = self.sub[pl].max(val);
            pl >>= 1;
        }
        while pr > 0 {
            self.sub[pr] = self.sub[pr].max(val);
            pr >>= 1;
        }
    }
}

fn assign_levels_direct(does: &[Does], fwd: &Forwarding, order: &[usize]) -> Vec<(usize, u32)> {
    let mut out: Vec<(usize, u32)> = Vec::with_capacity(order.len());
    let mut copies: Vec<usize> = Vec::new();
    let mut i_buf = [(0usize, 0usize, 0usize); 1];
    let mut j_buf = [(0usize, 0usize, 0usize); 1];
    for &i in order {
        let Some((segs, w)) = fwd.segments_of(does, i, &mut i_buf) else {
            continue;
        };
        let writes = w.0 < w.1;
        let mut level = 0u32;
        if writes {
            for &(j, at) in &out {
                if at < level {
                    continue;
                }
                let Some((j_segs, jw)) = fwd.segments_of(does, j, &mut j_buf) else {
                    continue;
                };
                let dep = (jw.0 < jw.1 && (meet(jw, w) || segs.iter().any(|&(_, a, n)| meet(jw, (a, a + n)))))
                    || j_segs.iter().any(|&(_, a, n)| meet((a, a + n), w));
                if dep {
                    level = at + 1;
                }
            }
            copies.push(out.len());
        } else {
            for &k in &copies {
                let (j, at) = out[k];
                if at < level {
                    continue;
                }
                let Some((_, jw)) = reads_writes(&does[j]) else {
                    continue;
                };
                if jw.0 < jw.1 && segs.iter().any(|&(_, a, n)| meet(jw, (a, a + n))) {
                    level = at + 1;
                }
            }
        }
        out.push((i, level));
    }
    out
}

fn assign_levels_segtree(does: &[Does], fwd: &Forwarding, order: &[usize], mut copies_left: usize) -> Vec<(usize, u32)> {
    let mut coords: Vec<usize> = Vec::with_capacity(order.len() * 2 + copies_left * 2);
    let mut single_buf = [(0usize, 0usize, 0usize); 1];
    for &i in order {
        let Some((segs, w)) = fwd.segments_of(does, i, &mut single_buf) else {
            continue;
        };
        for &(_, a, n) in segs {
            if n > 0 {
                coords.push(a);
                coords.push(a + n);
            }
        }
        if w.0 < w.1 {
            coords.push(w.0);
            coords.push(w.1);
        }
    }
    coords.sort_unstable();
    coords.dedup();
    if coords.len() < 2 {
        return order.iter().map(|&i| (i, 0)).collect();
    }
    let idx_of = |addr: usize| coords.binary_search(&addr).unwrap_or(0);
    let num_leaves = coords.len() - 1;
    let mut write_tree = LevelSegTree::new(num_leaves);
    let mut read_tree = LevelSegTree::new(num_leaves);
    let mut out: Vec<(usize, u32)> = Vec::with_capacity(order.len());
    for &i in order {
        let Some((segs, w)) = fwd.segments_of(does, i, &mut single_buf) else {
            continue;
        };
        let writes = w.0 < w.1;
        let mut level = 0u32;
        if segs.len() == 1 {
            let (_, a, n) = segs[0];
            let (rl, rr) = if n > 0 { (idx_of(a), idx_of(a + n)) } else { (0, 0) };
            if rl < rr {
                level = level.max(write_tree.query_max(rl, rr));
            }
            if writes {
                let (wl, wr) = (idx_of(w.0), idx_of(w.1));
                level = level.max(write_tree.query_max(wl, wr));
                level = level.max(read_tree.query_max(wl, wr));
                copies_left -= 1;
                let next = level + 1;
                if copies_left > 0 && rl < rr {
                    read_tree.chmax_range(rl, rr, next);
                }
                write_tree.chmax_range(wl, wr, next);
            } else if copies_left > 0 && rl < rr {
                read_tree.chmax_range(rl, rr, level + 1);
            }
        } else {
            for &(_, a, n) in segs {
                if n > 0 {
                    let (rl, rr) = (idx_of(a), idx_of(a + n));
                    level = level.max(write_tree.query_max(rl, rr));
                }
            }
            let (wl, wr) = if writes { (idx_of(w.0), idx_of(w.1)) } else { (0, 0) };
            if writes {
                level = level.max(write_tree.query_max(wl, wr));
                level = level.max(read_tree.query_max(wl, wr));
                copies_left -= 1;
            }
            let next = level + 1;
            if copies_left > 0 {
                for &(_, a, n) in segs {
                    if n > 0 {
                        let (rl, rr) = (idx_of(a), idx_of(a + n));
                        read_tree.chmax_range(rl, rr, next);
                    }
                }
            }
            if writes {
                write_tree.chmax_range(wl, wr, next);
            }
        }
        out.push((i, level));
    }
    out
}

/// Optional cross-list cutting metadata for [`cut_for`]: `(steps, run_starts, splittable)`
/// plus output `min_cut`.
struct CutQuery<'a> {
    steps: &'a [(usize, usize, usize)],
    run_starts: &'a [usize],
    splittable: &'a [bool],
    min_cut: Option<usize>,
}

/// The steps of `does` (by place) to be done on the card, in order, each
/// with its level, plus the forwarding table; or why not.
///
/// `forward`: when `by_order` is on and cross-list cuts have already been
/// taken, steps behind a [`Does::Placed`] forward the overlapped sub-ranges of
/// their reads to that placed copy's source instead of forcing a CPU flight
/// wait, while leaving the placed copy's destination buffer untouched ahead of
/// the list.
fn plan_inner(
    does: &[Does],
    stage: u32,
    by_order: bool,
    forward: bool,
    behind_first: &mut Option<(usize, usize)>,
    mut cut_query: Option<&mut CutQuery<'_>>,
) -> Result<(Vec<(usize, u32)>, Forwarding), &'static str> {
    *behind_first = None;
    if let Some(q) = cut_query.as_deref_mut() {
        q.min_cut = None;
    }
    let forward = by_order && forward;
    let takes = |d: &Does| match *d {
        Does::Copy { .. } if stage < 2 => Err("a buffer copy (OATHKEEP_CARD_STEPS=2 does it on the card)"),
        Does::Fill { to: false, .. } => Err("a fill of memory the card cannot be given the bytes of"),
        _ => Ok(()),
    };
    let mut more = Vec::new();
    let mut copy_indices = Vec::new();
    let mut placed_indices = Vec::new();
    let mut has_active_placed = false;
    for (i, d) in does.iter().enumerate() {
        match *d {
            Does::Run => return Err("a step of ours runs"),
            Does::Read { asked: true, .. } => return Err("a texture reads it"),
            Does::Copy { asked, .. } => {
                copy_indices.push(i);
                if asked {
                    more.push(i);
                }
            }
            Does::Fill { asked: true, .. } => more.push(i),
            Does::Placed {
                dst,
                src,
                len,
                waits,
                ..
            } => {
                if waits {
                    return Err(BEHIND_PLACED);
                }
                if forward && dst != src && len > 0 && meet((dst, dst + len), (src, src + len)) {
                    return Err(BEHIND_PLACED);
                }
                if len > 0 {
                    placed_indices.push(i);
                    if dst != src {
                        has_active_placed = true;
                    }
                }
            }
            _ => {}
        }
    }
    let fwd = Forwarding::build(does, forward, has_active_placed);
    let mut taken = vec![false; does.len()];
    let mut taken_count = 0usize;
    let (mut read, mut written) = (Vec::new(), Vec::new());
    let mut single_buf = [(0usize, 0usize, 0usize); 1];
    while !more.is_empty() {
        // (Joined once for the round: one by one the 2,800 fills of a frame
        // sorted the ranges each time, 16 ms of the submitting thread.)
        let mut reads = Vec::with_capacity(more.len());
        let mut writes = Vec::new();
        for i in std::mem::take(&mut more) {
            if taken[i] {
                continue;
            }
            takes(&does[i])?;
            taken[i] = true;
            taken_count += 1;
            if let Some((segs, w)) = fwd.segments_of(does, i, &mut single_buf) {
                for &(_, a, n) in segs {
                    let r = (a, a + n);
                    if w.0 < w.1 && meet(r, w) {
                        return Err("a copy onto itself");
                    }
                    if r.0 < r.1 {
                        reads.push(r);
                    }
                }
                if w.0 < w.1 {
                    writes.push(w);
                }
            }
        }
        if taken_count > MOST {
            return Err("too many steps depend on it");
        }
        let mut delta_read = Vec::new();
        join_ranges(&mut delta_read, reads);
        for &(a, b) in &delta_read {
            insert_range(&mut read, a, b);
        }
        let wrote_any = !writes.is_empty();
        let mut delta_written = Vec::new();
        if wrote_any {
            join_ranges(&mut delta_written, writes);
            for &(a, b) in &delta_written {
                insert_range(&mut written, a, b);
            }
        }
        if !wrote_any {
            // Neither `written` nor `stale_cpu` changed; only `Does::Copy`
            // steps can newly trigger via `touched(&delta_read, w)`.
            if let (Some(first), Some(last)) = (delta_read.first(), delta_read.last()) {
                let (dr_min, dr_max) = (first.0, last.1);
                for &i in &copy_indices {
                    if !taken[i]
                        && let Does::Copy { dst, len, .. } = does[i]
                        && dst < dr_max
                        && dr_min < dst + len
                        && touched(&delta_read, (dst, dst + len))
                    {
                        more.push(i);
                    }
                }
            }
            continue;
        }
        let placed_propagates = placed_indices.iter().any(|&p| {
            matches!(does[p], Does::Placed { dst, src, len, .. }
                if touched(&written, (src, src + len)) || touched(&written, (dst, dst + len)))
        });
        if !placed_propagates {
            let (dw_min, dw_max) = (delta_written[0].0, delta_written[delta_written.len() - 1].1);
            let touches_dw = |r: (usize, usize)| r.0 < dw_max && dw_min < r.1 && touched(&delta_written, r);
            for (i, d) in does.iter().enumerate() {
                if taken[i] || matches!(d, Does::Fill { to: false, .. }) {
                    continue;
                }
                if let Some((segs, w)) = fwd.segments_of(does, i, &mut single_buf) {
                    let raw_r = reads_writes(d).map(|(r, _)| r).unwrap_or((0, 0));
                    if segs.iter().any(|&(_, a, n)| touches_dw((a, a + n)))
                        || (!fwd.steps.is_empty() && touches_dw(raw_r))
                        || (w.0 < w.1 && (touches_dw(w) || touched(&delta_read, w)))
                    {
                        more.push(i);
                    }
                }
            }
        } else {
            let mut stale_cpu = written.clone();
            for (i, d) in does.iter().enumerate() {
                if let Does::Placed { dst, src, len, .. } = *d
                    && len > 0
                    && (touched(&stale_cpu, (src, src + len)) || touched(&stale_cpu, (dst, dst + len)))
                {
                    insert_range(&mut stale_cpu, dst, dst + len);
                    continue;
                }
                if taken[i] || matches!(d, Does::Fill { to: false, .. }) {
                    continue;
                }
                if let Some((segs, w)) = fwd.segments_of(does, i, &mut single_buf) {
                    let raw_r = reads_writes(d).map(|(r, _)| r).unwrap_or((0, 0));
                    if segs.iter().any(|&(_, a, n)| touched(&written, (a, a + n)))
                        || touched(&stale_cpu, raw_r)
                        || (w.0 < w.1 && (touched(&written, w) || touched(&read, w)))
                    {
                        more.push(i);
                    }
                }
            }
        }
    }
    let taken_copies: Vec<usize> = copy_indices.iter().copied().filter(|&i| taken[i]).collect();
    // A copy at its place in a list comes after what is done in front of all
    // of them: without forwarding, any taken step behind it that reads its
    // destination or writes its source or destination stands in its way; with
    // forwarding, reads of its destination have been remapped to its source, so
    // only taken writes behind it that touch its source or destination do.
    if !placed_indices.is_empty() {
        if forward {
            if !taken_copies.is_empty() {
                for &p in &placed_indices {
                    let Does::Placed { dst, src, len, .. } = does[p] else {
                        continue;
                    };
                    let (to, from) = ((dst, dst + len), (src, src + len));
                    if touched(&written, to) || touched(&written, from) {
                        let start = taken_copies.partition_point(|&i| i <= p);
                        if let Some(&i) = taken_copies[start..].iter().find(|&&i| {
                            matches!(does[i], Does::Copy { dst: cd, len: cl, .. }
                                if cl > 0 && (meet((cd, cd + cl), to) || meet((cd, cd + cl), from)))
                        }) {
                            *behind_first = Some((p, i));
                            return Err(BEHIND_PLACED);
                        }
                    }
                }
            }
        } else {
            let mut any_conflict = false;
            for &p in &placed_indices {
                let Does::Placed { dst, src, len, .. } = does[p] else {
                    continue;
                };
                let (to, from) = ((dst, dst + len), (src, src + len));
                if !(touched(&read, to) || touched(&written, to) || touched(&written, from)) {
                    continue;
                }
                if !by_order {
                    return Err(BEHIND_PLACED);
                }
                let in_the_way = |i: usize| {
                    reads_writes(&does[i])
                        .is_some_and(|(r, w)| meet(r, to) || (w.0 < w.1 && (meet(w, to) || meet(w, from))))
                };
                if let Some(q) = cut_query.as_deref_mut() {
                    let cut = q.steps[p].0 + 1;
                    if cut < q.splittable.len() && q.splittable[cut] {
                        let start = q.run_starts[cut];
                        if let Some(i) = (start..does.len()).find(|&i| taken[i] && in_the_way(i)) {
                            // `p` is in non-decreasing `run_idx` order, so the
                            // first splittable cross-list conflict is minimal.
                            q.min_cut = Some(cut);
                            *behind_first = Some((p, i));
                            return Err(BEHIND_PLACED);
                        }
                    }
                    if !any_conflict
                        && let Some(i) = (p + 1..does.len()).find(|&i| taken[i] && in_the_way(i))
                    {
                        any_conflict = true;
                        *behind_first = Some((p, i));
                    }
                } else if let Some(i) = (p + 1..does.len()).find(|&i| taken[i] && in_the_way(i)) {
                    *behind_first = Some((p, i));
                    return Err(BEHIND_PLACED);
                }
            }
            if any_conflict {
                return Err(BEHIND_PLACED);
            }
        }
    }
    if taken_count == 0 {
        return Ok((Vec::new(), fwd));
    }
    // What stays on the CPU and reads what an earlier taken copy writes
    // would read it too early.
    if !taken_copies.is_empty() {
        let mut cpu_stale: Vec<(usize, usize)> = Vec::new();
        for (i, d) in does.iter().enumerate() {
            if taken[i] {
                if let Does::Copy { dst, len, .. } = *d
                    && len > 0
                {
                    insert_range(&mut cpu_stale, dst, dst + len);
                }
                continue;
            }
            match *d {
                Does::Placed {
                    dst,
                    src,
                    len,
                    cpu_src,
                    ..
                } if len > 0 && (touched(&cpu_stale, (src, src + len)) || touched(&cpu_stale, (dst, dst + len))) => {
                    if cpu_src && touched(&cpu_stale, (src, src + len)) {
                        return Err(BEHIND_PLACED);
                    }
                    insert_range(&mut cpu_stale, dst, dst + len);
                }
                Does::Read { from, to, .. } if touched(&cpu_stale, (from, to)) => {
                    return Err("a texture or expanded vertices read what it writes");
                }
                Does::Fill { src, len, .. } if touched(&cpu_stale, (src, src + len)) => {
                    return Err("a fill of memory the card cannot be given the bytes of reads what it writes");
                }
                _ => {}
            }
        }
    }
    let mut order = Vec::with_capacity(taken_count);
    for (i, &is_taken) in taken.iter().enumerate() {
        if is_taken {
            order.push(i);
        }
    }
    let out = if taken_copies.is_empty() {
        order.into_iter().map(|i| (i, 0)).collect()
    } else if taken_copies.len() * order.len() <= 2048 {
        assign_levels_direct(does, &fwd, &order)
    } else {
        assign_levels_segtree(does, &fwd, &order, taken_copies.len())
    };
    Ok((out, fwd))
}

#[cfg(test)]
fn plan(does: &[Does], stage: u32, by_order: bool, behind: &mut Option<(usize, usize)>) -> Result<Vec<(usize, u32)>, &'static str> {
    plan_inner(does, stage, by_order, false, behind, None).map(|(out, _)| out)
}

/// Merged per-shadow refresh ranges across a submission, built lazily only when
/// [`Host::take_steps`] actually takes a [`Does::Copy`].
struct RefreshSet {
    by_shadow: ByCopy<Vec<(usize, usize)>>,
    min_from: usize,
    max_to: usize,
}

impl RefreshSet {
    fn from_runs(runs: &[&HostList]) -> Self {
        let mut by_shadow: ByCopy<Vec<(usize, usize)>> = ByCopy::default();
        let mut min_from = usize::MAX;
        let mut max_to = 0usize;
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::Refresh(shadow, at, len) = step
                    && *len > 0
                {
                    let end = *at + *len;
                    min_from = min_from.min(*at);
                    max_to = max_to.max(end);
                    insert_range(by_shadow.entry(Arc::as_ptr(shadow) as usize).or_default(), *at, end);
                }
            }
        }
        Self {
            by_shadow,
            min_from,
            max_to,
        }
    }

    /// Whether any host copy other than `shadow` is refreshed in `[from, to)`.
    fn elsewhere(&self, shadow: usize, from: usize, to: usize) -> bool {
        if from >= to || from >= self.max_to || self.min_from >= to {
            return false;
        }
        self.by_shadow
            .iter()
            .any(|(&s, ranges)| s != shadow && touches(ranges, from, to))
    }
}

/// Whether a refresh of `does` brings a host copy other than `shadow` up to
/// date in `[from, to)`.
#[cfg(test)]
fn elsewhere(does: &[Does], shadow: usize, from: usize, to: usize) -> bool {
    does.iter().any(|d| match *d {
        Does::Refresh {
            shadow: s,
            from: a,
            to: b,
        } => s != shadow && meet((a, b), (from, to)),
        _ => false,
    })
}

/// The host copy that holds `[from, to)` on the card once what `writers`
/// write is there: the one they all write it through, or without any the
/// one shaders would be given.
fn holder(writers: &[Writer], from: usize, to: usize) -> Result<Arc<Shadow>, &'static str> {
    let mut found: Option<&Arc<Shadow>> = None;
    for w in writers.iter().filter(|w| meet((w.from, w.to), (from, to))) {
        match (&w.shadow, found) {
            (None, _) => return Err("a read-back or a value behind a flight is written there"),
            (Some(s), Some(f)) if !Arc::ptr_eq(s, f) => return Err("flights write it through two host copies"),
            (Some(s), _) => found = Some(s),
        }
    }
    let shadow = match found {
        Some(s) => s.clone(),
        None => crate::shadow::find(from as u64, (to - from) as u64).ok_or("no host copy of it")?,
    };
    let (start, len) = shadow.span();
    if shadow.in_place() || (from as u64) < start || to as u64 > start + len {
        return Err("no host copy holds all of it");
    }
    Ok(shadow)
}

/// What the steps of `runs` do, which step each is `(run_idx, list_ptr, step_idx)`,
/// and where each run starts in `does`. `written`: what flights may write.
fn does_of(written: &[(usize, usize)], runs: &[&HostList]) -> (Vec<Does>, Vec<(usize, usize, usize)>, Vec<usize>) {
    let cap = runs.len().saturating_mul(16);
    let mut does = Vec::with_capacity(cap);
    // Which step each is. One list's lock at a time: a submission may
    // name a list twice.
    let mut steps = Vec::with_capacity(cap);
    let mut run_starts = Vec::with_capacity(runs.len() + 1);
    // Destinations of placed copies whose source or destination flights write:
    // steps behind them cannot read stale title memory on the CPU either.
    let mut placed_blocked: Vec<(usize, usize)> = Vec::new();
    for (run_idx, list) in runs.iter().enumerate() {
        run_starts.push(does.len());
        let list_ptr = std::ptr::from_ref::<HostList>(list) as usize;
        for (i, step) in list.deferred().copies.iter().enumerate() {
            does.push(match step {
                &Step::Copy(dst, src, len) => {
                    let via_placed = !placed_blocked.is_empty() && touched(&placed_blocked, (src, src + len));
                    if via_placed || copy_blocked(written, dst, src, len).is_some() {
                        if via_placed {
                            insert_range(&mut placed_blocked, dst, dst + len);
                        }
                        Does::Copy {
                            dst,
                            src,
                            len,
                            asked: true,
                        }
                    } else {
                        Does::Copy {
                            dst,
                            src,
                            len,
                            asked: false,
                        }
                    }
                }
                Step::Fill(_, src, len, to) => Does::Fill {
                    src: *src,
                    len: *len,
                    asked: read_blocked(written, *src, *len)
                        || (!placed_blocked.is_empty() && touched(&placed_blocked, (*src, *src + *len))),
                    to: to.is_some(),
                },
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    let end = at + len;
                    Does::Read {
                        from: at,
                        to: end,
                        asked: touches(written, at, end)
                            || (!placed_blocked.is_empty() && touched(&placed_blocked, (at, end))),
                    }
                }
                &Step::Plane((at, len), _) => {
                    let end = at.saturating_add(len);
                    Does::Read {
                        from: at,
                        to: end,
                        asked: touches(written, at, end)
                            || (!placed_blocked.is_empty() && touched(&placed_blocked, (at, end))),
                    }
                }
                &Step::Host(_, (from, to)) if from < to => Does::Read {
                    from,
                    to,
                    asked: !placed_blocked.is_empty() && touched(&placed_blocked, (from, to)),
                },
                Step::Run(_) => Does::Run,
                Step::Placed(p) => {
                    let cpu_src =
                        p.len > 0 && matches!(p.waits(&[(p.src, p.src + p.len)]), place::Waits::Yes);
                    let waits = match p.waits(written) {
                        place::Waits::Yes => true,
                        place::Waits::ForReaders => {
                            insert_range(&mut placed_blocked, p.dst, p.dst + p.len);
                            false
                        }
                        place::Waits::No => {
                            if !placed_blocked.is_empty() && touched(&placed_blocked, (p.src, p.src + p.len)) {
                                insert_range(&mut placed_blocked, p.dst, p.dst + p.len);
                                cpu_src
                            } else {
                                false
                            }
                        }
                    };
                    Does::Placed {
                        dst: p.dst,
                        src: p.src,
                        len: p.len,
                        cpu_src,
                        waits,
                    }
                }
                Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(..) => continue,
            });
            steps.push((run_idx, list_ptr, i));
        }
    }
    run_starts.push(does.len());
    (does, steps, run_starts)
}

/// Per-cut splittability for `runs`: `splittable[cut]` (`0 < cut < runs.len()`)
/// holds iff no `HostList` appears on both sides of `cut`. Computed in `O(R)`.
fn splittable_cuts(runs: &[&HostList]) -> Vec<bool> {
    let n = runs.len();
    let mut out = vec![false; n];
    if n < 2 {
        return out;
    }
    let mut last_seen: HashMap<usize, usize> = HashMap::default();
    for (idx, &list) in runs.iter().enumerate() {
        last_seen.insert(std::ptr::from_ref::<HostList>(list) as usize, idx);
    }
    let mut reach = 0usize;
    for (idx, &list) in runs[..n - 1].iter().enumerate() {
        let ptr = std::ptr::from_ref::<HostList>(list) as usize;
        reach = reach.max(last_seen.get(&ptr).copied().unwrap_or(idx));
        let cut = idx + 1;
        out[cut] = reach < cut;
    }
    out
}

/// With flights in the air: before which of `runs` the submission is cut so
/// that what reads or writes the bytes of a copy at its place in an earlier
/// list ([`BEHIND_PLACED`]) is done on the card in front of a submission of
/// its own, behind that copy.
pub(super) fn cut_for(runs: &[&HostList]) -> Option<usize> {
    if stage() == 0 || !relay_exact() || !placed_order() || runs.len() < 2 {
        return None;
    }
    // (Most submissions have none.)
    let first = runs
        .iter()
        .position(|l| l.deferred().copies.iter().any(|s| matches!(s, Step::Placed(_))))?;
    if first + 1 == runs.len() {
        return None;
    }
    let splittable = splittable_cuts(runs);
    if !splittable[first + 1..].iter().any(|&ok| ok) {
        return None;
    }
    let written = line().written.clone();
    blocked_by(&written, runs, false)?;
    let (does, steps, run_starts) = does_of(&written, runs);
    let mut behind = None;
    let mut query = CutQuery {
        steps: &steps,
        run_starts: &run_starts,
        splittable: &splittable,
        min_cut: None,
    };
    if plan_inner(&does, stage(), true, false, &mut behind, Some(&mut query)).err() != Some(BEHIND_PLACED) {
        return None;
    }
    query.min_cut
}

/// Whether every page of `[from, to)` can be read (through the kernel:
/// asking Windows walks the page tables, milliseconds in the title's heap).
/// `known`: pages that could be, of the same submission: its fills' places
/// lie eight to a page, and each was read whole to find out.
fn mapped(from: usize, to: usize, known: &mut HashMap<usize, ()>) -> bool {
    const PAGE: usize = 4096;
    (from / PAGE..to.div_ceil(PAGE)).all(|page| {
        known.contains_key(&page) || {
            let there = read_into((page * PAGE).max(from) as *const u8, &mut [0]);
            if there {
                known.insert(page, ());
            }
            there
        }
    })
}

/// A table by addresses and numbers of ours.
type HashMap<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<AddressHash>>;

/// Where the bytes of the fill that is `step` (its run index, its list by
/// address, its place in it) go.
fn fill_to(runs: &[&HostList], step: (usize, usize, usize)) -> Option<FillTo> {
    let list = runs.get(step.0)?;
    match list.deferred().copies.get(step.2)? {
        Step::Fill(_, _, _, to) => to.clone(),
        _ => None,
    }
}

enum Source {
    /// A host copy and the title address read from it.
    Shadow(Arc<Shadow>, usize),
    /// Bytes read from CPU title memory into `Own::staged` (`staged_offset`, `title_src`).
    Staged(u64, usize),
}

struct ReadPiece {
    source: Source,
    /// Offset within the step's slice in `between`.
    offset: u64,
    len: usize,
}

/// A step done on the card.
struct Op {
    reads: Vec<ReadPiece>,
    len: usize,
    /// Its place in the buffer between.
    at: u64,
    level: u32,
    to: To,
}

enum To {
    /// A host copy, and the title address.
    Copy(Arc<Shadow>, usize),
    /// A descriptor slot (title heap, slot).
    Slot(usize, u32),
    /// A buffer's beginning.
    Buffer(ID3D12Resource),
}

/// Records a level of what [`stage`] describes into `list` (open): `reads`
/// are copied into `between` (source, offset in it, offset in `between`,
/// bytes), then `writes` out of it (destination, offset in it, offset in
/// `between`, bytes). `between` is left readable as constants (and must be
/// so, unless this is the `first` level), the others in the common state,
/// as the submission's lists expect them.
fn record(
    first: bool,
    list: &ID3D12GraphicsCommandList,
    between: &ID3D12Resource,
    reads: &[(&ID3D12Resource, u64, u64, u64)],
    writes: &[(&ID3D12Resource, u64, u64, u64)],
) {
    let common = D3D12_RESOURCE_STATE_COMMON;
    let (source, dest) = (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    let mut sources: Vec<&ID3D12Resource> = Vec::new();
    let mut dests: Vec<&ID3D12Resource> = Vec::new();
    for (all, list) in [(reads, &mut sources), (writes, &mut dests)] {
        for &(buffer, ..) in all {
            if !list.iter().any(|b| b.as_raw() == buffer.as_raw()) {
                list.push(buffer);
            }
        }
    }
    let both = |b: &ID3D12Resource| dests.iter().any(|d| d.as_raw() == b.as_raw());
    let constants = source | D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER;
    let mut barriers = vec![transition(between, dest, constants)];
    barriers.extend(sources.iter().map(|b| transition(b, source, if both(b) { dest } else { common })));
    let done: Vec<D3D12_RESOURCE_BARRIER> = dests.iter().map(|b| transition(b, dest, common)).collect();
    // SAFETY: an open list; live buffers, and the callers' ranges lie in
    // them. The buffers start in the common state (the list is the first of
    // its call) and are promoted by their first use.
    unsafe {
        if first {
            // What earlier lists' shaders wrote, first.
            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                        pResource: std::mem::ManuallyDrop::new(None),
                    }),
                },
            }]);
        } else {
            list.ResourceBarrier(&[transition(between, constants, dest)]);
        }
        for &(buffer, offset, at, len) in reads {
            list.CopyBufferRegion(between, at, buffer, offset, len);
        }
        list.ResourceBarrier(&barriers);
        for &(buffer, offset, at, len) in writes {
            list.CopyBufferRegion(buffer, offset, between, at, len);
        }
        if !done.is_empty() {
            list.ResourceBarrier(&done);
        }
    }
}

/// The list in front of a submission's, its buffer ([`BETWEEN`] bytes), and
/// optional reusable CPU-staged buffer for forwarded placed-copy sources that
/// have no host shadow.
struct Own {
    list: HostList,
    between: ID3D12Resource,
    staged: Option<(ID3D12Resource, usize, u64)>,
}

// SAFETY: D3D12 objects are free-threaded, and the list is touched by one
// thread at a time (the submitting one until it is sent, then the one that
// lands it, then nobody until it is taken out of `POOL`).
unsafe impl Send for Own {}

/// Those that have landed: closed, their steps dropped.
static POOL: Mutex<Vec<Own>> = Mutex::new(Vec::new());

/// Since the last frame-rate line: fills and copies done on the card, their
/// bytes, and submissions that waited instead, refused by a check.
static DONE: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What [`Host::card_steps`] took off a submission: the list that does it,
/// for the submission to run first, and the steps the CPU leaves out (their
/// list by address, their place in it).
pub(super) struct Taken {
    own: Own,
    steps: std::collections::HashSet<(usize, usize), std::hash::BuildHasherDefault<AddressHash>>,
    copied: Vec<(Arc<Shadow>, usize, usize)>,
}

impl Taken {
    pub(super) fn list(&self) -> &HostList {
        &self.own.list
    }

    /// Whether step `i` of `list` is done by this.
    pub(super) fn takes(&self, list: &HostList, i: usize) -> bool {
        self.steps.contains(&(std::ptr::from_ref(list) as usize, i))
    }

    fn copied(&self) -> impl Iterator<Item = Writer> {
        self.copied.iter().map(|(shadow, from, to)| Writer {
            shadow: Some(shadow.clone()),
            from: *from,
            to: *to,
            copy: true,
        })
    }

    /// The submission has run and landed: the list and its buffer are free.
    pub(super) fn landed(self) {
        // The host copies its steps held go; its commands stay until the
        // next use resets it.
        drop(std::mem::take(&mut *self.own.list.deferred()));
        POOL.lock().unwrap_or_else(|e| e.into_inner()).push(self.own);
    }
}

impl Host {
    /// With flights in the air: if what keeps `runs` from being prepared
    /// now is reads of title memory that can all be done on the card
    /// ([`stage`]), the list that does them. `None` if nothing is in the
    /// way or the old wait is due. Under the submission lock.
    pub(super) fn card_steps(&self, runs: &[&HostList]) -> Option<Taken> {
        if stage() == 0 || !relay_exact() {
            return None;
        }
        let (written, copies) = {
            let line = line();
            if line.written.is_empty() {
                return None;
            }
            (line.written.clone(), copies(&line))
        };
        blocked_by(&written, runs, false)?;
        let taken = if copied_read(&copies, runs) {
            Err("a copy done on the card is read through another host copy")
        } else {
            self.take_steps(&written, runs)
        };
        match taken {
            Ok(taken) => taken,
            Err(why) => {
                DONE[3].fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                refused(why);
                let _ = why;
                None
            }
        }
    }

    fn take_steps(&self, written: &[(usize, usize)], runs: &[&HostList]) -> Result<Option<Taken>, &'static str> {
        let mut spent = crate::timeline::span("steps on the card: chosen");
        let (does, steps, _) = does_of(written, runs);
        let order_on = placed_order();
        let mut behind = None;
        let (chosen, fwd) = plan_inner(&does, stage(), order_on, order_on, &mut behind, None)?;
        if chosen.is_empty() {
            return Ok(None);
        }
        spent.next("steps on the card: what holds their bytes");
        let mut writers = {
            let line = line();
            let mut all = writers(&line.line);
            all.extend(line.landing.iter().map(|w| Writer {
                shadow: w.shadow.clone(),
                ..*w
            }));
            all
        };
        let mut ops: Vec<Op> = Vec::with_capacity(chosen.len());
        // The places read (host copy or 0 for staged, address, bytes, level):
        // where in the buffer between their bytes are.
        let mut places: HashMap<(usize, usize, usize, u32), u64> = HashMap::default();
        // Some 2,800 fills a frame, of a few hundred places: what holds each
        // is looked for once, as long as no copy is added to the writers.
        let mut held: HashMap<(usize, usize), Arc<Shadow>> = HashMap::default();
        let mut staged_places: HashMap<(usize, usize), u64> = HashMap::default();
        let mut refreshes: Option<RefreshSet> = None;
        let mut room = HashMap::default();
        let mut end = 0u64;
        let mut staged_end = 0u64;
        let mut single_buf = [(0usize, 0usize, 0usize); 1];
        for &(i, level) in &chosen {
            let Some((segs, _)) = fwd.segments_of(&does, i, &mut single_buf) else {
                continue;
            };
            let (Does::Copy { len, .. } | Does::Fill { len, .. }) = does[i] else {
                continue;
            };
            if len == 0 {
                return Err("nothing to read");
            }
            // Only materialize `ReadPiece`s for chosen steps.
            let mut pieces: Vec<ReadPiece> = Vec::with_capacity(segs.len());
            for &(seg_off, seg_src, seg_len) in segs {
                let source = match held.get(&(seg_src, seg_len)) {
                    Some(from) => Source::Shadow(from.clone(), seg_src),
                    None => match holder(&writers, seg_src, seg_src + seg_len) {
                        Ok(from) => {
                            held.insert((seg_src, seg_len), from.clone());
                            Source::Shadow(from, seg_src)
                        }
                        Err(why) => {
                            if touches(written, seg_src, seg_src + seg_len)
                                || writers.iter().any(|w| meet((w.from, w.to), (seg_src, seg_src + seg_len)))
                            {
                                return Err(why);
                            }
                            if !mapped(seg_src, seg_src + seg_len, &mut room) {
                                return Err("not all of what it reads is mapped");
                            }
                            let staged_at = *staged_places.entry((seg_src, seg_len)).or_insert_with(|| {
                                let at = staged_end;
                                staged_end = (staged_end + seg_len as u64).next_multiple_of(16);
                                at
                            });
                            Source::Staged(staged_at, seg_src)
                        }
                    },
                };
                pieces.push(ReadPiece {
                    source,
                    offset: seg_off as u64,
                    len: seg_len,
                });
            }
            let at = if pieces.len() == 1 {
                match &pieces[0].source {
                    Source::Shadow(from, seg_src) => {
                        let place = (Arc::as_ptr(from) as usize, *seg_src, len, level);
                        match places.get(&place) {
                            Some(&at) => at,
                            None => {
                                if end + len as u64 > BETWEEN {
                                    return Err("too much at once");
                                }
                                if !mapped(*seg_src, *seg_src + len, &mut room) {
                                    return Err("not all of what it reads is mapped");
                                }
                                let at = end;
                                end = (end + len as u64).next_multiple_of(256);
                                places.insert(place, at);
                                at
                            }
                        }
                    }
                    Source::Staged(_, seg_src) => {
                        let place = (0, *seg_src, len, level);
                        match places.get(&place) {
                            Some(&at) => at,
                            None => {
                                if end + len as u64 > BETWEEN {
                                    return Err("too much at once");
                                }
                                let at = end;
                                end = (end + len as u64).next_multiple_of(256);
                                places.insert(place, at);
                                at
                            }
                        }
                    }
                }
            } else {
                if end + len as u64 > BETWEEN {
                    return Err("too much at once");
                }
                for piece in &pieces {
                    if let Source::Shadow(_, seg_src) = &piece.source
                        && !mapped(*seg_src, *seg_src + piece.len, &mut room)
                    {
                        return Err("not all of what it reads is mapped");
                    }
                }
                let at = end;
                end = (end + len as u64).next_multiple_of(256);
                at
            };
            let to = match does[i] {
                Does::Copy { dst, .. } => {
                    let to = holder(&writers, dst, dst + len)?;
                    let rset = refreshes.get_or_insert_with(|| RefreshSet::from_runs(runs));
                    if rset.elsewhere(Arc::as_ptr(&to) as usize, dst, dst + len) {
                        return Err("another host copy of what it writes is read");
                    }
                    if !mapped(dst, dst + len, &mut room) {
                        return Err("not all of what it writes is mapped");
                    }
                    // What the steps behind it read of it is in that host
                    // copy, or nowhere.
                    writers.push(Writer {
                        shadow: Some(to.clone()),
                        from: dst,
                        to: dst + len,
                        copy: true,
                    });
                    held.clear();
                    To::Copy(to, dst)
                }
                _ => match fill_to(runs, steps[i]) {
                    Some(FillTo::Slot(heap, index)) => To::Slot(heap, index),
                    Some(FillTo::Buffer(buffer)) => To::Buffer(buffer),
                    None => continue,
                },
            };
            ops.push(Op {
                reads: pieces,
                len,
                at,
                level,
                to,
            });
        }
        spent.next("steps on the card: the list");
        let Some(mut own) = self.own_list() else {
            return Err("no host list or buffer for it");
        };
        if staged_end > 0 {
            let Some(staged_ptr) = self.ensure_staged(&mut own, staged_end) else {
                return Err("no staging buffer for it");
            };
            for (&(seg_src, seg_len), &staged_at) in &staged_places {
                // SAFETY: `staged` has at least `staged_end` mapped bytes.
                let out = unsafe { std::slice::from_raw_parts_mut((staged_ptr + staged_at as usize) as *mut u8, seg_len) };
                if !read_into(seg_src as *const u8, out) {
                    return Err("not all of what it reads is mapped");
                }
            }
        }
        // First every refresh, then the writes: the bytes compared with are
        // the title's. (One step for each host copy, over all that is read of
        // it: a flush brings a copy up to date from the first range asked for
        // to the last, whatever the steps.)
        let mut spans: ByCopy<(Arc<Shadow>, usize, usize)> = ByCopy::default();
        for op in &ops {
            for piece in &op.reads {
                if let Source::Shadow(shadow, at) = &piece.source {
                    let span = spans
                        .entry(Arc::as_ptr(shadow) as usize)
                        .or_insert_with(|| (shadow.clone(), *at, *at + piece.len));
                    (span.1, span.2) = (span.1.min(*at), span.2.max(*at + piece.len));
                }
            }
            if let To::Copy(to, dst) = &op.to {
                let span = spans
                    .entry(Arc::as_ptr(to) as usize)
                    .or_insert_with(|| (to.clone(), *dst, *dst + op.len));
                (span.1, span.2) = (span.1.min(*dst), span.2.max(*dst + op.len));
            }
        }
        for (shadow, from, to) in spans.into_values() {
            shadow.refresh(&own.list, from as u64, (to - from) as u64);
        }
        let mut copied = Vec::new();
        for op in &ops {
            if let To::Copy(to, dst) = &op.to {
                to.gpu_writes(&own.list, *dst as u64, op.len as u64);
                copied.push((to.clone(), *dst, *dst + op.len));
            }
        }
        #[cfg(feature = "diagnostics")]
        own.list.note_op(|| format!("{} copies of title memory done on the card, {end} bytes (OATHKEEP_CARD_STEPS)", ops.len()));
        let staged_buf = own.staged.as_ref().map(|(b, _, _)| b.clone());
        let levels = ops.iter().map(|op| op.level).max().unwrap_or(0);
        let mut by_level: Vec<Vec<usize>> = vec![Vec::new(); levels as usize + 1];
        for (idx, op) in ops.iter().enumerate() {
            by_level[op.level as usize].push(idx);
        }
        for (lvl_idx, bucket) in by_level.iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            let level = lvl_idx as u32;
            let mut reads = Vec::with_capacity(bucket.len());
            let mut writes = Vec::new();
            for &op_idx in bucket {
                let op = &ops[op_idx];
                let len = op.len as u64;
                if op.reads.len() == 1 {
                    match &op.reads[0].source {
                        Source::Shadow(from, seg_src) => {
                            if places
                                .remove(&(Arc::as_ptr(from) as usize, *seg_src, op.len, level))
                                .is_some()
                            {
                                reads.push((&from.buffer, from.offset(*seg_src as u64), op.at, len));
                            }
                        }
                        Source::Staged(staged_at, seg_src) => {
                            if places.remove(&(0, *seg_src, op.len, level)).is_some()
                                && let Some(buf) = &staged_buf
                            {
                                reads.push((buf, *staged_at, op.at, len));
                            }
                        }
                    }
                } else {
                    for piece in &op.reads {
                        match &piece.source {
                            Source::Shadow(from, seg_src) => {
                                reads.push((
                                    &from.buffer,
                                    from.offset(*seg_src as u64),
                                    op.at + piece.offset,
                                    piece.len as u64,
                                ));
                            }
                            Source::Staged(staged_at, _) => {
                                if let Some(buf) = &staged_buf {
                                    reads.push((buf, *staged_at, op.at + piece.offset, piece.len as u64));
                                }
                            }
                        }
                    }
                }
                match &op.to {
                    To::Copy(to, dst) => writes.push((&to.buffer, to.offset(*dst as u64), op.at, len)),
                    To::Buffer(buffer) => writes.push((buffer, 0, op.at, len)),
                    To::Slot(..) => {}
                }
            }
            record(level == 0, &own.list.list, &own.between, &reads, &writes);
        }
        own.list.close();
        // SAFETY: a live buffer.
        let gpu = unsafe { own.between.GetGPUVirtualAddress() };
        spent.next("steps on the card: the slots");
        // (11,000 slots a frame in a street, two descriptors each: on
        // several threads, a slot always on the same one, in the steps'
        // order.)
        let parts = if ops.len() < 256 { 1 } else { 8 };
        crate::land::each_under(parts, &|part| {
            for op in &ops {
                if let To::Slot(heap, index) = &op.to
                    && *index as usize / 4 % parts == part
                {
                    // (A heap that is gone: nothing can read the slot.)
                    crate::descriptor::constant_slot(*heap, *index, &own.between, gpu + op.at, op.len as u32);
                }
            }
        });
        let copies = ops.iter().filter(|op| matches!(op.to, To::Copy(..))).count();
        DONE[0].fetch_add((ops.len() - copies) as u64, Ordering::Relaxed);
        DONE[1].fetch_add(copies as u64, Ordering::Relaxed);
        DONE[2].fetch_add(ops.iter().map(|op| op.len as u64).sum(), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        noted(&ops);
        spent.end();
        Ok(Some(Taken {
            own,
            steps: chosen.iter().map(|&(i, _)| (steps[i].1, steps[i].2)).collect(),
            copied,
        }))
    }

    /// A list of ours open for recording, and its buffer.
    fn own_list(&self) -> Option<Own> {
        let pooled = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop();
        if let Some(own) = pooled {
            own.list.reset();
            return Some(own);
        }
        let between = self.uav_buffer(BETWEEN)?;
        name(&between, || "copies of title memory on the card".into());
        let list = self.new_list().ok()?;
        Some(Own {
            list,
            between,
            staged: None,
        })
    }

    /// Ensures `own.staged` holds at least `needed` mapped bytes.
    fn ensure_staged(&self, own: &mut Own, needed: u64) -> Option<usize> {
        if let Some((_, ptr, cap)) = &own.staged
            && *cap >= needed
        {
            return Some(*ptr);
        }
        let size = needed.max(64 << 10).next_multiple_of(64 << 10);
        let (buffer, data) = self.shared_buffer(size)?;
        name(&buffer, || "staged CPU bytes for card steps".into());
        if let Some((old, _, _)) = own.staged.replace((buffer, data as usize, size)) {
            own.list.keep(&old);
        }
        Some(data as usize)
    }
}

/// Diagnostics: the copies and fills done on the card since the last
/// frame-rate line, by bytes, host copy read and host copy written (start
/// and length; zeros for a fill): how many, and the last title addresses.
#[cfg(feature = "diagnostics")]
type Noted = std::collections::HashMap<(usize, (u64, u64), (u64, u64)), (u64, usize, usize)>;
#[cfg(feature = "diagnostics")]
static NOTED: Mutex<Option<Noted>> = Mutex::new(None);

#[cfg(feature = "diagnostics")]
fn noted(ops: &[Op]) {
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    let noted = noted.get_or_insert_with(Default::default);
    for op in ops {
        let (from_span, src) = match op.reads.first().map(|p| &p.source) {
            Some(Source::Shadow(s, src)) => (s.span(), *src),
            Some(Source::Staged(_, src)) => ((0, 0), *src),
            None => ((0, 0), 0),
        };
        let (to, dst) = match &op.to {
            To::Copy(to, dst) => (to.span(), *dst),
            To::Slot(..) | To::Buffer(_) => ((0, 0), 0),
        };
        let e = noted.entry((op.len, from_span, to)).or_default();
        *e = (e.0 + 1, src, dst);
    }
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    if stage() == 0 {
        return;
    }
    let d: Vec<f64> = DONE.iter().map(|d| d.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "steps done on the card instead of waiting for flights, per frame (OATHKEEP_CARD_STEPS={}): {:.2} fills, {:.2} buffer \
         copies, {:.1} KB; {:.2} submissions refused and waited",
        stage(),
        d[0],
        d[1],
        d[2] / 1024.0,
        d[3],
    );
    #[cfg(feature = "diagnostics")]
    {
        let noted = NOTED.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
        let mut noted: Vec<_> = noted.into_iter().collect();
        noted.sort_unstable_by_key(|(_, e)| std::cmp::Reverse(e.0));
        for ((len, from, to), (n, src, dst)) in noted.into_iter().take(5) {
            let n = n as f64 / frames;
            if to.1 == 0 {
                tracing::info!("done on the card, per frame: {n:.2} fills of {len:#x} bytes from the host copy of {:#x}+{:#x} (last {src:#x})", from.0, from.1);
            } else {
                tracing::info!(
                    "done on the card, per frame: {n:.2} copies of {len:#x} bytes from the host copy of {:#x}+{:#x} (last {src:#x}) \
                     to that of {:#x}+{:#x} (last {dst:#x})",
                    from.0,
                    from.1,
                    to.0,
                    to.1
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(dst: usize, src: usize, len: usize, asked: bool) -> Does {
        Does::Copy { dst, src, len, asked }
    }

    fn fill(src: usize, len: usize, asked: bool) -> Does {
        Does::Fill { src, len, asked, to: true }
    }

    fn stuck(src: usize, len: usize) -> Does {
        Does::Fill { src, len, asked: false, to: false }
    }

    /// What is done on the card, as (place, level), in stage 2.
    fn planned(does: &[Does]) -> Vec<(usize, u32)> {
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

    fn placed_item(dst: usize, src: usize, len: usize) -> Does {
        Does::Placed {
            dst,
            src,
            len,
            cpu_src: false,
            waits: false,
        }
    }

    fn plan_forwarded(does: &[Does]) -> Result<(Vec<(usize, u32)>, Forwarding), &'static str> {
        plan_inner(does, 2, true, true, &mut None, None)
    }

    fn segs_of(fwd: &Forwarding, does: &[Does], i: usize) -> Vec<(usize, usize, usize)> {
        let mut buf = [(0usize, 0usize, 0usize); 1];
        fwd.segments_of(does, i, &mut buf).unwrap().0.to_vec()
    }

    /// Reference backwards resolver used only to verify exact equivalence of
    /// [`Forwarding::build`]'s single-pass `ActivePlaced` interval map.
    fn reference_placed_segments(does: &[Does], at: usize) -> Vec<(usize, usize, usize)> {
        let (r, _) = reads_writes(&does[at]).unwrap();
        let total = r.1.saturating_sub(r.0);
        if total == 0 {
            return vec![(0, r.0, 0)];
        }
        let mut segs: Vec<(usize, usize, usize, bool)> = vec![(0, r.0, total, false)];
        for k in (0..at).rev() {
            if segs.iter().all(|s| s.3) {
                break;
            }
            match does[k] {
                Does::Copy { dst, len, .. } if len > 0 => {
                    let c = (dst, dst + len);
                    if !segs.iter().any(|&(_, a, n, locked)| !locked && meet((a, a + n), c)) {
                        continue;
                    }
                    let mut next = Vec::with_capacity(segs.len() + 2);
                    for (off, a, n, locked) in segs {
                        let b = a + n;
                        if locked || !meet((a, b), c) {
                            next.push((off, a, n, locked));
                            continue;
                        }
                        if a < c.0 {
                            next.push((off, a, c.0 - a, false));
                        }
                        let (lo, hi) = (a.max(c.0), b.min(c.1));
                        next.push((off + (lo - a), lo, hi - lo, true));
                        if b > c.1 {
                            next.push((off + (c.1 - a), c.1, b - c.1, false));
                        }
                    }
                    segs = next;
                }
                Does::Placed { dst, src, len, .. } if len > 0 => {
                    let p = (dst, dst + len);
                    if !segs.iter().any(|&(_, a, n, locked)| !locked && meet((a, a + n), p)) {
                        continue;
                    }
                    let mut next = Vec::with_capacity(segs.len() + 2);
                    for (off, a, n, locked) in segs {
                        let b = a + n;
                        if locked || !meet((a, b), p) {
                            next.push((off, a, n, locked));
                            continue;
                        }
                        if a < p.0 {
                            next.push((off, a, p.0 - a, false));
                        }
                        let (lo, hi) = (a.max(p.0), b.min(p.1));
                        next.push((off + (lo - a), src + (lo - dst), hi - lo, false));
                        if b > p.1 {
                            next.push((off + (p.1 - a), p.1, b - p.1, false));
                        }
                    }
                    segs = next;
                }
                _ => {}
            }
        }
        let mut out: Vec<(usize, usize, usize)> = Vec::with_capacity(segs.len());
        for (off, a, n, _) in segs {
            if let Some(last) = out.last_mut()
                && last.0 + last.2 == off
                && last.1 + last.2 == a
            {
                last.2 += n;
            } else {
                out.push((off, a, n));
            }
        }
        out
    }

    #[test]
    fn single_placed_forwarding_remaps_same_list_reads() {
        let p = placed_item(0x9_9273_8280, 0x9_6071_0400, 0xbd0);
        let f = fill(0x9_9273_8280, 0xf0, true);
        let does = [p, f];
        let (chosen, fwd) = plan_forwarded(&does).unwrap();
        assert_eq!(chosen, vec![(1, 0)]);
        assert_eq!(segs_of(&fwd, &does, 1), vec![(0, 0x9_6071_0400, 0xf0)]);
        // Fully covered single-interval forwarding does not touch `seg_pool`.
        assert!(fwd.seg_pool.is_empty());

        // Partial left/right straddling around a single placed copy.
        let straddle = [placed_item(2000, 5000, 32), fill(1992, 48, true)];
        let (chosen, fwd) = plan_forwarded(&straddle).unwrap();
        assert_eq!(chosen, vec![(1, 0)]);
        assert_eq!(segs_of(&fwd, &straddle, 1), vec![(0, 1992, 8), (8, 5000, 32), (40, 2032, 8)]);

        // Taken copy ahead of placed copy elevates forwarded fill to level 1.
        let dep = [copy(3000, 7000, 64, true), placed_item(2000, 3000, 64), fill(2000, 64, false)];
        let (chosen, fwd) = plan_forwarded(&dep).unwrap();
        assert_eq!(chosen, vec![(0, 0), (2, 1)]);
        assert_eq!(segs_of(&fwd, &dep, 2), vec![(0, 3000, 64)]);
    }

    #[test]
    fn chained_placed_copies_forward_transitively_and_coalesce() {
        // Multi-hop chain with partial overlap on both ends.
        let chain = [
            placed_item(6000, 8000, 64),
            placed_item(4000, 6008, 48),
            placed_item(2000, 4016, 32),
            fill(2008, 48, true),
        ];
        let (chosen, fwd) = plan_forwarded(&chain).unwrap();
        assert_eq!(chosen, vec![(3, 0)]);
        assert_eq!(segs_of(&fwd, &chain, 3), vec![(0, 8032, 24), (24, 2032, 24)]);
        assert_eq!(segs_of(&fwd, &chain, 3), reference_placed_segments(&chain, 3));

        // Adjacent collinear placed copies coalesce back into a single piece.
        let coalesced = [
            placed_item(2000, 5000, 32),
            placed_item(2032, 5032, 32),
            fill(2016, 32, true),
        ];
        let (chosen, fwd) = plan_forwarded(&coalesced).unwrap();
        assert_eq!(chosen, vec![(2, 0)]);
        assert_eq!(segs_of(&fwd, &coalesced, 2), vec![(0, 5016, 32)]);
        assert!(fwd.seg_pool.is_empty());

        // Round-trip placed copy `A -> B` then `B -> A` restores identity cleanly.
        let round_trip = [
            placed_item(3000, 2000, 64),
            placed_item(2000, 3000, 64),
            fill(2000, 64, true),
        ];
        let (_, fwd) = plan_forwarded(&round_trip).unwrap();
        assert_eq!(segs_of(&fwd, &round_trip, 2), vec![(0, 2000, 64)]);
    }

    #[test]
    fn copy_overwrite_splits_and_invalidates_active_placed_intervals() {
        // A Copy overwriting the middle `[4016, 4048)` of an active Placed
        // `[4000, 4064) -> 9000` splits `active` into left `[4000, 4016) -> 9000`
        // and right `[4048, 4064) -> 9048`, while locking `[4016, 4048)` to the
        // Copy's destination.
        let does = [
            placed_item(4000, 9000, 64),
            copy(4016, 7000, 32, false),
            placed_item(2000, 4000, 64),
            fill(2000, 64, true),
        ];
        let fwd = Forwarding::build(&does, true, true);
        assert_eq!(
            segs_of(&fwd, &does, 3),
            vec![(0, 9000, 16), (16, 4016, 32), (48, 9048, 16)]
        );
        assert_eq!(segs_of(&fwd, &does, 3), reference_placed_segments(&does, 3));
        // Because the Copy at index 1 wrote `[4016, 4048)` behind `placed_item(4000, 9000, 64)`
        // and is pulled in by `fill(2000, 64, true)`, `plan_forwarded` refuses with `BEHIND_PLACED`.
        assert_eq!(plan_forwarded(&does).err(), Some(BEHIND_PLACED));

        // When the Copy is ahead of `placed_item(2000, 4000, 64)` with no earlier
        // placed copy on 4000, `plan_forwarded` pulls the Copy onto level 0 and
        // schedules the forwarded Fill on level 1.
        let valid = [
            copy(4016, 7000, 32, false),
            placed_item(2000, 4000, 64),
            fill(2000, 64, true),
        ];
        let (chosen, fwd) = plan_forwarded(&valid).unwrap();
        assert_eq!(chosen, vec![(0, 0), (2, 1)]);
        assert_eq!(segs_of(&fwd, &valid, 2), vec![(0, 4000, 64)]);
    }

    #[test]
    fn self_overlap_and_conflicting_writes_are_rejected() {
        // Placed copy overlapping its own source (`dst != src && meet(dst, src)`).
        assert_eq!(plan_forwarded(&[placed_item(2000, 2032, 64), fill(2000, 64, true)]).err(), Some(BEHIND_PLACED));
        assert_eq!(plan_forwarded(&[placed_item(2032, 2000, 64), fill(2032, 64, true)]).err(), Some(BEHIND_PLACED));

        // Placed copy whose CPU scratch source waits on flights.
        let waiting = Does::Placed {
            dst: 2000,
            src: 3000,
            len: 64,
            cpu_src: true,
            waits: true,
        };
        assert_eq!(plan_forwarded(&[waiting, fill(2000, 64, true)]).err(), Some(BEHIND_PLACED));

        // Copy whose forwarded read maps onto its own destination (`a copy onto itself`).
        let forwarded_self_copy = [placed_item(2000, 5000, 64), copy(5016, 2000, 64, true)];
        assert_eq!(plan_forwarded(&forwarded_self_copy).err(), Some("a copy onto itself"));

        // Writes behind a placed copy onto its dst or src refuse.
        assert_eq!(
            plan_forwarded(&[placed_item(2000, 3000, 64), copy(2000, 7000, 8, true)]).err(),
            Some(BEHIND_PLACED)
        );
        assert_eq!(
            plan_forwarded(&[placed_item(2000, 3000, 64), copy(3000, 7000, 8, true)]).err(),
            Some(BEHIND_PLACED)
        );

        // CPU-sourced placed copy behind a taken copy onto its source refuses.
        let cpu_placed = Does::Placed {
            dst: 2000,
            src: 3000,
            len: 64,
            cpu_src: true,
            waits: false,
        };
        assert_eq!(
            plan_forwarded(&[copy(3000, 7000, 8, true), cpu_placed, fill(2000, 64, true)]).err(),
            Some(BEHIND_PLACED)
        );

        // Cross-list cut query skips same-list conflicts and finds the earliest
        // splittable cross-list cut in one pass.
        let cross = [
            placed_item(2000, 3000, 64),
            fill(2000, 16, true),
            fill(2016, 16, true),
        ];
        let steps = [(0, 100, 0), (0, 100, 1), (1, 200, 0)];
        let run_starts = [0, 2, 3];
        let splittable = [false, true];
        let mut behind = None;
        let mut query = CutQuery {
            steps: &steps,
            run_starts: &run_starts,
            splittable: &splittable,
            min_cut: None,
        };
        assert_eq!(
            plan_inner(&cross, 2, true, false, &mut behind, Some(&mut query)).err(),
            Some(BEHIND_PLACED)
        );
        assert_eq!(query.min_cut, Some(1));
    }

    #[test]
    fn forwarded_placed_levels_preserve_exact_sequential_semantics() {
        let mut seed = 54321u32;
        let mut random = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let (mut done, mut forwarded) = (0, 0);
        for _ in 0..4000 {
            let does: Vec<Does> = (0..2 + random(8))
                .map(|_| {
                    let (at, other, len) = (random(60) as usize, random(60) as usize, 1 + random(8) as usize);
                    let asked = random(4) == 0;
                    match random(10) {
                        0..3 => copy(at, other, len, asked),
                        3..5 => placed_item(at, other, len),
                        5..8 => fill(at, len, asked),
                        8 => stuck(at, len),
                        _ => Does::Read {
                            from: at,
                            to: at + len,
                            asked: false,
                        },
                    }
                })
                .collect();
            let Ok((chosen, fwd)) = plan_forwarded(&does) else {
                continue;
            };
            done += 1;
            // Verify both ActivePlaced equivalence and LevelSegTree equivalence!
            let order: Vec<usize> = chosen.iter().map(|&(i, _)| i).collect();
            let copies_n = order.iter().filter(|&&i| matches!(does[i], Does::Copy { .. })).count();
            assert_eq!(
                assign_levels_segtree(&does, &fwd, &order, copies_n),
                assign_levels_direct(&does, &fwd, &order)
            );
            for (idx, d) in does.iter().enumerate() {
                if matches!(d, Does::Copy { .. } | Does::Fill { to: true, .. }) {
                    assert_eq!(segs_of(&fwd, &does, idx), reference_placed_segments(&does, idx));
                }
            }
            let start: Vec<u32> = (0..80).collect();
            let mut memory = start.clone();
            let mut wanted = Vec::new();
            for d in &does {
                let (from, to) = match *d {
                    Does::Read { from, to, .. } => (from, to),
                    Does::Placed { src, len, .. } => (src, src + len),
                    _ => reads_writes(d).unwrap().0,
                };
                wanted.push(memory[from..to].to_vec());
                match *d {
                    Does::Copy { dst, src, len, .. } | Does::Placed { dst, src, len, .. } => {
                        memory.copy_within(src..src + len, dst);
                    }
                    _ => {}
                }
            }
            let is_taken = |i: usize| chosen.binary_search_by_key(&i, |&(idx, _)| idx).is_ok();
            let untaken_races_placed = does.iter().enumerate().any(|(p, d)| {
                let Does::Placed { dst, src, len, .. } = *d else {
                    return false;
                };
                does[p + 1..].iter().enumerate().any(|(o, later)| {
                    !is_taken(p + 1 + o)
                        && matches!(*later, Does::Copy { dst: cd, len: cl, .. }
                            if meet((cd, cd + cl), (dst, dst + len)) || meet((cd, cd + cl), (src, src + len)))
                })
            });
            if untaken_races_placed {
                continue;
            }
            let mut cpu = start.clone();
            let mut card = start.clone();
            let mut cpu_stale: Vec<(usize, usize)> = Vec::new();
            for (i, d) in does.iter().enumerate() {
                if is_taken(i) {
                    if let Some((_, w)) = reads_writes(d)
                        && w.0 < w.1
                    {
                        insert_range(&mut cpu_stale, w.0, w.1);
                    }
                    continue;
                }
                match *d {
                    Does::Placed { dst, src, len, .. } => {
                        if touched(&cpu_stale, (src, src + len)) {
                            insert_range(&mut cpu_stale, dst, dst + len);
                        } else if !touched(&cpu_stale, (dst, dst + len)) {
                            cpu.copy_within(src..src + len, dst);
                        }
                    }
                    Does::Copy { dst, src, len, .. } => {
                        assert_eq!(cpu[src..src + len], wanted[i][..]);
                        cpu.copy_within(src..src + len, dst);
                        card[dst..dst + len].copy_from_slice(&wanted[i]);
                    }
                    _ => {}
                }
            }
            for level in 0..=chosen.iter().map(|c| c.1).max().unwrap_or(0) {
                let of: Vec<usize> = chosen.iter().filter(|c| c.1 == level).map(|c| c.0).collect();
                let before = card.clone();
                for i in of {
                    let segs = segs_of(&fwd, &does, i);
                    let ((raw_from, raw_to), (dst, dst_end)) = reads_writes(&does[i]).unwrap();
                    if segs.len() > 1 || segs[0].1 != raw_from {
                        forwarded += 1;
                    }
                    let mut assembled = vec![0u32; raw_to - raw_from];
                    for (off, a, n) in segs {
                        assembled[off..off + n].copy_from_slice(&before[a..a + n]);
                    }
                    assert_eq!(assembled, wanted[i], "step {i} of {does:?}: {chosen:?}");
                    if dst < dst_end {
                        card[dst..dst_end].copy_from_slice(&assembled);
                    }
                }
            }
            for d in &does {
                if let Does::Placed { dst, src, len, .. } = *d {
                    card.copy_within(src..src + len, dst);
                }
            }
            assert_eq!(card, memory, "{does:?}: {chosen:?}");
        }
        assert!(done > 500 && forwarded > 50, "{done} {forwarded}");
    }

    #[test]
    fn stress_ten_thousand_steps_plans_fast_and_accurately() {
        const N: usize = 10_000;
        let mut does = Vec::with_capacity(N);
        // Realistic heavy submission: chained copies, placed ring copies, and
        // thousands of constant-buffer fills (both overlapping placed copies and
        // disjoint from them) exercising O(1) bounding-box skips, O(log P)
        // active-interval lookups, and O(N log N) segment-tree level assignment.
        for c in 0..64usize {
            let src = 0x1000_0000 + c * 128;
            let dst = 0x1000_0000 + (c + 1) * 128;
            does.push(copy(dst, src, 128, c == 0));
        }
        for p in 0..64usize {
            let src = 0x1000_0000 + (p + 1) * 128;
            let dst = 0x2000_0000 + p * 256;
            does.push(placed_item(dst, src, 128));
        }
        while does.len() < N {
            let idx = does.len();
            if idx % 5 == 0 {
                // Overlaps an active placed copy -> forwarded to copy chain!
                let p = (idx / 5) % 64;
                does.push(fill(0x2000_0000 + p * 256 + 16, 64, true));
            } else {
                // Disjoint ring fill -> O(1) bounding-box skip!
                does.push(fill(0x3000_0000 + idx * 64, 64, idx % 2 == 0));
            }
        }
        // Warm once, then measure.
        let _ = plan_forwarded(&does).unwrap();
        let started = std::time::Instant::now();
        let (chosen, fwd) = plan_forwarded(&does).unwrap();
        let elapsed = started.elapsed();
        assert!(chosen.len() > 5_000);
        // Chain of 64 dependent copies produces levels 0..=63, and fills reading
        // placed copy 63 (sourced from copy 63) land on level 64.
        let max_level = chosen.iter().map(|&(_, lvl)| lvl).max().unwrap_or(0);
        assert_eq!(max_level, 64);
        let first_fill = (128..N).find(|&idx| idx % 5 == 0).unwrap();
        let expected_p = (first_fill / 5) % 64;
        assert_eq!(
            segs_of(&fwd, &does, first_fill),
            vec![(0, 0x1000_0000 + (expected_p + 1) * 128 + 16, 64)]
        );
        eprintln!("stress 10,000 steps planned {} card ops in {:?}", chosen.len(), elapsed);
        let limit_ms = if cfg!(debug_assertions) { 75 } else { 5 };
        assert!(
            elapsed.as_millis() <= limit_ms,
            "10,000-step plan took {:?} (limit {limit_ms} ms)",
            elapsed
        );
    }

    #[test]
    fn placed_order_defaults_on_unless_zero() {
        // Without OATHKEEP_PLACED_ORDER=0, placed_order() is enabled by default
        // so fills ahead of a placed copy are not refused.
        assert!(placed_order());
    }

    /// Whatever the steps, done level by level (in a level every read
    /// before every write) they read and leave what they read and leave one
    /// after the other, the CPU's steps ahead of all of them.
    #[test]
    fn levels_do_what_the_order_does() {
        let mut seed = 12345u32;
        let mut random = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let (mut done, mut refused) = (0, 0);
        for _ in 0..4000 {
            let does: Vec<Does> = (0..2 + random(9))
                .map(|_| {
                    let (at, other, len) = (random(60) as usize, random(60) as usize, 1 + random(8) as usize);
                    let asked = random(4) == 0;
                    match random(8) {
                        0..4 => copy(at, other, len, asked),
                        4..6 => fill(at, len, asked),
                        6 => stuck(at, len),
                        _ => Does::Read { from: at, to: at + len, asked: false },
                    }
                })
                .collect();
            let Ok(chosen) = plan(&does, 2, true, &mut None) else {
                refused += 1;
                continue;
            };
            done += 1;
            let start: Vec<u32> = (0..80).collect();
            // One after the other: what each reads, and what is left.
            let mut memory = start.clone();
            let mut wanted = Vec::new();
            for d in &does {
                let (from, to) = match *d {
                    Does::Read { from, to, .. } => (from, to),
                    _ => reads_writes(d).unwrap().0,
                };
                wanted.push(memory[from..to].to_vec());
                if let Does::Copy { dst, src, len, .. } = *d {
                    memory.copy_within(src..src + len, dst);
                }
            }
            // The CPU's, then the card's.
            let mut cpu = start.clone();
            let taken = |i: usize| chosen.iter().any(|c| c.0 == i);
            for (i, d) in does.iter().enumerate().filter(|&(i, _)| !taken(i)) {
                let (from, to) = match *d {
                    Does::Read { from, to, .. } => (from, to),
                    _ => reads_writes(d).unwrap().0,
                };
                assert_eq!(cpu[from..to], wanted[i][..], "step {i} of {does:?}");
                if let Does::Copy { dst, src, len, .. } = *d {
                    cpu.copy_within(src..src + len, dst);
                }
            }
            let mut card = cpu.clone();
            for level in 0..=chosen.iter().map(|c| c.1).max().unwrap_or(0) {
                let of: Vec<usize> = chosen.iter().filter(|c| c.1 == level).map(|c| c.0).collect();
                let before = card.clone();
                for i in of {
                    let ((from, to), (dst, _)) = reads_writes(&does[i]).unwrap();
                    assert_eq!(before[from..to], wanted[i][..], "step {i} of {does:?}: {chosen:?}");
                    if matches!(does[i], Does::Copy { .. }) {
                        card[dst..dst + to - from].copy_from_slice(&before[from..to]);
                    }
                }
            }
            assert_eq!(card, memory, "{does:?}: {chosen:?}");
        }
        assert!(done > 500 && refused > 100, "{done} {refused}");
    }

    /// What cannot be done on the card refuses the lot, where it stands in
    /// the way.
    #[test]
    fn steps_in_the_way_are_refused() {
        let taken = copy(1000, 5000, 100, true);
        assert!(plan(&[taken, Does::Run], 2, true, &mut None).is_err());
        // A texture that itself waits for flights; a fill that cannot be
        // taken but waits.
        assert!(plan(&[taken, Does::Read { from: 1, to: 2, asked: true }], 2, true, &mut None).is_err());
        assert!(plan(&[Does::Fill { src: 1, len: 1, asked: true, to: false }], 2, true, &mut None).is_err());
        // Reads on the CPU of what it writes: behind it, not ahead of it.
        for read in [Does::Read { from: 900, to: 1001, asked: false }, stuck(1050, 3)] {
            assert!(plan(&[taken, read], 2, true, &mut None).is_err(), "{read:?} behind");
            assert_eq!(planned(&[read, taken]), [(1, 0)], "{read:?} ahead");
        }
        // Also of what is taken along.
        assert!(plan(&[taken, copy(3000, 1000, 10, false), stuck(3005, 1)], 2, true, &mut None).is_err());
        // Into itself, and copies without stage 2.
        assert!(plan(&[copy(1000, 1050, 100, true)], 2, true, &mut None).is_err());
        assert!(plan(&[taken], 1, true, &mut None).is_err());
        assert!(plan(&[fill(1000, 4, true), copy(1000, 5000, 100, false)], 1, true, &mut None).is_err());
    }

    #[test]
    fn a_refresh_of_another_host_copy_is_seen() {
        let does = [Does::Refresh { shadow: 7, from: 100, to: 200 }];
        assert!(!elsewhere(&does, 7, 100, 200));
        assert!(elsewhere(&does, 8, 199, 300));
        assert!(!elsewhere(&does, 8, 200, 300));
    }

    /// A value written behind a flight, or a read-back, is in no host copy
    /// before it lands.
    #[test]
    fn what_no_host_copy_holds_is_refused() {
        let writers = [Writer { shadow: None, from: 100, to: 108, copy: false }];
        assert!(holder(&writers, 104, 120).is_err());
    }

    /// Reads are all before writes, between two buffers and within one, and
    /// the buffers are usable in the lists that follow in the same call.
    #[test]
    fn the_card_copies_what_is_recorded() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let (a, a_data) = host.shared_buffer(1 << 16).unwrap();
        let (b, b_data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: mappings of 64 KiB.
        let (a_bytes, b_bytes) =
            unsafe { (std::slice::from_raw_parts_mut(a_data, 1 << 16), std::slice::from_raw_parts_mut(b_data, 1 << 16)) };
        for (i, (x, y)) in a_bytes.iter_mut().zip(b_bytes.iter_mut()).enumerate() {
            (*x, *y) = (i as u8 ^ (i >> 8) as u8, 0xee);
        }
        let was = a_bytes.to_vec();
        let own = host.own_list().unwrap();
        // Within `a` (there and back), from `a` to `b`, and a read alone.
        record(
            true,
            &own.list.list,
            &own.between,
            &[(&a, 33, 0, 100), (&a, 2000, 256, 100), (&a, 5001, 512, 7), (&a, 9, 1024, 4)],
            &[(&a, 2000, 0, 100), (&a, 33, 256, 100), (&b, 77, 512, 7)],
        );
        // A level behind it, of what it wrote.
        record(false, &own.list.list, &own.between, &[(&b, 77, 2048, 7)], &[(&b, 1077, 2048, 7)]);
        own.list.close();
        // A list behind it that uses the buffers in other ways.
        let next = host.new_list().unwrap();
        let (out, out_data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: an open list; live buffers.
        unsafe {
            next.list.CopyBufferRegion(&out, 0, &own.between, 1024, 4);
            next.list.CopyBufferRegion(&b, 4096, &a, 33, 100);
        }
        next.close();
        host.execute(&[&own.list, &next]);
        // With the debug layer (`OATHKEEP_D3D12_DEBUG=1`, diagnostics).
        assert_eq!(host.log_messages(), None);
        assert_eq!(a_bytes[2000..2100], was[33..133]);
        assert_eq!(a_bytes[33..133], was[2000..2100]);
        assert_eq!(b_bytes[77..84], was[5001..5008]);
        assert_eq!(b_bytes[1077..1084], was[5001..5008]);
        assert_eq!(b_bytes[4096..4196], was[2000..2100]);
        // SAFETY: a mapping of 64 KiB.
        assert_eq!(unsafe { std::slice::from_raw_parts(out_data, 4) }, &was[9..13]);
        assert!(b_bytes[..77].iter().all(|&x| x == 0xee));
        a_bytes[2000..2100].copy_from_slice(&was[2000..2100]);
        a_bytes[33..133].copy_from_slice(&was[33..133]);
        assert_eq!(a_bytes, &was[..]);
        // As the landing does: its next use records anew.
        POOL.lock().unwrap().push(own);
        let own = host.own_list().unwrap();
        own.list.close();
    }

    /// The whole way, in a process of its own with the relay on:
    /// `OATHKEEP_RELAY=2 OATHKEEP_CARD_STEPS=2 cargo test -p oathkeep-gpu --lib
    /// card_steps -- --ignored --test-threads=1`. A flight's list writes
    /// part of what the next submission's copy reads, the title wrote the
    /// rest; the copy is done on the card behind the flight, and a second
    /// one reads what the first wrote. Title memory ends as on the CPU.
    #[test]
    #[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_CARD_STEPS=2 and a process of its own"]
    fn a_copy_behind_a_flight_reads_what_the_flight_wrote() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const LEN: usize = 4 << 20;
        // Without it (`TEST_UNWATCHED=1`) the title's bytes cannot be put in
        // under a flight's writes: the flights that declared them are waited
        // for.
        let write_watch = if std::env::var_os("TEST_UNWATCHED").is_some() { 0 } else { 0x20_0000 };
        let Some(host) = host() else {
            return;
        };
        assert!(relay_exact() && stage() == 2, "set OATHKEEP_RELAY=2 OATHKEEP_CARD_STEPS=2");
        // Two title buffers: in the first a copy within it, from it into the
        // second, and back.
        let (title, start) = {
            // SAFETY: memory of our own, never freed.
            let base = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | write_watch, PAGE_READWRITE) }.cast::<u8>();
            assert!(!base.is_null());
            // SAFETY: `LEN` bytes just committed.
            (unsafe { std::slice::from_raw_parts_mut(base, LEN) }, base as usize)
        };
        const HALF: usize = LEN / 2;
        crate::shadow::register(0x1111, start as u64, HALF as u64);
        crate::shadow::register(0x2222, (start + HALF) as u64, HALF as u64);
        let first = crate::shadow::find(start as u64, 64).unwrap();
        let (slow_from, slow_to) = (host.uav_buffer(64 << 20).unwrap(), host.uav_buffer(64 << 20).unwrap());
        let before = DONE[1].load(Ordering::Relaxed);
        let mut lists = Vec::new();
        for round in 0..8usize {
            // The title writes; then a "shader" of a flight writes 300 bytes
            // of what the copy reads, behind some work.
            let seed = |i: usize| (i * 7 + round * 13 + i / 251) as u8;
            for (i, b) in title[4000..6000].iter_mut().enumerate() {
                *b = seed(i);
            }
            let (upload, data) = host.upload_buffer(4096).unwrap();
            for i in 0..300 {
                // SAFETY: inside the mapping.
                unsafe { data.add(i).write(0x80 | (round * 31 + i) as u8) };
            }
            let a = host.new_list().unwrap();
            first.refresh(&a, start as u64, HALF as u64);
            first.gpu_writes(&a, start as u64, HALF as u64);
            // SAFETY: an open list; live buffers.
            unsafe {
                for _ in 0..24 {
                    a.list.CopyBufferRegion(&slow_to, 0, &slow_from, 0, 64 << 20);
                }
                a.list.CopyBufferRegion(&first.buffer, 4500, &upload, 0, 300);
                a.list
                    .ResourceBarrier(&[transition(&first.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
            }
            a.close();
            host.execute_own(&a);
            // With the flight in the air, beside what it writes.
            for (i, b) in title[5300..5400].iter_mut().enumerate() {
                *b = seed(1300 + i) ^ 0x5c;
            }
            // Thousands of runs of bytes of its own.
            for b in title[1_700_000..1_708_000].iter_mut().step_by(2) {
                *b = 1 + round as u8;
            }
            // Into the second buffer, and within the first.
            let b = host.new_list().unwrap();
            b.defer_copy((start + HALF + 100) as *mut u8, (start + 4001) as *const u8, 1500);
            b.defer_copy((start + 900_001) as *mut u8, (start + 4400) as *const u8, 777);
            b.defer_copy((start + HALF + 10_000) as *mut u8, (start + 1_700_000) as *const u8, 8000);
            // What depends on them: a copy of the first copy, upload memory
            // filled from that, and the first one's bytes copied over.
            b.defer_copy((start + 1_500_000) as *mut u8, (start + HALF + 600) as *const u8, 700);
            let (staging, staged) = b.upload(host, 4096).unwrap();
            b.defer_fill_buffer(&staging, staged, (start + 1_500_100) as *const u8, 500);
            b.defer_copy((start + HALF + 300) as *mut u8, (start + 8000) as *const u8, 64);
            b.close();
            host.execute_own(&b);
            // What the first wrote, read by the next submission.
            let c = host.new_list().unwrap();
            c.defer_copy((start + 1_200_000) as *mut u8, (start + HALF + 100) as *const u8, 1500);
            c.close();
            let number = host.execute_own(&c);
            settle_now(WHY_SYNC);
            assert!(FINISHED.load(Ordering::Acquire) >= number);
            let wanted: Vec<u8> = (0..2000)
                .map(|i| match i {
                    500..800 => 0x80 | (round * 31 + i - 500) as u8,
                    1300..1400 => seed(i) ^ 0x5c,
                    _ => seed(i),
                })
                .collect();
            assert!(title[4000..6000] == wanted[..], "what the flight wrote, round {round}");
            // (The copy over the first one's bytes came last.)
            let mut over = wanted[1..1501].to_vec();
            over[200..264].fill(0);
            assert!(title[HALF + 100..HALF + 1600] == over[..], "the copy to the other buffer, round {round}");
            assert!(title[1_500_000..1_500_700] == wanted[501..1201], "the copy behind a copy, round {round}");
            // SAFETY: the mapping, which the flight wrote and has landed.
            let staged = unsafe { std::slice::from_raw_parts(staged, 500) };
            assert!(staged == &wanted[601..1101], "the fill behind it, round {round}");
            assert!(title[900_001..900_778] == wanted[400..1177], "the copy within the buffer, round {round}");
            assert!(title[1_200_000..1_201_500] == over[..], "the copy of the copy, round {round}");
            assert!(title[HALF..HALF + 100].iter().all(|&x| x == 0));
            assert!(title[HALF + 1600..HALF + 10_000].iter().all(|&x| x == 0));
            assert!(title[HALF + 18_000..].iter().all(|&x| x == 0));
            let runs = &title[HALF + 10_000..HALF + 18_000];
            assert!(runs.chunks(2).all(|b| b == [1 + round as u8, 0]), "the runs, round {round}");
            a.keep(&upload);
            lists.extend([a, b, c]);
        }
        assert_eq!(host.log_messages(), None);
        let done = DONE[1].load(Ordering::Relaxed) - before;
        eprintln!("{done} of 48 copies were done on the card, {} submissions waited", DONE[3].load(Ordering::Relaxed));
        eprintln!("settles for title memory under shader writes: {}", LATE_STATS[WHY_STALE].load(Ordering::Relaxed));
        assert!(done >= 20, "only {done} copies were done on the card");
    }
}
