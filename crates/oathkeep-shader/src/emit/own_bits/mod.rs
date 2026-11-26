//! Wave ORs that only steer a loop over each lane's own bits.
//!
//! The title's light loops are "scalarised": the lanes' light masks are ORed over the wave and
//! the wave walks the set bits of the result together, each lane working only where its own
//! mask has the bit:
//!
//! ```text
//! merged = WaveActiveBitOr(mask);            // over the lanes that run
//! while (merged != 0) {
//!     bit = firstbitlow(merged);
//!     if (mask & (1 << bit)) { ... }         // the body
//!     merged ^= 1 << bit;
//! }
//! ```
//!
//! A console wave64 program in two host waves of 32 cannot exchange the halves' ORs where not
//! every lane runs (a group barrier must be reached by every thread). It does not have to: as
//! one thread runs it, the loop visits the bits of `merged` in rising order and does something
//! only at the bits of the thread's own mask, so any `merged` that holds the thread's own bits
//! gives that thread the same run. The host wave's own OR is such a value.
//!
//! That holds only if the OR is used for nothing else, and if a round of the loop at a bit the
//! thread does not have leaves nothing behind. [`find`] proves both for each wave OR, with no
//! instruction sequence taken on trust:
//!
//! - the OR and what derives from it as "the bits still to visit" (copies, the loop's join, the
//!   first-lane read of it, the set less the visited bit) are read only by the loop's own
//!   steps: the test for an empty set, the lowest set bit, the removal of that bit, the test
//!   that ends the loop;
//! - the lane's test is its own bit of the very value that was ORed, and "the body" is what a
//!   lane only reaches with that test true in the same round;
//! - outside the body the loop has no side effect and no cross-lane step, writes nothing that
//!   is read after the loop or carried into a later round, and is left only by the test for
//!   "no bits left";
//! - no lane mask that depends on the test is used outside the round that made it.
//!
//! Anything else is refused there as before ("a wave reduction where not every lane runs") and
//! left to the forms of the program in which every lane runs that code (`emit`'s retries: the
//! skips left out, the branches around it taken wave-wide), where the halves do exchange.
//! `OATHKEEP_WAVE_OWN_BITS=0` turns this off.

use std::collections::{HashMap, HashSet};

use super::wave::{self, Op, Shape};
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Exit, Inst, Program, Reg, Value};
use crate::structure::{Atom, Bool, Structured};

pub(super) type Why = &'static str;

pub(super) const USED: Why = "a wave OR used as data";
pub(super) const NO_LOOP: Why = "a wave OR that steers no loop over its bits";
pub(super) const SHAPE: Why = "a loop over a wave OR's bits of another shape";
pub(super) const TEST: Why = "a loop over a wave OR's bits whose lanes do not test their own bit";
pub(super) const LEAVES: Why = "a value of a round over a wave OR's bits that outlives the round";
pub(super) const EFFECT: Why = "an effect in a loop over a wave OR's bits outside the lanes' own bits";
pub(super) const CROSS: Why = "a cross-lane step in a loop over a wave OR's bits outside the lanes' own bits";
pub(super) const EXIT: Why = "a loop over a wave OR's bits left another way";
pub(super) const STALE: Why = "a lane's own-bit test used outside its round";
pub(super) const MASK: Why = "a wave OR over a mask a running lane may be outside";

/// What [`find`] proved.
#[derive(Debug, Default)]
pub(super) struct OwnBits {
    /// Wave ORs (the values their sections leave) a host wave may take over its own lanes.
    pub totals: HashSet<Value>,
    /// VGPR values of those loops that every lane of a host wave holds alike (the bits still
    /// to visit): a first-lane read of one is the lane's own value.
    pub alike: HashSet<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum User {
    Inst(usize, usize),
    Phi(usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Arg {
    Val(Value),
    K(u32),
    Other,
}

pub(super) struct Ctx<'a> {
    pub(super) p: &'a Program,
    pub(super) st: &'a Structured,
    users: HashMap<Value, Vec<User>>,
    /// The instruction writing each value.
    def: HashMap<Value, (usize, usize)>,
    /// Values an instruction reads, directly or through joins.
    live: HashSet<Value>,
    idom: Vec<usize>,
    sections: Vec<HashMap<usize, wave::Section>>,
}

/// The values instruction `i` of block `b` uses: what its operands read, without EXEC (lane
/// masks are followed as booleans) and without the old value of a VGPR it writes, which only
/// lanes outside a guard keep.
pub(super) fn used(p: &Program, st: &Structured, b: usize, i: usize) -> Vec<Value> {
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

pub(super) fn value(inst: &Inst, op: &Operand) -> Option<Value> {
    match *op {
        Operand::Scalar { code, .. } if code != NULL => inst.read(Reg::S(code)),
        Operand::Vector { reg, .. } => inst.read(Reg::V(reg)),
        _ => None,
    }
}

/// The sources of a scalar or unmodified vector operation.
pub(super) fn args(inst: &Inst) -> Vec<Arg> {
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

/// The other source of a two-source operation that reads `a`.
pub(super) fn other(inst: &Inst, a: Value) -> Option<Arg> {
    match args(inst)[..] {
        [Arg::Val(x), o] if x == a => Some(o),
        [o, Arg::Val(x)] if x == a => Some(o),
        _ => None,
    }
}

/// The scalar (not SCC) or vector dword an instruction writes, if it writes one.
pub(super) fn result(inst: &Inst) -> Option<Value> {
    let mut w = inst.writes.iter().filter(|w| w.0 != Reg::Scc);
    match (w.next(), w.next()) {
        (Some(&(_, v)), None) => Some(v),
        _ => None,
    }
}

/// The same test as `wave_local`'s and `Lower::inst`'s `effect`, and the messages a program
/// sends.
pub(super) fn effect(inst: &Inst) -> bool {
    let name = inst.name();
    matches!(inst.fields, Fields::Exp { .. })
        || matches!(inst.fields, Fields::Flat { .. }) && !name.starts_with("global_load_")
        || name.contains("store")
        || name.contains("atomic")
        || matches!(inst.fields, Fields::Ds { .. })
            && !(name.starts_with("ds_read") || name.starts_with("ds_swizzle") || name.contains("permute"))
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
        // through joins
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

    pub(super) fn inst(&self, u: (usize, usize)) -> &Inst {
        &self.p.blocks[u.0].insts[u.1]
    }

    pub(super) fn at(&self, u: (usize, usize)) -> usize {
        self.inst(u).machine.offset
    }

    /// Whether block `a` dominates block `b`.
    pub(super) fn dominates(&self, a: usize, mut b: usize) -> bool {
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

    pub(super) fn readers(&self, v: Value) -> Vec<(usize, usize)> {
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

    /// The joins reading `v` whose value something reads.
    pub(super) fn joins(&self, v: Value) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = self
            .users
            .get(&v)
            .into_iter()
            .flatten()
            .filter_map(|u| match *u {
                User::Phi(b, k) if self.live.contains(&self.p.blocks[b].phis[k].value) => Some((b, k)),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The loop of header `h`: the blocks that reach one of its back edges without passing it.
    pub(super) fn natural_loop(&self, h: usize) -> Vec<bool> {
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

    /// Whether `b` holds only in lanes where `a` does.
    pub(super) fn implied(&self, b: Bool, a: Atom, memo: &mut HashMap<Bool, bool>) -> bool {
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

    /// The conditional branch reading `scc`, as (its place, the block it goes to when SCC is
    /// set, the other one).
    pub(super) fn branch(&self, scc: Value) -> Option<((usize, usize), usize, usize)> {
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
}

/// The wave ORs of `p` that only steer a loop over each lane's own bits (see the module
/// notes). `why`: one line on stderr for each wave OR that is not one.
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
                // (values only the section itself reads are not results)
                let inside = |u: &User| matches!(*u, User::Inst(b2, i) if b2 == b && i > s && i < sec.end);
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


mod site;
#[cfg(test)]
mod tests;
