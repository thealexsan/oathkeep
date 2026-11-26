use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::ir::Value;

/// A per-lane boolean: a node of [`Bools`]. Equal functions are equal nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bool(pub(super) u32);

impl Bool {
    pub const FALSE: Bool = Bool(0);
    pub const TRUE: Bool = Bool(1);
}

/// What a variable of [`Bools`] stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Atom {
    /// The lane's bit of the compare or carry result in this VALU-written
    /// value, before EXEC masking.
    Lane(Value),
    /// An SCC value, the same in every lane.
    Uniform(Value),
    /// A lane mask joined at a block entry, by its low dword's phi.
    Phi(Value),
    /// `s_wqm` result: some lane of the quad is in the source mask.
    Quad(Value),
    /// A mask the pass cannot see into (loaded, built from scalars, an
    /// EXEC write it does not follow): the lane's bit of the register pair
    /// this value is a dword of.
    Opaque(Value),
    /// A constant mask other than all lanes or none.
    Constant(u64),
}

/// Reduced ordered binary decision diagrams over [`Atom`]s.
#[derive(Debug, Clone)]
pub struct Bools {
    /// (variable, low, high); nodes 0 and 1 are the constants.
    pub(super) nodes: Vec<(u32, Bool, Bool)>,
    unique: Map<(u32, Bool, Bool), Bool>,
    memo: Map<(Bool, Bool, Bool), Bool>,
    atoms: Vec<Atom>,
    /// Reverse-postorder index of the block that defines each atom.
    born: Vec<usize>,
    ids: Map<Atom, u32>,
    pub(super) overflow: bool,
}

/// The tables below have small integer keys; SipHash dominated run time.
pub(in crate::structure) type Map<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

/// FxHash's multiply-rotate step.
#[derive(Default)]
pub(in crate::structure) struct Fx(u64);

impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

const TERMINAL: u32 = u32::MAX;
const MAX_NODES: usize = 1 << 21;
/// Case splits [`Bools::meet`] makes before it gives up.
pub(super) const MAX_MEET: usize = 1 << 16;
/// Conditions [`Meets`] keeps; past that, they are searched anew.
const MAX_MEETS: usize = 1 << 18;

/// What [`Bools::meet`] found out about one [`Bools`], for its later calls:
/// the conditions no lane satisfies, and the case splits it took to find
/// that out of each.
#[derive(Debug, Default)]
pub struct Meets {
    pub(super) none: Map<Box<[u32]>, usize>,
}

impl Bools {
    pub(super) fn new() -> Self {
        Bools {
            nodes: vec![
                (TERMINAL, Bool::FALSE, Bool::FALSE),
                (TERMINAL, Bool::TRUE, Bool::TRUE),
            ],
            unique: Map::default(),
            memo: Map::default(),
            atoms: Vec::new(),
            born: Vec::new(),
            ids: Map::default(),
            overflow: false,
        }
    }

    /// The atom tested at the top of `b` and the results when it is false
    /// and true; `None` for the constants.
    pub fn node(&self, b: Bool) -> Option<(Atom, Bool, Bool)> {
        let (v, lo, hi) = self.nodes[b.0 as usize];
        (v != TERMINAL).then(|| (self.atoms[v as usize], lo, hi))
    }

    /// The atoms `b` depends on, in variable order.
    pub fn support(&self, b: Bool) -> Vec<Atom> {
        let mut vars = self.vars(b);
        vars.sort_unstable();
        vars.into_iter().map(|v| self.atoms[v as usize]).collect()
    }

    fn vars(&self, b: Bool) -> Vec<u32> {
        let (mut seen, mut vars, mut stack) = (Vec::new(), Vec::new(), vec![b]);
        while let Some(b) = stack.pop() {
            let (v, lo, hi) = self.nodes[b.0 as usize];
            if v == TERMINAL || seen.contains(&b) {
                continue;
            }
            seen.push(b);
            if !vars.contains(&v) {
                vars.push(v);
            }
            stack.extend([lo, hi]);
        }
        vars
    }

    pub(super) fn atom(&mut self, atom: Atom, born: usize) -> Bool {
        let v = *self.ids.entry(atom).or_insert_with(|| {
            self.atoms.push(atom);
            self.born.push(born);
            (self.atoms.len() - 1) as u32
        });
        self.mk(v, Bool::FALSE, Bool::TRUE)
    }

    fn mk(&mut self, v: u32, lo: Bool, hi: Bool) -> Bool {
        if lo == hi {
            return lo;
        }
        if let Some(&b) = self.unique.get(&(v, lo, hi)) {
            return b;
        }
        if self.nodes.len() >= MAX_NODES {
            // The caller fails once it sees `overflow`.
            self.overflow = true;
            return Bool::FALSE;
        }
        let b = Bool(self.nodes.len() as u32);
        self.nodes.push((v, lo, hi));
        self.unique.insert((v, lo, hi), b);
        b
    }

    /// Cofactors of `b` for `v`, which is `b`'s top variable or above it.
    pub(super) fn split(&self, b: Bool, v: u32) -> (Bool, Bool) {
        match self.nodes[b.0 as usize] {
            (bv, lo, hi) if bv == v => (lo, hi),
            _ => (b, b),
        }
    }

    pub(super) fn ite(&mut self, f: Bool, g: Bool, h: Bool) -> Bool {
        if f == Bool::TRUE || g == h {
            return g;
        }
        if f == Bool::FALSE {
            return h;
        }
        if (g, h) == (Bool::TRUE, Bool::FALSE) {
            return f;
        }
        if let Some(&r) = self.memo.get(&(f, g, h)) {
            return r;
        }
        let top = |b: Bool| self.nodes[b.0 as usize].0;
        let v = top(f).min(top(g)).min(top(h));
        let ((f0, f1), (g0, g1), (h0, h1)) = (self.split(f, v), self.split(g, v), self.split(h, v));
        let lo = self.ite(f0, g0, h0);
        let hi = self.ite(f1, g1, h1);
        let r = self.mk(v, lo, hi);
        self.memo.insert((f, g, h), r);
        r
    }

    pub(super) fn and(&mut self, a: Bool, b: Bool) -> Bool {
        self.ite(a, b, Bool::FALSE)
    }

    pub(super) fn or(&mut self, a: Bool, b: Bool) -> Bool {
        self.ite(a, Bool::TRUE, b)
    }

    pub(super) fn not(&mut self, a: Bool) -> Bool {
        self.ite(a, Bool::FALSE, Bool::TRUE)
    }

    pub(super) fn xor(&mut self, a: Bool, b: Bool) -> Bool {
        let nb = self.not(b);
        self.ite(a, nb, b)
    }

    /// Whether some lane can satisfy every `(b, holds)` of `all` at once:
    /// `b` where `holds`, its negation where not. Yes when the search grows
    /// too large to finish.
    pub fn meet(&self, all: &[(Bool, bool)]) -> bool {
        self.meet_seen(all, &mut Meets::default())
    }

    /// [`Self::meet`], which looks up in `seen` what earlier calls on these
    /// `Bools` found out and adds what it finds. The answer is the one
    /// `meet` gives.
    pub fn meet_seen(&self, all: &[(Bool, bool)], seen: &mut Meets) -> bool {
        self.meet_within(all, MAX_MEET, seen)
    }

    /// [`Self::meet_seen`] with `budget` case splits.
    pub(super) fn meet_within(&self, all: &[(Bool, bool)], mut budget: usize, seen: &mut Meets) -> bool {
        let mut work = all
            .iter()
            .map(|&(b, holds)| b.0 << 1 | u32::from(holds))
            .collect();
        self.meet_in(0, &mut work, &mut budget, seen)
    }

    /// The search of [`Self::meet`] for the conditions `work[at..]` (a node
    /// doubled, plus one where it must hold), which it takes off `work`.
    ///
    /// The search is a case split on the top variable, and the cofactors of
    /// a few large functions come down to the same conditions over and over
    /// (a block's reach under a mask and under its negation share most of
    /// their nodes), so splitting each anew is exponential where the
    /// distinct conditions are few. `seen` keeps the conditions that no lane
    /// satisfies with the case splits it took to find that out: meeting
    /// them again takes those splits off the budget at once. A search
    /// therefore gives up exactly where one that split everything anew
    /// would (nothing below such conditions ends it early, so it either
    /// makes all those splits or runs out among them), and gives the same
    /// answer.
    pub(super) fn meet_in(
        &self,
        at: usize,
        work: &mut Vec<u32>,
        budget: &mut usize,
        seen: &mut Meets,
    ) -> bool {
        // Constants either fail or say nothing.
        let mut kept = at;
        for i in at..work.len() {
            let (b, holds) = (work[i] >> 1, work[i] & 1 != 0);
            if b > Bool::TRUE.0 {
                work[kept] = work[i];
                kept += 1;
            } else if (b == Bool::TRUE.0) != holds {
                work.truncate(at);
                return false;
            }
        }
        work.truncate(kept);
        if kept == at || *budget == 0 {
            work.truncate(at);
            return true;
        }
        // As a set: neither order nor repeats change the search.
        work[at..].sort_unstable();
        let mut end = at + 1;
        for i in at + 1..kept {
            if work[i] != work[end - 1] {
                work[end] = work[i];
                end += 1;
            }
        }
        work.truncate(end);
        if let Some(&splits) = seen.none.get(&work[at..]) {
            let within = splits <= *budget;
            *budget = budget.saturating_sub(splits);
            work.truncate(at);
            return !within;
        }
        let before = *budget;
        *budget -= 1;
        let top = |code: u32| self.nodes[(code >> 1) as usize].0;
        let v = work[at..].iter().map(|&c| top(c)).min().expect("not empty");
        let mut found = false;
        for value in [false, true] {
            for i in at..end {
                let (lo, hi) = self.split(Bool(work[i] >> 1), v);
                work.push((if value { hi } else { lo }).0 << 1 | work[i] & 1);
            }
            if self.meet_in(end, work, budget, seen) {
                found = true;
                break;
            }
        }
        if !found && seen.none.len() < MAX_MEETS {
            seen.none.insert(work[at..end].into(), before - *budget);
        }
        work.truncate(at);
        found
    }

    pub(super) fn implies(&mut self, a: Bool, b: Bool) -> bool {
        let nb = self.not(b);
        self.and(a, nb) == Bool::FALSE
    }

    /// `b` with every atom born at reverse-postorder index `from` or later
    /// existentially quantified: what still holds once those are redefined.
    pub(super) fn forget(&mut self, b: Bool, from: usize, memo: &mut Map<Bool, Bool>) -> Bool {
        let (v, lo, hi) = self.nodes[b.0 as usize];
        if v == TERMINAL {
            return b;
        }
        if let Some(&r) = memo.get(&b) {
            return r;
        }
        let lo = self.forget(lo, from, memo);
        let hi = self.forget(hi, from, memo);
        let r = if self.born[v as usize] >= from {
            self.or(lo, hi)
        } else {
            self.mk(v, lo, hi)
        };
        memo.insert(b, r);
        r
    }

    pub(super) fn restrict(&mut self, b: Bool, v: u32, value: bool, memo: &mut Map<Bool, Bool>) -> Bool {
        let (bv, lo, hi) = self.nodes[b.0 as usize];
        if bv == TERMINAL || bv > v {
            return b;
        }
        if bv == v {
            return if value { hi } else { lo };
        }
        if let Some(&r) = memo.get(&b) {
            return r;
        }
        let l = self.restrict(lo, v, value, memo);
        let h = self.restrict(hi, v, value, memo);
        let r = self.mk(bv, l, h);
        memo.insert(b, r);
        r
    }

    /// `b` with each atom that `known` decides replaced by its value.
    pub(super) fn given(&mut self, mut b: Bool, known: Bool) -> Bool {
        for v in self.vars(b) {
            let x = self.mk(v, Bool::FALSE, Bool::TRUE);
            let nx = self.not(x);
            if self.implies(known, x) {
                b = self.restrict(b, v, true, &mut Map::default());
            } else if self.implies(known, nx) {
                b = self.restrict(b, v, false, &mut Map::default());
            }
        }
        b
    }
}

