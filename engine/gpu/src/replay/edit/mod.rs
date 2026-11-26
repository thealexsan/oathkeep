//! Experiments on a list's commands: the same list with something left out
//! or put otherwise.

use std::collections::{HashMap, HashSet};

use super::wire::{ALL, Barrier, Cmd, NONE, Va, slot};

/// The buffer the clamped arguments of all clamp passes lie in, in lists
/// edited by [`clamps`].
pub const RESULTS: u32 = u32::MAX - 1;
/// The buffer texels are read back through (`player::pixel`).
pub const PROBE: u32 = u32::MAX - 2;
/// A buffer of the card's that depth goes through on the way there.
pub const SCRATCH: u32 = u32::MAX - 3;

const INDIRECT_ARGUMENT: i32 = 0x200;
const UNORDERED_ACCESS: i32 = 0x8;
const COPY_SOURCE: i32 = 0x800;

/// Drops what runs a pipeline of `psos`; how many commands that were.
pub fn skip_pipelines(cmds: Vec<Cmd>, psos: &HashSet<u32>) -> (Vec<Cmd>, u32) {
    let mut bound = NONE;
    let mut dropped = 0;
    let out = cmds
        .into_iter()
        .filter(|c| {
            if let Cmd::Pipeline { pso } = c {
                bound = *pso;
            }
            let drop = c.runs_shaders() && psos.contains(&bound);
            dropped += u32::from(drop);
            !drop
        })
        .collect();
    (out, dropped)
}

/// Drops unordered-access barriers; how many.
pub fn no_uav_barriers(cmds: Vec<Cmd>) -> (Vec<Cmd>, u32) {
    let mut dropped = 0;
    let out = cmds
        .into_iter()
        .filter_map(|c| match c {
            Cmd::Barriers { mut all } => {
                let n = all.len();
                all.retain(|b| !matches!(b, Barrier::Uav { .. }));
                dropped += (n - all.len()) as u32;
                (!all.is_empty()).then_some(Cmd::Barriers { all })
            }
            c => Some(c),
        })
        .collect();
    (out, dropped)
}

/// Barriers only: what a list left out still does to the states of
/// resources.
pub fn barriers_only(cmds: Vec<Cmd>) -> Vec<Cmd> {
    cmds.into_iter()
        .filter(|c| matches!(c, Cmd::Barriers { .. }))
        .collect()
}

/// Joins the barrier calls with no work between them into one; how many
/// calls that saved.
pub fn merge_barriers(cmds: Vec<Cmd>) -> (Vec<Cmd>, u32) {
    let mut out: Vec<Cmd> = Vec::with_capacity(cmds.len());
    let mut open: Option<usize> = None;
    let mut saved = 0;
    for c in cmds {
        match c {
            Cmd::Barriers { all } => {
                if let Some(i) = open
                    && let Cmd::Barriers { all: have } = &mut out[i]
                {
                    for b in all {
                        join(have, b);
                    }
                    saved += 1;
                } else {
                    open = Some(out.len());
                    let mut have = Vec::new();
                    for b in all {
                        join(&mut have, b);
                    }
                    out.push(Cmd::Barriers { all: have });
                }
            }
            c => {
                if c.works() || matches!(c, Cmd::BeginQuery { .. } | Cmd::EndQuery { .. }) {
                    open = None;
                }
                out.push(c);
            }
        }
    }
    out.retain(|c| !matches!(c, Cmd::Barriers { all } if all.is_empty()));
    (out, saved)
}

/// Adds a barrier to a call's.
fn join(have: &mut Vec<Barrier>, b: Barrier) {
    match b {
        Barrier::Uav { res, .. } => {
            let covered = have
                .iter()
                .any(|h| matches!(h, Barrier::Uav { res: r, .. } if *r == res || *r == NONE));
            if covered {
                return;
            }
            if res == NONE {
                have.retain(|h| !matches!(h, Barrier::Uav { .. }));
            }
            have.push(b);
        }
        Barrier::Transition {
            flags: 0,
            res,
            sub,
            before,
            after,
        } => {
            // The last one of this resource, if it is of the same part of it.
            let last = have
                .iter()
                .rposition(|h| matches!(h, Barrier::Transition { res: r, .. } if *r == res));
            if let Some(i) = last
                && let Barrier::Transition {
                    flags: 0,
                    sub: s,
                    before: first,
                    after: until,
                    ..
                } = have[i]
                && s == sub
                && until == before
            {
                if first != after {
                    have[i] = Barrier::Transition {
                        flags: 0,
                        res,
                        sub,
                        before: first,
                        after,
                    };
                } else {
                    have.remove(i);
                    if after == UNORDERED_ACCESS {
                        // Still a wait for the writes before.
                        join(have, Barrier::Uav { flags: 0, res });
                    }
                }
                return;
            }
            have.push(b);
        }
        b => have.push(b),
    }
}

/// A clamp pass of ours found in a list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clamp {
    /// The command that dispatches it.
    pub at: usize,
    /// The scratch buffer it writes the arguments to, from its start.
    pub scratch: u32,
    pub bytes: u64,
    /// Where its arguments are in [`RESULTS`].
    pub result: u64,
}

/// The clamp passes of a list (`psos`: the clamp's pipelines), given places
/// in the results from `next` on.
pub fn find_clamps(cmds: &[Cmd], psos: &HashSet<u32>, next: &mut u64) -> Vec<Clamp> {
    let (mut bound, mut constants, mut to) = (NONE, [0u32; 6], None);
    let mut found = Vec::new();
    for (at, c) in cmds.iter().enumerate() {
        match c {
            Cmd::Pipeline { pso } => bound = *pso,
            Cmd::Constants {
                graphics: false,
                param: 0,
                offset,
                values,
            } => {
                for (i, v) in values.iter().enumerate() {
                    if let Some(c) = constants.get_mut(*offset as usize + i) {
                        *c = *v;
                    }
                }
            }
            // The clamp's parameters: constants (kind, stride, records,
            // ...), arguments, count, then where it writes.
            Cmd::RootView {
                graphics: false,
                param: 3,
                va: Va::At { res, offset: 0 },
                ..
            } => to = Some(*res),
            Cmd::Dispatch { .. } if psos.contains(&bound) => {
                if let Some(scratch) = to {
                    let bytes = u64::from(constants[1]) * u64::from(constants[2]);
                    found.push(Clamp {
                        at,
                        scratch,
                        bytes,
                        result: *next,
                    });
                    *next += bytes.next_multiple_of(256);
                }
            }
            _ => {}
        }
    }
    found
}

/// The list with a copy of what each clamp wrote into the results.
pub fn keep_clamped(cmds: Vec<Cmd>, clamps: &[Clamp]) -> Vec<Cmd> {
    let mut out = Vec::with_capacity(cmds.len() + 3 * clamps.len());
    let mut open: Option<Clamp> = None;
    for (at, c) in cmds.into_iter().enumerate() {
        if let Some(clamp) = clamps.iter().find(|c| c.at == at) {
            open = Some(*clamp);
        }
        let readable = |b: &Barrier, scratch: u32| matches!(b, Barrier::Transition { res, after: INDIRECT_ARGUMENT, .. } if *res == scratch);
        let done = match (&c, open) {
            (Cmd::Barriers { all }, Some(clamp)) => all.iter().any(|b| readable(b, clamp.scratch)),
            _ => false,
        };
        out.push(c);
        if done && let Some(clamp) = open.take() {
            let moved = |before, after| Cmd::Barriers {
                all: vec![Barrier::Transition {
                    flags: 0,
                    res: clamp.scratch,
                    sub: ALL,
                    before,
                    after,
                }],
            };
            out.push(moved(INDIRECT_ARGUMENT, COPY_SOURCE));
            out.push(Cmd::CopyBuffer {
                dst: RESULTS,
                dst_at: clamp.result,
                src: clamp.scratch,
                src_at: 0,
                len: clamp.bytes,
            });
            out.push(moved(COPY_SOURCE, INDIRECT_ARGUMENT));
        }
    }
    out
}

/// The list without its clamp passes and the barriers of their scratch
/// buffers; what they bounded is read from the results instead.
pub fn skip_clamps(cmds: Vec<Cmd>, clamps: &[Clamp]) -> Vec<Cmd> {
    let scratches: HashSet<u32> = clamps.iter().map(|c| c.scratch).collect();
    let by_at: HashMap<usize, &Clamp> = clamps.iter().map(|c| (c.at, c)).collect();
    let mut last: HashMap<u32, u64> = HashMap::new();
    let mut out = Vec::with_capacity(cmds.len());
    for (at, c) in cmds.into_iter().enumerate() {
        if let Some(clamp) = by_at.get(&at) {
            last.insert(clamp.scratch, clamp.result);
            continue;
        }
        match c {
            Cmd::Barriers { mut all } => {
                let of_scratch = |b: &Barrier| matches!(b, Barrier::Transition { res, .. } if scratches.contains(res));
                if all.iter().any(of_scratch) {
                    // The waits made with the scratch buffer's moves are the
                    // clamp's too.
                    all.retain(|b| !of_scratch(b) && !matches!(b, Barrier::Uav { res: NONE, .. }));
                }
                if !all.is_empty() {
                    out.push(Cmd::Barriers { all });
                }
            }
            Cmd::Indirect {
                signature,
                most,
                args,
                at,
                count,
                count_at,
            } if last.contains_key(&args) => {
                let result = last.get(&args).copied().unwrap_or(0);
                out.push(Cmd::Indirect {
                    signature,
                    most,
                    args: RESULTS,
                    at: result + at,
                    count,
                    count_at,
                });
            }
            c => out.push(c),
        }
    }
    out
}


mod batch;
pub use batch::*;

#[cfg(test)]
mod tests;
