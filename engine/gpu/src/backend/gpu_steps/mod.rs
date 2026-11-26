//! Steps of a submission that read title memory flights in the air may
//! still write, done on the card in the flights' order instead of on the
//! CPU after a wait for them (`OATHKEEP_CARD_STEPS`, see [`stage`]).

use std::sync::Arc;

use super::*;
use crate::memory::shadow::Shadow;

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
/// The landing tells the guards (`crate::memory::watch::landing`), so a texture
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
    if let Some(on) = crate::diagnostics::ab_toggle::pick("OATHKEEP_PLACED_ORDER") {
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
#[derive(Clone, Copy, Debug)]
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

mod analyze;
mod exec;
mod forwarding;
mod levels;
mod plan;
mod record;
#[cfg(test)]
mod tests;

pub(super) use analyze::cut_for;
pub(super) use record::{Taken, log};
