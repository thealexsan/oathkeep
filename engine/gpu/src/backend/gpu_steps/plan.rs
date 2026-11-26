use super::*;
use super::forwarding::Forwarding;
use super::levels::{assign_levels_direct, assign_levels_segtree};

/// Optional cross-list cutting metadata for [`cut_for`]: `(steps, run_starts, splittable)`
/// plus output `min_cut`.
pub(super) struct CutQuery<'a> {
    pub(super) steps: &'a [(usize, usize, usize)],
    pub(super) run_starts: &'a [usize],
    pub(super) splittable: &'a [bool],
    pub(super) min_cut: Option<usize>,
}

/// The steps of `does` (by place) to be done on the card, in order, each
/// with its level, plus the forwarding table; or why not.
///
/// `forward`: when `by_order` is on and cross-list cuts have already been
/// taken, steps behind a [`Does::Placed`] forward the overlapped sub-ranges of
/// their reads to that placed copy's source instead of forcing a CPU flight
/// wait, while leaving the placed copy's destination buffer untouched ahead of
/// the list.
pub(super) fn plan_inner(
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
pub(super) fn plan(does: &[Does], stage: u32, by_order: bool, behind: &mut Option<(usize, usize)>) -> Result<Vec<(usize, u32)>, &'static str> {
    plan_inner(does, stage, by_order, false, behind, None).map(|(out, _)| out)
}
