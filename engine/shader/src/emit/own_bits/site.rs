use std::collections::{HashMap, HashSet};

use crate::gfx10::Fields;
use crate::ir::{EXEC, Exit, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit};

use super::super::lower;
use super::{Arg, CROSS, Ctx, EFFECT, EXIT, LEAVES, MASK, NO_LOOP, SHAPE, STALE, TEST, USED, User, Why, args, effect, other, result, used};

impl Ctx<'_> {
    pub(super) fn site(&self, sb: usize, t: Value, x: Value, mask: Value) -> Result<Vec<Value>, (usize, Why)> {
        let p = self.p;
        let st = self.st;
        let n = p.blocks.len();
        let here = p.blocks[sb].offset;
        let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
        // every lane that runs the section is one of the lanes ORed
        let &mb = st.masks.get(&mask).ok_or((here, MASK))?;
        if st.bools.meet(&[(st.reach[sb], true), (mb, false)]) {
            return Err((here, MASK));
        }
        // ---- the bits still to visit, and the loop's own steps ----
        let mut rem: HashSet<Value> = HashSet::from([t]);
        let mut pattern: HashSet<(usize, usize)> = HashSet::new();
        let mut alike: Vec<Value> = Vec::new();
        // an unguarded copy
        let copy = |u: (usize, usize)| -> Option<Value> {
            let i = self.inst(u);
            (matches!(i.name(), "v_mov_b32" | "s_mov_b32") && args(i).len() == 1 && st.guards[u.0][u.1].is_none())
                .then(|| result(i))
                .flatten()
        };
        let mut init = vec![t];
        // (SCC, whether it is set when the OR is empty)
        let mut zero: Option<(Value, bool)> = None;
        for u in self.readers(t) {
            let i = self.inst(u);
            let name = i.name();
            if matches!(name, "s_cmp_eq_u32" | "s_cmp_lg_u32" | "s_cmp_eq_i32" | "s_cmp_lg_i32")
                && other(i, t) == Some(Arg::K(0))
                && zero.is_none()
                && let Some(scc) = i.write(Reg::Scc)
            {
                zero = Some((scc, name.contains("_eq_")));
                rem.insert(scc);
            } else if let Some(w) = copy(u) {
                init.push(w);
                rem.insert(w);
                if matches!(i.writes[0].0, Reg::V(_)) {
                    alike.push(w);
                }
            } else {
                return Err((self.at(u), USED));
            }
            pattern.insert(u);
        }
        // the loop's join of them
        let mut join: Option<(usize, usize)> = None;
        for &v in &init {
            if v != t
                && let Some(&u) = self.readers(v).first()
            {
                return Err((self.at(u), USED));
            }
            for j in self.joins(v) {
                if join.is_some_and(|q| q != j) {
                    return Err((p.blocks[j.0].offset, USED));
                }
                join = Some(j);
            }
        }
        let (h, k) = join.ok_or((here, NO_LOOP))?;
        let phi = &p.blocks[h].phis[k];
        let r = phi.value;
        rem.insert(r);
        let head = p.blocks[h].offset;
        let lp = self.natural_loop(h);
        if !lp[h] || lp[sb] {
            return Err((head, NO_LOOP));
        }
        // the set as a scalar: the first lane's copy of it
        let cur = if matches!(phi.reg, Reg::V(_)) {
            alike.push(r);
            let [u] = self.readers(r)[..] else {
                return Err((head, USED));
            };
            let i = self.inst(u);
            if i.name() != "v_readfirstlane_b32" || args(i).len() != 1 || !lp[u.0] {
                return Err((self.at(u), USED));
            }
            pattern.insert(u);
            let c = result(i).ok_or((self.at(u), SHAPE))?;
            if !self.joins(r).is_empty() {
                return Err((head, USED));
            }
            c
        } else {
            r
        };
        rem.insert(cur);
        // its lowest bit, the set without it, the tests for "that was the last"
        let mut idx: Option<Value> = None;
        let mut next: Option<(Value, Value)> = None;
        // (SCC, whether it is set while bits remain)
        let mut more: Vec<(Value, bool)> = Vec::new();
        let mut bits: Vec<Value> = Vec::new();
        for u in self.readers(cur) {
            let i = self.inst(u);
            let name = i.name();
            if !lp[u.0] {
                return Err((self.at(u), USED));
            }
            match (name, &args(i)[..]) {
                ("s_ff1_i32_b32", [Arg::Val(_)]) if idx.is_none() => idx = result(i),
                ("s_xor_b32", _) | ("s_andn2_b32", [Arg::Val(_), Arg::Val(_)]) if next.is_none() => {
                    let bit = match (name, &args(i)[..]) {
                        ("s_andn2_b32", [Arg::Val(c), Arg::Val(b)]) if *c == cur && *b != cur => *b,
                        ("s_xor_b32", _) => match other(i, cur) {
                            Some(Arg::Val(b)) if b != cur => b,
                            _ => return Err((self.at(u), USED)),
                        },
                        _ => return Err((self.at(u), USED)),
                    };
                    let w = result(i).ok_or((self.at(u), SHAPE))?;
                    next = Some((w, bit));
                    bits.push(bit);
                    if let Some(scc) = i.write(Reg::Scc) {
                        more.push((scc, true));
                        rem.insert(scc);
                    }
                }
                ("s_cmp_eq_u32" | "s_cmp_lg_u32" | "s_cmp_eq_i32" | "s_cmp_lg_i32", _) => {
                    let (Some(Arg::Val(b)), Some(scc)) = (other(i, cur), i.write(Reg::Scc)) else {
                        return Err((self.at(u), USED));
                    };
                    bits.push(b);
                    more.push((scc, name.contains("_lg_")));
                    rem.insert(scc);
                }
                _ => return Err((self.at(u), USED)),
            }
            pattern.insert(u);
        }
        if cur != r && !self.joins(cur).is_empty() {
            return Err((head, USED));
        }
        let (Some(idx), Some((nx, bit))) = (idx, next) else {
            return Err((head, SHAPE));
        };
        if bits.iter().any(|&b| b != bit) {
            return Err((head, SHAPE));
        }
        // the bit: 1 shifted by the lowest set bit's number (a shift takes five bits of it)
        let made = |v: Value| self.def.get(&v).map(|&u| (u, self.inst(u)));
        let shifted = match made(bit) {
            Some((u, i)) if i.name() == "s_lshl_b32" && lp[u.0] => match args(i)[..] {
                [Arg::K(1), Arg::Val(sh)] => Some(sh),
                _ => None,
            },
            _ => None,
        };
        let by_idx = shifted.is_some_and(|sh| {
            sh == idx
                || made(sh).is_some_and(|(_, i)| i.name() == "s_and_b32" && other(i, idx) == Some(Arg::K(31)))
        });
        if !by_idx {
            return Err((head, SHAPE));
        }
        // the set less the bit: back into the join, or tested for empty
        rem.insert(nx);
        let mut nexts = vec![nx];
        for u in self.readers(nx) {
            let i = self.inst(u);
            let name = i.name();
            if matches!(name, "s_cmp_eq_u32" | "s_cmp_lg_u32" | "s_cmp_eq_i32" | "s_cmp_lg_i32")
                && other(i, nx) == Some(Arg::K(0))
                && let Some(scc) = i.write(Reg::Scc)
            {
                more.push((scc, name.contains("_lg_")));
                rem.insert(scc);
            } else if let Some(w) = copy(u) {
                nexts.push(w);
                rem.insert(w);
                if matches!(i.writes[0].0, Reg::V(_)) {
                    alike.push(w);
                }
            } else {
                return Err((self.at(u), USED));
            }
            pattern.insert(u);
        }
        for &v in &nexts {
            if v != nx
                && let Some(&u) = self.readers(v).first()
            {
                return Err((self.at(u), USED));
            }
            if self.joins(v).iter().any(|&j| j != (h, k)) {
                return Err((head, USED));
            }
        }
        for (q, a) in p.blocks[h].preds.iter().zip(&phi.args) {
            let ok = if lp[*q] { nexts.contains(a) } else { init.contains(a) };
            if !ok {
                return Err((head, SHAPE));
            }
        }
        // the loop goes on while bits remain and is left when none does
        let mut leaves: HashSet<usize> = HashSet::new();
        for &(scc, set_while_more) in &more {
            if !self.joins(scc).is_empty() {
                return Err((head, USED));
            }
            if self.readers(scc).is_empty() {
                continue;
            }
            let (u, set, clear) = self.branch(scc).ok_or((head, USED))?;
            let (on, out) = if set_while_more { (set, clear) } else { (clear, set) };
            if !lp[u.0] || !lp[on] || lp[out] {
                return Err((self.at(u), SHAPE));
            }
            leaves.insert(u.0);
            pattern.insert(u);
        }
        if leaves.is_empty() {
            return Err((head, SHAPE));
        }
        // "no bit at all" skips the loop; what lies between that test and the loop runs only
        // when there is one
        let mut pre = vec![false; n];
        if let Some((scc, set_when_empty)) = zero {
            if !self.joins(scc).is_empty() {
                return Err((here, USED));
            }
            if !self.readers(scc).is_empty() {
                let (u, set, clear) = self.branch(scc).ok_or((here, USED))?;
                let (none, some) = if set_when_empty { (set, clear) } else { (clear, set) };
                if none == some || lp[u.0] {
                    return Err((self.at(u), SHAPE));
                }
                if some != h {
                    if p.blocks[some].preds[..] != [u.0] || !self.dominates(some, h) {
                        return Err((self.at(u), SHAPE));
                    }
                    for b in 0..n {
                        pre[b] = self.dominates(some, b) && !self.dominates(h, b);
                    }
                    for b in (0..n).filter(|&b| pre[b]) {
                        let inside = match p.blocks[b].exit {
                            Exit::End | Exit::Indirect => false,
                            ref e => e.successors().iter().all(|&s| pre[s] || s == h),
                        };
                        if !inside {
                            return Err((p.blocks[b].offset, SHAPE));
                        }
                    }
                }
                pattern.insert(u);
            }
        }
        // ---- the lane's test of its own bit ----
        // (the atom of the compare, its block, its index)
        let mut tests: Vec<(Atom, usize, usize)> = Vec::new();
        for u in self.readers(bit) {
            let i = self.inst(u);
            if i.name() != "v_and_b32" || other(i, bit) != Some(Arg::Val(x)) || st.guards[u.0][u.1].is_some() {
                continue;
            }
            let Some(y) = result(i) else { continue };
            for c in self.readers(y) {
                let ci = self.inst(c);
                if matches!(
                    ci.name(),
                    "v_cmp_ne_u32" | "v_cmpx_ne_u32" | "v_cmp_lg_u32" | "v_cmpx_lg_u32" | "v_cmp_ne_i32" | "v_cmpx_ne_i32"
                        | "v_cmp_lg_i32" | "v_cmpx_lg_i32"
                ) && other(ci, y) == Some(Arg::K(0))
                    && lp[c.0]
                {
                    tests.extend(ci.writes.iter().map(|w| (Atom::Lane(w.1), c.0, c.1)));
                }
            }
        }
        if tests.is_empty() {
            return Err((head, TEST));
        }
        let mut memo: Vec<HashMap<Bool, bool>> = vec![HashMap::new(); tests.len()];
        // what a lane only reaches with its own bit set, in the round of that test
        let mut body_block = vec![false; n];
        for b in (0..n).filter(|&b| lp[b]) {
            body_block[b] = tests.iter().enumerate().any(|(j, &(a, tb, _))| {
                b != tb && self.dominates(tb, b) && self.implied(st.reach[b], a, &mut memo[j])
            });
        }
        let body_inst = |b: usize, i: usize, memo: &mut Vec<HashMap<Bool, bool>>| {
            body_block[b]
                || lp[b]
                    && st.guards[b][i].is_some_and(|g| {
                        tests.iter().enumerate().any(|(j, &(a, tb, ti))| {
                            (b == tb && i > ti || b != tb && self.dominates(tb, b)) && self.implied(g, a, &mut memo[j])
                        })
                    })
        };
        // ---- a round outside the body leaves nothing behind ----
        for b in (0..n).filter(|&b| lp[b] || pre[b]) {
            for (i, inst) in p.blocks[b].insts.iter().enumerate() {
                if lp[b] && body_inst(b, i, &mut memo) {
                    continue;
                }
                let u = (b, i);
                for &(reg, w) in &inst.writes {
                    if rem.contains(&w) || is_mask(w) || matches!(reg, Reg::S(c) if c == EXEC || c == EXEC + 1) {
                        continue;
                    }
                    for user in self.users.get(&w).into_iter().flatten() {
                        let stays = match *user {
                            User::Inst(b2, _) => lp[b2] || pre[b2],
                            User::Phi(b2, k2) => {
                                body_block[b2] || !self.live.contains(&p.blocks[b2].phis[k2].value)
                            }
                        };
                        if !stays {
                            return Err((self.at(u), LEAVES));
                        }
                    }
                }
                if pattern.contains(&u) {
                    continue;
                }
                if effect(inst) {
                    return Err((self.at(u), EFFECT));
                }
                let name = inst.name();
                let cross = name.starts_with("ds_swizzle")
                    || name.contains("permute")
                    || name.starts_with("v_mbcnt")
                    || name.contains("lane")
                    || matches!(&inst.fields, Fields::Valu(v) if v.dpp.is_some())
                    || lower::reads_mask_as_data(st, inst)
                    || self.sections[b].iter().any(|(&s, sec)| i >= s && i <= sec.end);
                if cross {
                    return Err((self.at(u), CROSS));
                }
                if used(p, st, b, i).iter().any(|v| rem.contains(v)) {
                    return Err((self.at(u), USED));
                }
            }
            // left only by "no bits left"; the body may leave it as it likes
            if lp[b] && !body_block[b] {
                let ok = match p.blocks[b].exit {
                    Exit::End | Exit::Indirect => false,
                    ref e => leaves.contains(&b) || e.successors().iter().all(|&s| lp[s]),
                };
                if !ok {
                    return Err((p.blocks[b].offset, EXIT));
                }
            }
        }
        // ---- no mask that depends on the test outlives its round ----
        let mut dep: HashSet<Atom> = tests.iter().map(|t| t.0).collect();
        let mut changed = true;
        while changed {
            changed = false;
            for (&id, edges) in &st.phi_masks {
                if !dep.contains(&Atom::Phi(id))
                    && edges.iter().any(|&e| st.bools.support(e).iter().any(|a| dep.contains(a)))
                {
                    dep.insert(Atom::Phi(id));
                    changed = true;
                }
            }
        }
        let mut seen: HashMap<Bool, bool> = HashMap::new();
        let mut tainted = |b: Bool| -> bool {
            *seen
                .entry(b)
                .or_insert_with(|| st.bools.support(b).iter().any(|a| dep.contains(a)))
        };
        for b in 0..n {
            if lp[b] && tests.iter().any(|&(_, tb, _)| self.dominates(tb, b)) {
                continue;
            }
            let stale = st.guards[b].iter().flatten().any(|&g| tainted(g))
                || matches!(st.exits[b], LaneExit::Branch { when, .. } if tainted(when));
            if stale {
                return Err((p.blocks[b].offset, STALE));
            }
        }
        for (&v, &m) in &st.masks {
            if !tainted(m) {
                continue;
            }
            for user in self.users.get(&v).into_iter().flatten() {
                let outside = match *user {
                    User::Inst(b2, _) => !lp[b2],
                    User::Phi(b2, k2) => !lp[b2] && self.live.contains(&p.blocks[b2].phis[k2].value),
                };
                if outside {
                    return Err((head, STALE));
                }
            }
        }
        Ok(alike)
    }
}
