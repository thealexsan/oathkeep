use super::*;
use super::plan::{CutQuery, plan_inner};

/// Merged per-shadow refresh ranges across a submission, built lazily only when
/// [`Host::take_steps`] actually takes a [`Does::Copy`].
pub(super) struct RefreshSet {
    by_shadow: ByCopy<Vec<(usize, usize)>>,
    min_from: usize,
    max_to: usize,
}

impl RefreshSet {
    pub(super) fn from_runs(runs: &[&HostList]) -> Self {
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
    pub(super) fn elsewhere(&self, shadow: usize, from: usize, to: usize) -> bool {
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
pub(super) fn elsewhere(does: &[Does], shadow: usize, from: usize, to: usize) -> bool {
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
pub(super) fn holder(writers: &[Writer], from: usize, to: usize) -> Result<Arc<Shadow>, &'static str> {
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
pub(super) fn does_of(written: &[(usize, usize)], runs: &[&HostList]) -> (Vec<Does>, Vec<(usize, usize, usize)>, Vec<usize>) {
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
pub(super) fn splittable_cuts(runs: &[&HostList]) -> Vec<bool> {
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
pub(in crate::host) fn cut_for(runs: &[&HostList]) -> Option<usize> {
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
pub(super) fn mapped(from: usize, to: usize, known: &mut HashMap<usize, ()>) -> bool {
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
pub(super) type HashMap<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<AddressHash>>;

/// Where the bytes of the fill that is `step` (its run index, its list by
/// address, its place in it) go.
pub(super) fn fill_to(runs: &[&HostList], step: (usize, usize, usize)) -> Option<FillTo> {
    let list = runs.get(step.0)?;
    match list.deferred().copies.get(step.2)? {
        Step::Fill(_, _, _, to) => to.clone(),
        _ => None,
    }
}

