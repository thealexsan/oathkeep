use std::collections::{HashMap, HashSet};

use super::super::wire::{ALL, Barrier, Cmd, NONE, Va, slot};
use super::{INDIRECT_ARGUMENT, UNORDERED_ACCESS, find_clamps};

/// The root signature, pipeline and table of the batched clamp
/// (`crate::execution::indirect::batch`), and the scratch buffer it writes in the
/// first list of the frame (the next lists' are the ids before it), in lists
/// edited by [`batch_clamps`].
pub const BATCH_ROOT: u32 = u32::MAX - 2;
pub const BATCH: u32 = u32::MAX - 3;
pub const BATCH_JOBS: u32 = u32::MAX - 4;
pub const BATCH_SCRATCH: u32 = u32::MAX - 16;

/// What [`batch_clamps`] did to a list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Batched {
    /// Clamp passes before; after; the records the latter clamp.
    pub clamps: u32,
    pub batches: u32,
    pub jobs: u32,
    /// The bytes of the scratch buffer.
    pub scratch: u64,
}

/// A clamp batch being filled by [`batch_clamps`].
struct Batch {
    /// Where its table is in the jobs, and its root views in the list.
    table: usize,
    at: usize,
    sources: Vec<u32>,
    /// By source, offset and kind: the job, and how often it is read.
    known: HashMap<(u32, u64, u32), (u32, u32)>,
    n: u32,
    base: u64,
}

/// The list with its clamps done in batches, as `crate::execution::draw` does with
/// `OATHKEEP_INDIRECT_BATCH`: one pass at the first execution after a
/// point where arguments may have changed (a wait for shader writes, with
/// dispatches since the batch before it; or a draw, with dispatches since
/// the last wait). `careful`: before indirect dispatches too, with
/// dispatches since the last wait. `compute`: the pipelines that are compute
/// ones. The tables go to `jobs` (the contents of [`BATCH_JOBS`]).
pub fn batch_clamps(
    cmds: Vec<Cmd>,
    psos: &HashSet<u32>,
    compute: &HashSet<u32>,
    scratch: u32,
    careful: bool,
    jobs: &mut Vec<u8>,
) -> (Vec<Cmd>, Batched) {
    use crate::execution::indirect::batch::{JOBS, SLOT, SOURCES, TIMES, job};
    const DISPATCH: u32 = 2;
    let mut done = Batched::default();
    let scratches: HashSet<u32> = {
        let mut next = 0;
        let found = find_clamps(&cmds, psos, &mut next);
        found.iter().map(|c| c.scratch).collect()
    };
    if scratches.is_empty() {
        return (cmds, done);
    }
    let of_clamp =
        |b: &Barrier| matches!(b, Barrier::Transition { res, .. } if scratches.contains(res));
    let moved = |before, after| Barrier::Transition {
        flags: 0,
        res: scratch,
        sub: ALL,
        before,
        after,
    };
    // What the last clamp into a scratch buffer clamped: source, offset,
    // kind.
    let mut clamps: HashMap<u32, (u32, u64, u32)> = HashMap::new();
    let (mut bound, mut constants, mut views) = (NONE, [0u32; 6], [(NONE, 0u64); 5]);
    let mut out: Vec<Cmd> = Vec::with_capacity(cmds.len());
    let mut open: Option<Batch> = None;
    let (mut written, mut dirty, mut readable) = (true, true, false);
    let mut cmds = cmds.into_iter().peekable();
    while let Some(c) = cmds.next() {
        match &c {
            // A clamp's own commands: its root signature and all it sets.
            Cmd::RootSignature {
                graphics: false, ..
            } if matches!(cmds.peek(), Some(Cmd::Pipeline { pso }) if psos.contains(pso)) => {}
            Cmd::Pipeline { pso } => {
                bound = *pso;
                if !psos.contains(pso) {
                    out.push(c);
                }
            }
            Cmd::Constants {
                graphics: false,
                values,
                offset,
                ..
            } if psos.contains(&bound) => {
                for (i, v) in values.iter().enumerate() {
                    if let Some(c) = constants.get_mut(*offset as usize + i) {
                        *c = *v;
                    }
                }
            }
            Cmd::RootView {
                graphics: false,
                param,
                va: Va::At { res, offset },
                ..
            } if psos.contains(&bound) => {
                if let Some(v) = views.get_mut(*param as usize) {
                    *v = (*res, *offset);
                }
            }
            Cmd::Dispatch { .. } if psos.contains(&bound) => {
                done.clamps += 1;
                let (source, offset) = views[1];
                clamps.insert(
                    views[3].0,
                    (source, offset + u64::from(constants[3]), constants[0]),
                );
                // What it was clamped for follows: a batch is open by then.
                let full =
                    |o: &Batch| !o.sources.contains(&source) && o.sources.len() == SOURCES;
                if dirty && (constants[0] != DISPATCH || careful) || open.as_ref().is_some_and(full)
                {
                    open = None;
                }
                if open.is_some() {
                    continue;
                }
                done.batches += 1;
                let table = jobs.len();
                jobs.resize(table + 16 + 16 * 2048, 0);
                out.push(Cmd::Barriers {
                    all: vec![
                        Barrier::Uav {
                            flags: 0,
                            res: NONE,
                        },
                        moved(if readable { INDIRECT_ARGUMENT } else { 0 }, UNORDERED_ACCESS),
                    ],
                });
                readable = true;
                out.push(Cmd::RootSignature {
                    graphics: false,
                    root: BATCH_ROOT,
                });
                out.push(Cmd::Pipeline { pso: BATCH });
                let at = out.len();
                let mut view = |kind, param: usize, res, offset| {
                    out.push(Cmd::RootView {
                        graphics: false,
                        kind,
                        param: param as u32,
                        va: Va::At { res, offset },
                    });
                };
                for i in 0..SOURCES {
                    view(slot::SRV, i, source, 0);
                }
                view(slot::SRV, SOURCES, BATCH_JOBS, table as u64);
                view(slot::UAV, SOURCES + 1, scratch, 0);
                view(slot::UAV, SOURCES + 2, views[4].0, views[4].1);
                out.push(Cmd::Dispatch {
                    groups: [JOBS.div_ceil(64), 1, 1],
                });
                out.push(Cmd::Barriers {
                    all: vec![moved(UNORDERED_ACCESS, INDIRECT_ARGUMENT)],
                });
                open = Some(Batch {
                    table,
                    at,
                    sources: Vec::new(),
                    known: HashMap::new(),
                    n: 0,
                    base: done.scratch,
                });
                (written, dirty) = (false, false);
            }
            Cmd::Barriers { all } => {
                if all.iter().any(of_clamp) {
                    continue;
                }
                let waits = |b: &Barrier| matches!(b, Barrier::Uav { res: NONE, .. });
                if all.iter().any(waits) {
                    if written {
                        open = None;
                    }
                    dirty = false;
                }
                out.push(c);
            }
            Cmd::Indirect {
                signature,
                most,
                args,
                at,
                count,
                count_at,
            } if clamps.contains_key(args) => {
                let (source, offset, kind) = clamps.get(args).copied().unwrap_or_default();
                let Some(o) = &mut open else {
                    // Not clamped by any batch: left out, which shows.
                    continue;
                };
                let slot = match o.sources.iter().position(|&s| s == source) {
                    Some(i) => i,
                    None => {
                        o.sources.push(source);
                        let i = o.sources.len() - 1;
                        if let Some(Cmd::RootView {
                            va: Va::At { res, .. },
                            ..
                        }) = out.get_mut(o.at + i)
                        {
                            *res = source;
                        }
                        i
                    }
                };
                let offset = offset + at;
                let entry = o.known.entry((source, offset, kind)).or_insert((o.n, 0));
                entry.1 += 1;
                let (i, times) = *entry;
                if i == o.n {
                    o.n += 1;
                    done.jobs += 1;
                    done.scratch = o.base + u64::from(o.n * SLOT);
                    // Wider than the live pass where its table is longer.
                    if let Some(Cmd::Dispatch { groups }) = out.get_mut(o.at + SOURCES + 3) {
                        groups[0] = groups[0].max(o.n.div_ceil(64));
                    }
                }
                let place = o.base + u64::from(i * SLOT);
                let words = [
                    job(slot as u32, kind, times.min(TIMES)),
                    offset as u32,
                    place as u32,
                    0,
                ];
                let mut put = |at: usize, w: u32| {
                    jobs[o.table + at..o.table + at + 4].copy_from_slice(&w.to_le_bytes());
                };
                for (k, w) in words.into_iter().enumerate() {
                    put(16 + 16 * i as usize + 4 * k, w);
                }
                put(0, o.n);
                out.push(Cmd::Indirect {
                    signature: *signature,
                    most: *most,
                    args: scratch,
                    at: place,
                    count: *count,
                    count_at: *count_at,
                });
                if compute.contains(&bound) {
                    (written, dirty) = (true, true);
                }
            }
            Cmd::Dispatch { .. } => {
                (written, dirty) = (true, true);
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    if readable {
        out.push(Cmd::Barriers {
            all: vec![moved(INDIRECT_ARGUMENT, 0)],
        });
    }
    (out, done)
}

