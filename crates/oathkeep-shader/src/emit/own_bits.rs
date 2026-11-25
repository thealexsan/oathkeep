use std::collections::{HashMap, HashSet};

use super::wave::{self, Op, Shape};
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Exit, Inst, Program, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit, Structured};

type Why = &'static str;

const USED: Why = "a wave OR used as data";
const NO_LOOP: Why = "a wave OR that steers no loop over its bits";
const SHAPE: Why = "a loop over a wave OR's bits of another shape";
const TEST: Why = "a loop over a wave OR's bits whose lanes do not test their own bit";
const LEAVES: Why = "a value of a round over a wave OR's bits that outlives the round";
const EFFECT: Why = "an effect in a loop over a wave OR's bits outside the lanes' own bits";
const CROSS: Why = "a cross-lane step in a loop over a wave OR's bits outside the lanes' own bits";
const EXIT: Why = "a loop over a wave OR's bits left another way";
const STALE: Why = "a lane's own-bit test used outside its round";
const MASK: Why = "a wave OR over a mask a running lane may be outside";

#[derive(Debug, Default)]
pub(super) struct OwnBits {
    pub totals: HashSet<Value>,
    pub alike: HashSet<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum User {
    Inst(usize, usize),
    Phi(usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arg {
    Val(Value),
    K(u32),
    Other,
}

struct Ctx<'a> {
    p: &'a Program,
    st: &'a Structured,
    users: HashMap<Value, Vec<User>>,
    def: HashMap<Value, (usize, usize)>,
    live: HashSet<Value>,
    idom: Vec<usize>,
    sections: Vec<HashMap<usize, wave::Section>>,
}

fn used(p: &Program, st: &Structured, b: usize, i: usize) -> Vec<Value> {
    let inst = &p.blocks[b].insts[i];
    let ops = crate::ir::operand_reads(p, inst);
    let guarded = st.guards[b][i].is_some();
    inst.reads
        .iter()
        .filter(|&&(r, _)| match r {
            Reg::S(c) if c == EXEC || c == EXEC + 1 => false,
            Reg::V(_) => ops.contains(&r) || guarded,
            _ => true,
        })
        .map(|r| r.1)
        .collect()
}

fn value(inst: &Inst, op: &Operand) -> Option<Value> {
    match *op {
        Operand::Scalar { code, .. } if code != NULL => inst.read(Reg::S(code)),
        Operand::Vector { reg, .. } => inst.read(Reg::V(reg)),
        _ => None,
    }
}

fn args(inst: &Inst) -> Vec<Arg> {
    let src: &[Operand] = match &inst.fields {
        Fields::Sop { src, .. } => src,
        Fields::Valu(v)
            if v.dpp.is_none()
                && v.sdwa.is_none()
                && v.abs == 0
                && v.neg == 0
                && !v.clamp
                && v.omod == 0 =>
        {
            &v.src
        }
        _ => &[],
    };
    src.iter()
        .map(|o| match (value(inst, o), o) {
            (Some(v), _) => Arg::Val(v),
            (None, &Operand::Const(c)) => crate::resource::inline(c).map_or(Arg::Other, Arg::K),
            (None, &Operand::Literal(x)) => Arg::K(x),
            _ => Arg::Other,
        })
        .collect()
}

fn other(inst: &Inst, a: Value) -> Option<Arg> {
    match args(inst)[..] {
        [Arg::Val(x), o] if x == a => Some(o),
        [o, Arg::Val(x)] if x == a => Some(o),
        _ => None,
    }
}

fn result(inst: &Inst) -> Option<Value> {
    let mut w = inst.writes.iter().filter(|w| w.0 != Reg::Scc);
    match (w.next(), w.next()) {
        (Some(&(_, v)), None) => Some(v),
        _ => None,
    }
}

fn effect(inst: &Inst) -> bool {
    let name = inst.name();
    matches!(inst.fields, Fields::Exp { .. })
        || matches!(inst.fields, Fields::Flat { .. }) && !name.starts_with("global_load_")
        || name.contains("store")
        || name.contains("atomic")
        || matches!(inst.fields, Fields::Ds { .. })
            && !(name.starts_with("ds_read")
                || name.starts_with("ds_swizzle")
                || name.contains("permute"))
        || matches!(name, "s_barrier" | "s_sendmsg" | "s_sendmsghalt")
}

impl Ctx<'_> {
    fn new<'a>(p: &'a Program, st: &'a Structured) -> Ctx<'a> {
        let mut users: HashMap<Value, Vec<User>> = HashMap::new();
        let mut def = HashMap::new();
        let mut live: HashSet<Value> = HashSet::new();
        for (b, block) in p.blocks.iter().enumerate() {
            for (k, phi) in block.phis.iter().enumerate() {
                for &a in &phi.args {
                    users.entry(a).or_default().push(User::Phi(b, k));
                }
            }
            for (i, inst) in block.insts.iter().enumerate() {
                for v in used(p, st, b, i) {
                    users.entry(v).or_default().push(User::Inst(b, i));
                    live.insert(v);
                }
                for &(_, w) in &inst.writes {
                    def.insert(w, (b, i));
                }
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for block in &p.blocks {
                for phi in &block.phis {
                    if live.contains(&phi.value) {
                        for &a in &phi.args {
                            changed |= live.insert(a);
                        }
                    }
                }
            }
        }
        let order = crate::ir::reverse_postorder(&p.blocks);
        let mut index = vec![usize::MAX; p.blocks.len()];
        for (n, &b) in order.iter().enumerate() {
            index[b] = n;
        }
        let idom = crate::ir::idoms(&p.blocks, &order, &index);
        let sections = (0..p.blocks.len()).map(|b| wave::sections(p, b)).collect();
        Ctx {
            p,
            st,
            users,
            def,
            live,
            idom,
            sections,
        }
    }

    fn inst(&self, u: (usize, usize)) -> &Inst {
        &self.p.blocks[u.0].insts[u.1]
    }

    fn at(&self, u: (usize, usize)) -> usize {
        self.inst(u).machine.offset
    }

    fn dominates(&self, a: usize, mut b: usize) -> bool {
        loop {
            if a == b {
                return true;
            }
            let d = self.idom.get(b).copied().unwrap_or(usize::MAX);
            if d == b || d == usize::MAX {
                return false;
            }
            b = d;
        }
    }

    fn readers(&self, v: Value) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = self
            .users
            .get(&v)
            .into_iter()
            .flatten()
            .filter_map(|u| match *u {
                User::Inst(b, i) => Some((b, i)),
                User::Phi(..) => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn joins(&self, v: Value) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = self
            .users
            .get(&v)
            .into_iter()
            .flatten()
            .filter_map(|u| match *u {
                User::Phi(b, k) if self.live.contains(&self.p.blocks[b].phis[k].value) => {
                    Some((b, k))
                }
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn natural_loop(&self, h: usize) -> Vec<bool> {
        let mut inside = vec![false; self.p.blocks.len()];
        let mut work: Vec<usize> = self.p.blocks[h]
            .preds
            .iter()
            .copied()
            .filter(|&q| self.dominates(h, q))
            .collect();
        if work.is_empty() {
            return inside;
        }
        inside[h] = true;
        while let Some(b) = work.pop() {
            if inside[b] {
                continue;
            }
            inside[b] = true;
            work.extend(self.p.blocks[b].preds.iter().copied());
        }
        inside
    }

    fn implied(&self, b: Bool, a: Atom, memo: &mut HashMap<Bool, bool>) -> bool {
        if b == Bool::FALSE {
            return true;
        }
        if let Some(&r) = memo.get(&b) {
            return r;
        }
        let r = match self.st.bools.node(b) {
            None => false,
            Some((x, lo, _)) if x == a => lo == Bool::FALSE,
            Some((_, lo, hi)) => self.implied(lo, a, memo) && self.implied(hi, a, memo),
        };
        memo.insert(b, r);
        r
    }

    fn branch(&self, scc: Value) -> Option<((usize, usize), usize, usize)> {
        let [u] = self.readers(scc)[..] else {
            return None;
        };
        let Exit::Branch { taken, not_taken } = self.p.blocks[u.0].exit else {
            return None;
        };
        if u.1 + 1 != self.p.blocks[u.0].insts.len() {
            return None;
        }
        match self.inst(u).name() {
            "s_cbranch_scc1" => Some((u, taken, not_taken)),
            "s_cbranch_scc0" => Some((u, not_taken, taken)),
            _ => None,
        }
    }

    fn site(&self, sb: usize, t: Value, x: Value, mask: Value) -> Result<Vec<Value>, (usize, Why)> {
        let p = self.p;
        let st = self.st;
        let n = p.blocks.len();
        let here = p.blocks[sb].offset;
        let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
        let &mb = st.masks.get(&mask).ok_or((here, MASK))?;
        if st.bools.meet(&[(st.reach[sb], true), (mb, false)]) {
            return Err((here, MASK));
        }
        let mut rem: HashSet<Value> = HashSet::from([t]);
        let mut pattern: HashSet<(usize, usize)> = HashSet::new();
        let mut alike: Vec<Value> = Vec::new();
        let copy = |u: (usize, usize)| -> Option<Value> {
            let i = self.inst(u);
            (matches!(i.name(), "v_mov_b32" | "s_mov_b32")
                && args(i).len() == 1
                && st.guards[u.0][u.1].is_none())
            .then(|| result(i))
            .flatten()
        };
        let mut init = vec![t];
        let mut zero: Option<(Value, bool)> = None;
        for u in self.readers(t) {
            let i = self.inst(u);
            let name = i.name();
            if matches!(
                name,
                "s_cmp_eq_u32" | "s_cmp_lg_u32" | "s_cmp_eq_i32" | "s_cmp_lg_i32"
            ) && other(i, t) == Some(Arg::K(0))
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
        let mut idx: Option<Value> = None;
        let mut next: Option<(Value, Value)> = None;
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
                ("s_xor_b32", _) | ("s_andn2_b32", [Arg::Val(_), Arg::Val(_)])
                    if next.is_none() =>
                {
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
                || made(sh).is_some_and(|(_, i)| {
                    i.name() == "s_and_b32" && other(i, idx) == Some(Arg::K(31))
                })
        });
        if !by_idx {
            return Err((head, SHAPE));
        }
        rem.insert(nx);
        let mut nexts = vec![nx];
        for u in self.readers(nx) {
            let i = self.inst(u);
            let name = i.name();
            if matches!(
                name,
                "s_cmp_eq_u32" | "s_cmp_lg_u32" | "s_cmp_eq_i32" | "s_cmp_lg_i32"
            ) && other(i, nx) == Some(Arg::K(0))
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
            let ok = if lp[*q] {
                nexts.contains(a)
            } else {
                init.contains(a)
            };
            if !ok {
                return Err((head, SHAPE));
            }
        }
        let mut leaves: HashSet<usize> = HashSet::new();
        for &(scc, set_while_more) in &more {
            if !self.joins(scc).is_empty() {
                return Err((head, USED));
            }
            if self.readers(scc).is_empty() {
                continue;
            }
            let (u, set, clear) = self.branch(scc).ok_or((head, USED))?;
            let (on, out) = if set_while_more {
                (set, clear)
            } else {
                (clear, set)
            };
            if !lp[u.0] || !lp[on] || lp[out] {
                return Err((self.at(u), SHAPE));
            }
            leaves.insert(u.0);
            pattern.insert(u);
        }
        if leaves.is_empty() {
            return Err((head, SHAPE));
        }
        let mut pre = vec![false; n];
        if let Some((scc, set_when_empty)) = zero {
            if !self.joins(scc).is_empty() {
                return Err((here, USED));
            }
            if !self.readers(scc).is_empty() {
                let (u, set, clear) = self.branch(scc).ok_or((here, USED))?;
                let (none, some) = if set_when_empty {
                    (set, clear)
                } else {
                    (clear, set)
                };
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
        let mut tests: Vec<(Atom, usize, usize)> = Vec::new();
        for u in self.readers(bit) {
            let i = self.inst(u);
            if i.name() != "v_and_b32"
                || other(i, bit) != Some(Arg::Val(x))
                || st.guards[u.0][u.1].is_some()
            {
                continue;
            }
            let Some(y) = result(i) else { continue };
            for c in self.readers(y) {
                let ci = self.inst(c);
                if matches!(
                    ci.name(),
                    "v_cmp_ne_u32"
                        | "v_cmpx_ne_u32"
                        | "v_cmp_lg_u32"
                        | "v_cmpx_lg_u32"
                        | "v_cmp_ne_i32"
                        | "v_cmpx_ne_i32"
                        | "v_cmp_lg_i32"
                        | "v_cmpx_lg_i32"
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
                            (b == tb && i > ti || b != tb && self.dominates(tb, b))
                                && self.implied(g, a, &mut memo[j])
                        })
                    })
        };
        for b in (0..n).filter(|&b| lp[b] || pre[b]) {
            for (i, inst) in p.blocks[b].insts.iter().enumerate() {
                if lp[b] && body_inst(b, i, &mut memo) {
                    continue;
                }
                let u = (b, i);
                for &(reg, w) in &inst.writes {
                    if rem.contains(&w)
                        || is_mask(w)
                        || matches!(reg, Reg::S(c) if c == EXEC || c == EXEC + 1)
                    {
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
                    || super::lower::reads_mask_as_data(st, inst)
                    || self.sections[b]
                        .iter()
                        .any(|(&s, sec)| i >= s && i <= sec.end);
                if cross {
                    return Err((self.at(u), CROSS));
                }
                if used(p, st, b, i).iter().any(|v| rem.contains(v)) {
                    return Err((self.at(u), USED));
                }
            }
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
        let mut dep: HashSet<Atom> = tests.iter().map(|t| t.0).collect();
        let mut changed = true;
        while changed {
            changed = false;
            for (&id, edges) in &st.phi_masks {
                if !dep.contains(&Atom::Phi(id))
                    && edges
                        .iter()
                        .any(|&e| st.bools.support(e).iter().any(|a| dep.contains(a)))
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
                    User::Phi(b2, k2) => {
                        !lp[b2] && self.live.contains(&p.blocks[b2].phis[k2].value)
                    }
                };
                if outside {
                    return Err((head, STALE));
                }
            }
        }
        Ok(alike)
    }
}

pub(super) fn find(p: &Program, st: &Structured, why: bool) -> OwnBits {
    let mut out = OwnBits::default();
    let mut ctx: Option<Ctx> = None;
    for b in 0..p.blocks.len() {
        let sections = wave::sections(p, b);
        let mut starts: Vec<_> = sections.keys().copied().collect();
        starts.sort_unstable();
        for s in starts {
            let sec = &sections[&s];
            let mut left: Vec<_> = sec.shapes.iter().collect();
            left.sort_unstable_by_key(|(v, _)| v.0);
            for (&t, &shape) in left {
                let Shape::Active(Op::Or, x, mask, 0) = shape else {
                    continue;
                };
                let ctx = ctx.get_or_insert_with(|| Ctx::new(p, st));
                let inside =
                    |u: &User| matches!(*u, User::Inst(b2, i) if b2 == b && i > s && i < sec.end);
                if ctx.users.get(&t).is_none_or(|us| us.iter().all(inside)) {
                    continue;
                }
                match ctx.site(b, t, x, mask) {
                    Ok(alike) => {
                        out.totals.insert(t);
                        out.alike.extend(alike);
                    }
                    Err((offset, what)) => {
                        if why {
                            eprintln!(
                                "WAVE64-OWN-BITS the wave OR at {:#x}: not a loop over the lanes' own bits at {offset:#x}: {what}",
                                p.blocks[b].insts[s].machine.offset
                            );
                        }
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::wave::WAVE_OR_V7_TO_S7;

    const ADD_BIT_NUMBER: u32 = 0x4A0A_0A0A;

    fn light_loop(own: bool, body: &[u32], latch: &[u32], after: &[u32]) -> Program {
        let mut w = WAVE_OR_V7_TO_S7.to_vec();
        let test: &[u32] = if own {
            &[0x3608_0E0B, 0xBE8C_047E, 0x7DAA_0880]
        } else {
            &[0xBE8C_047E]
        };
        let head = 3 + test.len() + usize::from(own) + body.len() + 1 + latch.len() + 3;
        w.extend_from_slice(&[
            0xBF06_8007,
            0xBF85_0000 | (3 + head) as u32,
            0x7E06_0207,
            0xBF82_0001,
            0x7E06_0209,
            0x7E10_0503,
            0xBE8A_1308,
            0x8F0B_0A81,
        ]);
        w.extend_from_slice(test);
        if own {
            w.push(0xBF88_0000 | body.len() as u32);
        }
        w.extend_from_slice(body);
        w.push(0xBEFE_040C);
        w.extend_from_slice(latch);
        w.extend_from_slice(&[
            0x8909_080B,
            0xBF06_0B08,
            0xBF84_0000 | (0x1_0000 - head - 1) as u32,
        ]);
        w.extend_from_slice(after);
        w.push(0xBF81_0000);
        let code: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        crate::ir::build(&code, true, 256).unwrap()
    }

    fn found(p: &Program) -> OwnBits {
        let st = crate::structure::structure(p).unwrap();
        find(p, &st, false)
    }

    #[test]
    fn a_wave_or_that_only_steers_a_loop_over_own_bits_is_found() {
        let p = light_loop(true, &[ADD_BIT_NUMBER], &[], &[]);
        let insts = || p.blocks.iter().flat_map(|b| &b.insts);
        let names: Vec<&str> = insts().map(|i| i.name()).collect();
        for n in [
            "s_ff1_i32_b32",
            "s_lshl_b32",
            "v_and_b32",
            "v_cmpx_ne_u32",
            "s_xor_b32",
            "v_readfirstlane_b32",
            "s_cbranch_execz",
        ] {
            assert!(names.contains(&n), "{n} in {names:?}");
        }
        assert!(
            p.blocks.iter().enumerate().any(|(b, blk)| blk
                .exit
                .successors()
                .iter()
                .any(|&s| s < b))
        );
        let own = found(&p);
        let total = insts()
            .rfind(|i| i.name() == "v_readlane_b32")
            .and_then(|i| i.write(Reg::S(7)))
            .unwrap();
        assert_eq!(own.totals, HashSet::from([total]));
        let first = insts().find(|i| i.name() == "v_readfirstlane_b32").unwrap();
        assert!(own.alike.contains(&first.read(Reg::V(3)).unwrap()));
    }

    #[test]
    fn a_loop_whose_lanes_all_work_at_every_bit_needs_the_whole_waves_or() {
        let p = light_loop(false, &[ADD_BIT_NUMBER], &[], &[]);
        assert!(found(&p).totals.is_empty());
    }

    #[test]
    fn a_bit_number_read_after_the_loop_is_not_the_lanes_own() {
        let p = light_loop(true, &[ADD_BIT_NUMBER], &[], &[0x7E0C_020A]);
        assert!(found(&p).totals.is_empty());
    }

    #[test]
    fn the_bits_still_to_visit_read_in_the_body_are_not_the_lanes_own() {
        let p = light_loop(true, &[0x4A0A_0A08], &[], &[]);
        assert!(found(&p).totals.is_empty());
    }

    #[test]
    fn a_count_of_the_rounds_is_not_the_lanes_own() {
        let p = light_loop(true, &[ADD_BIT_NUMBER], &[0x810E_810E], &[0x7E0C_020E]);
        assert!(
            p.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| i.name() == "s_add_i32")
        );
        assert!(found(&p).totals.is_empty());
        let p = light_loop(true, &[ADD_BIT_NUMBER], &[], &[0x7E0C_020E]);
        assert_eq!(found(&p).totals.len(), 1);
    }

    #[test]
    fn an_effect_where_every_lane_passes_is_not_the_lanes_own() {
        let p = light_loop(true, &[ADD_BIT_NUMBER], &[0xD834_0000, 0x0000_0505], &[]);
        assert!(
            p.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| i.name() == "ds_write_b32")
        );
        assert!(found(&p).totals.is_empty());
    }
}
