use std::collections::{HashMap, HashSet};

use super::mask_analysis::all_lane_blocks;
use super::thread_model::ThreadModel;
use crate::gfx10::{Fields, NULL, Operand};
use crate::io::Io;
use crate::ir::{EXEC, Inst, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit, Structured};

/// Static LDS byte-range summary for proving disjointness between LDS reads
/// and LDS atomics within a divergent region.
#[derive(Clone, Copy)]
enum LdsRange {
    Exact(u32, u32),
    Min(u32),
    Any,
}

impl LdsRange {
    fn disjoint(self, other: Self) -> bool {
        match (self, other) {
            (LdsRange::Exact(a, la), LdsRange::Exact(b, lb)) => {
                a.saturating_add(la) <= b || b.saturating_add(lb) <= a
            }
            (LdsRange::Exact(a, la), LdsRange::Min(mb)) => a.saturating_add(la) <= mb,
            (LdsRange::Min(ma), LdsRange::Exact(b, lb)) => b.saturating_add(lb) <= ma,
            _ => false,
        }
    }
}

/// Checks whether multi-wave (`group > 64`) widened execution is provably exact
/// and deadlock-free: every widened branch and every guarded scalar branch in an
/// all-lane block must have at most one active console wave (`exit_exec`), and
/// no non-mask SGPR written inside a widened branch's region may be read outside
/// that region.
pub(in crate::emit) fn multi_wave_wide_safe(
    p: &crate::ir::Program,
    st: &Structured,
    io: &Io,
    threads: [u32; 3],
) -> bool {
    let group = threads.iter().product::<u32>();
    if group <= 64 || group % 64 != 0 || group > 1024 || st.wide.is_empty() {
        return false;
    }
    let Some(all) = all_lane_blocks(st) else {
        return false;
    };
    let model = ThreadModel::new(p, io, threads);
    for (b, exit) in st.exits.iter().enumerate() {
        let LaneExit::Branch { when, .. } = *exit else {
            continue;
        };
        let wide = st.wide.contains(&b);
        let guarded_scalar = !wide
            && all[b]
            && st.exit_exec[b] != Bool::TRUE
            && st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_)));
        if (wide || guarded_scalar) && !model.single_console_wave(st, st.exit_exec[b]) {
            return false;
        }
    }
    // Check that no non-mask SGPR written inside any widened branch's CFG region
    // is read outside that region (dragging an inactive console wave through a
    // widened branch could otherwise clobber a live scalar).
    let n = st.exits.len();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, e) in st.exits.iter().enumerate() {
        match *e {
            LaneExit::Jump(t) => succ[b].push(t),
            LaneExit::Branch { taken, not_taken, .. } => {
                succ[b].push(taken);
                succ[b].push(not_taken);
            }
            LaneExit::End => {}
            LaneExit::Indirect => return false,
        }
    }
    let words = n.div_ceil(64);
    let full = vec![u64::MAX; words];
    let mut pdom: Vec<Vec<u64>> = (0..n)
        .map(|b| {
            if succ[b].is_empty() {
                let mut v = vec![0; words];
                v[b / 64] |= 1 << (b % 64);
                v
            } else {
                full.clone()
            }
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            if succ[b].is_empty() {
                continue;
            }
            let mut v = full.clone();
            for &t in &succ[b] {
                for w in 0..words {
                    v[w] &= pdom[t][w];
                }
            }
            v[b / 64] |= 1 << (b % 64);
            if v != pdom[b] {
                pdom[b] = v;
                changed = true;
            }
        }
    }
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    for &d in &st.wide {
        // Compute blocks in `d`'s region (reachable from `d`, not post-dominating `d`).
        let mut inside = vec![false; n];
        let mut stack = succ[d].clone();
        while let Some(b) = stack.pop() {
            if inside[b] || pdom[d][b / 64] >> (b % 64) & 1 == 1 {
                continue;
            }
            inside[b] = true;
            stack.extend(succ[b].iter().copied());
        }
        // Only check outermost widened regions whose own entry may have another
        // console wave live after exit: nested widened branches inside a single-
        // wave region already share the outer region's live-out check.
        let mut written_sgprs: HashSet<Value> = HashSet::new();
        for (b, block) in p.blocks.iter().enumerate() {
            if !inside[b] {
                continue;
            }
            for phi in &block.phis {
                if matches!(phi.reg, Reg::S(c) if c != EXEC && c != EXEC + 1) && !is_mask(phi.value) {
                    written_sgprs.insert(phi.value);
                }
            }
            for inst in &block.insts {
                for &(r, v) in &inst.writes {
                    if matches!(r, Reg::S(c) if c != EXEC && c != EXEC + 1) && !is_mask(v) {
                        written_sgprs.insert(v);
                    }
                }
            }
        }
        if written_sgprs.is_empty() {
            continue;
        }
        // If `d` itself is nested inside another widened branch `outer` with the
        // same single active wave, only check reads outside `outer`; checking
        // directly outside `d` is still sound whenever no scalar escapes `d`,
        // unless `d` is an inner branch whose scalar feeds the same wave's code.
        // Skip inner branches that already sit strictly inside an enclosing widened
        // branch's region (their enclosing region checks everything that leaves).
        let enclosed = st.wide.iter().any(|&o| {
            if o == d {
                return false;
            }
            let mut seen = vec![false; n];
            let mut st_o = succ[o].clone();
            while let Some(b) = st_o.pop() {
                if seen[b] || pdom[o][b / 64] >> (b % 64) & 1 == 1 {
                    continue;
                }
                seen[b] = true;
                if b == d {
                    return true;
                }
                st_o.extend(succ[b].iter().copied());
            }
            false
        });
        if enclosed {
            continue;
        }
        for (b, block) in p.blocks.iter().enumerate() {
            if inside[b] {
                continue;
            }
            for phi in &block.phis {
                if matches!(phi.reg, Reg::S(c) if c != EXEC && c != EXEC + 1)
                    && !is_mask(phi.value)
                    && phi.args.iter().any(|a| written_sgprs.contains(a))
                {
                    return false;
                }
            }
            for inst in &block.insts {
                if inst.reads.iter().any(|&(_, v)| written_sgprs.contains(&v)) {
                    return false;
                }
            }
        }
    }
    true
}

/// LDS accesses of a console wave run as two host waves, where some sit in blocks that only
/// some lanes run (`all` is [`all_lane_blocks`]): per IR block, whether the host waves meet (a
/// group barrier) at its start and before its exit, or `Err(blocks)` naming the LDS-touching
/// blocks in regions whose accesses barriers alone cannot order.
///
/// The console runs the wave's lanes in step, so an LDS write is seen by every later read. Two
/// host waves keep that order if they meet between any two accesses of which one writes. A
/// barrier must be run by every thread, so it cannot stand in a lane branch's region; but
/// every way into and out of such a region passes a block all lanes run, and the waves meet
/// there: before the exit of an all-lane block into a region with LDS accesses, and at the
/// start of an all-lane block entered from one. Accesses in all-lane blocks get their own
/// barrier as before. What that cannot order is a write and a read inside one region (blocks
/// joined without an all-lane block between them): refused, unless:
/// - every block with LDS accesses in that region runs in at most one 32-lane half of each
///   console wave (e.g. a single-thread guard `tid == 0` or `tid == 63`), or
/// - there are no non-atomic `ds_write` instructions in the region, every non-CAS atomic is
///   provably byte-disjoint from all reads in the region, and every `ds_cmpst_rtn_b32` takes
///   its expected operand from an LDS read in the region (lock-free CAS retry loop).
/// shortcut: two writes in one region are taken not to hit the same dword from both halves.
pub(super) fn lds_meetings(
    p: &crate::ir::Program,
    st: &Structured,
    io: &Io,
    threads: [u32; 3],
    all: &[bool],
) -> Result<Vec<(bool, bool)>, Vec<usize>> {
    let n = st.exits.len();
    let succ = |b: usize| -> Vec<usize> {
        match st.exits[b] {
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch { taken, not_taken, .. } => vec![taken, not_taken],
            LaneExit::End | LaneExit::Indirect => Vec::new(),
        }
    };
    // regions: blocks not every lane runs, joined by the edges between them
    let mut region: Vec<usize> = (0..n).collect();
    fn find(region: &mut [usize], mut b: usize) -> usize {
        while region[b] != b {
            region[b] = region[region[b]];
            b = region[b];
        }
        b
    }
    for b in 0..n {
        for t in succ(b) {
            if !all[b] && !all[t] {
                let (x, y) = (find(&mut region, b), find(&mut region, t));
                region[x] = y;
            }
        }
    }
    // Lightweight constant / minimum byte-offset tracking for LDS addresses.
    let mut const_val: HashMap<Value, u32> = HashMap::new();
    let mut min_val: HashMap<Value, u32> = HashMap::new();
    let op_const = |inst: &Inst, op: &Operand, cv: &HashMap<Value, u32>| -> Option<u32> {
        match *op {
            Operand::Vector { reg, .. } => cv.get(&inst.read(Reg::V(reg))?).copied(),
            Operand::Scalar { code, .. } if code != NULL => {
                cv.get(&inst.read(Reg::S(code))?).copied()
            }
            Operand::Const(c) => crate::resource::inline(c),
            Operand::Literal(k) => Some(k),
            _ => None,
        }
    };
    let op_min = |inst: &Inst, op: &Operand, mv: &HashMap<Value, u32>| -> Option<u32> {
        match *op {
            Operand::Vector { reg, .. } => mv.get(&inst.read(Reg::V(reg))?).copied(),
            Operand::Scalar { code, .. } if code != NULL => {
                mv.get(&inst.read(Reg::S(code))?).copied()
            }
            Operand::Const(c) => crate::resource::inline(c),
            Operand::Literal(k) => Some(k),
            _ => None,
        }
    };
    for &b in &crate::ir::reverse_postorder(&p.blocks) {
        for inst in &p.blocks[b].insts {
            let name = inst.name();
            let (dst, src): (Option<Value>, &[Operand]) = match &inst.fields {
                Fields::Valu(v) => (
                    match v.dst {
                        Some(Operand::Vector { reg, .. }) => inst.write(Reg::V(reg)),
                        _ => None,
                    },
                    &v.src,
                ),
                Fields::Sop { dst, src, .. } => (
                    match dst {
                        Some(Operand::Scalar { code, .. }) => inst.write(Reg::S(*code)),
                        _ => None,
                    },
                    src,
                ),
                _ => continue,
            };
            let Some(dst) = dst else { continue };
            match (name, src) {
                ("v_mov_b32" | "s_mov_b32", [a]) => {
                    if let Some(k) = op_const(inst, a, &const_val) {
                        const_val.insert(dst, k);
                        min_val.insert(dst, k);
                    } else if let Some(m) = op_min(inst, a, &min_val) {
                        min_val.insert(dst, m);
                    }
                }
                ("v_add_nc_u32" | "s_add_u32" | "s_add_i32", [a, b_op]) => {
                    match (op_const(inst, a, &const_val), op_const(inst, b_op, &const_val)) {
                        (Some(x), Some(y)) => {
                            let s = x.wrapping_add(y);
                            const_val.insert(dst, s);
                            min_val.insert(dst, s);
                        }
                        (Some(k), None) if k <= 0x10000 => {
                            let m = op_min(inst, b_op, &min_val).unwrap_or(0);
                            if let Some(s) = m.checked_add(k) {
                                min_val.insert(dst, s);
                            }
                        }
                        (None, Some(k)) if k <= 0x10000 => {
                            let m = op_min(inst, a, &min_val).unwrap_or(0);
                            if let Some(s) = m.checked_add(k) {
                                min_val.insert(dst, s);
                            }
                        }
                        _ => {}
                    }
                }
                ("v_and_b32" | "s_and_b32", [a, b_op]) => {
                    match (op_const(inst, a, &const_val), op_const(inst, b_op, &const_val)) {
                        (Some(x), Some(y)) => {
                            const_val.insert(dst, x & y);
                            min_val.insert(dst, x & y);
                        }
                        (Some(m), None) | (None, Some(m)) if m <= 0x7FFF_FFFF => {
                            min_val.insert(dst, 0);
                        }
                        _ => {}
                    }
                }
                ("v_lshlrev_b32" | "s_lshl_b32", _) => {
                    min_val.insert(dst, 0);
                }
                _ => {}
            }
        }
    }
    let ds_ranges = |inst: &Inst| -> Vec<LdsRange> {
        let Fields::Ds { addr, offset0, offset1, .. } = &inst.fields else {
            return Vec::new();
        };
        let name = inst.name();
        let offset = u32::from(*offset0) | u32::from(*offset1) << 8;
        let width = match name.rsplit('_').next() {
            Some("b64") => 2u32,
            Some("b96") => 3,
            Some("b128") => 4,
            _ => 1,
        };
        let elems: Vec<(u32, u32)> = if name.starts_with("ds_read2") || name.starts_with("ds_write2") {
            let scale = 4 * width * if name.contains("st64") { 64 } else { 1 };
            vec![
                (u32::from(*offset0) * scale, 4 * width),
                (u32::from(*offset1) * scale, 4 * width),
            ]
        } else {
            vec![(offset, 4 * width)]
        };
        let addr_const = addr.as_ref().and_then(|o| op_const(inst, o, &const_val));
        let addr_min = addr.as_ref().and_then(|o| op_min(inst, o, &min_val));
        elems
            .into_iter()
            .map(|(off, len)| {
                if let Some(base) = addr_const {
                    LdsRange::Exact(base.wrapping_add(off), len)
                } else if let Some(m) = addr_min.and_then(|m| m.checked_add(off)) {
                    LdsRange::Min(m)
                } else if off > 0 {
                    LdsRange::Min(off)
                } else {
                    LdsRange::Any
                }
            })
            .collect()
    };
    // Per region: LDS reads, LDS writes, blocks touching LDS, and ds_read result values.
    let mut reads: Vec<Vec<LdsRange>> = vec![Vec::new(); n];
    let mut read_vals: Vec<HashSet<Value>> = vec![HashSet::new(); n];
    // Writes: (is_non_atomic, is_cas, cas_expected_val, ranges)
    let mut writes: Vec<Vec<(bool, bool, Option<Value>, Vec<LdsRange>)>> = vec![Vec::new(); n];
    let mut blocks_in: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, block) in p.blocks.iter().enumerate().take(n) {
        if all[b] {
            continue;
        }
        let r = find(&mut region, b);
        let mut touches = false;
        for i in &block.insts {
            let name = i.name();
            if !matches!(i.fields, Fields::Ds { .. }) || name.starts_with("ds_swizzle") || name.contains("permute") {
                continue;
            }
            touches = true;
            let ranges = ds_ranges(i);
            if name.starts_with("ds_read") {
                reads[r].extend(ranges);
                for &(_, w) in &i.writes {
                    read_vals[r].insert(w);
                }
            } else {
                let non_atomic = name.starts_with("ds_write");
                let is_cas = name.starts_with("ds_cmpst");
                let expected = if is_cas
                    && let Fields::Ds { data, .. } = &i.fields
                    && let Some(Operand::Vector { reg, .. }) = data.first()
                {
                    i.read(Reg::V(*reg))
                } else {
                    None
                };
                writes[r].push((non_atomic, is_cas, expected, ranges));
            }
        }
        if touches {
            blocks_in[r].push(b);
        }
    }
    let model = ThreadModel::new(p, io, threads);
    let mut bad: Vec<usize> = Vec::new();
    for r in 0..n {
        if reads[r].is_empty() || writes[r].is_empty() {
            continue;
        }
        // Single-half-per-console-wave blocks never run concurrently in both halves
        // of the same console wave.
        if blocks_in[r]
            .iter()
            .all(|&b| model.single_half_per_wave(st, st.reach[b]))
        {
            continue;
        }
        // Atomic-only regions where every non-CAS atomic is byte-disjoint from all
        // reads and every CAS (`ds_cmpst_rtn_b32`) compares against an LDS read
        // from this region (lock-free CAS retry loop).
        let atomic_safe = writes[r].iter().all(|(non_atomic, is_cas, expected, wranges)| {
            if *non_atomic {
                return false;
            }
            if *is_cas {
                return expected.is_some_and(|v| read_vals[r].contains(&v));
            }
            wranges
                .iter()
                .all(|&wr| reads[r].iter().all(|&rr| wr.disjoint(rr)))
        });
        if !atomic_safe {
            bad.extend_from_slice(&blocks_in[r]);
            continue;
        }
    }
    if !bad.is_empty() {
        return Err(bad);
    }
    let mut meet = vec![(false, false); n];
    for b in 0..n {
        for t in succ(b) {
            if all[b] && !all[t] {
                let r = find(&mut region, t);
                meet[b].1 |= !reads[r].is_empty() || !writes[r].is_empty();
            } else if !all[b] && all[t] {
                let r = find(&mut region, b);
                meet[t].0 |= !reads[r].is_empty() || !writes[r].is_empty();
            }
        }
    }
    Ok(meet)
}
