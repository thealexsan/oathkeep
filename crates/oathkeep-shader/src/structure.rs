use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use crate::ShaderError;
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Exit, Inst, Program, Reg, VCC, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bool(u32);

impl Bool {
    pub const FALSE: Bool = Bool(0);
    pub const TRUE: Bool = Bool(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Atom {
    Lane(Value),
    Uniform(Value),
    Phi(Value),
    Quad(Value),
    Opaque(Value),
    Constant(u64),
}

#[derive(Debug, Clone)]
pub struct Bools {
    nodes: Vec<(u32, Bool, Bool)>,
    unique: Map<(u32, Bool, Bool), Bool>,
    memo: Map<(Bool, Bool, Bool), Bool>,
    atoms: Vec<Atom>,
    born: Vec<usize>,
    ids: Map<Atom, u32>,
    overflow: bool,
}

type Map<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

#[derive(Default)]
struct Fx(u64);

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
const MAX_MEET: usize = 1 << 16;
const MAX_MEETS: usize = 1 << 18;

#[derive(Debug, Default)]
pub struct Meets {
    none: Map<Box<[u32]>, usize>,
}

impl Bools {
    fn new() -> Self {
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

    pub fn node(&self, b: Bool) -> Option<(Atom, Bool, Bool)> {
        let (v, lo, hi) = self.nodes[b.0 as usize];
        (v != TERMINAL).then(|| (self.atoms[v as usize], lo, hi))
    }

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

    fn atom(&mut self, atom: Atom, born: usize) -> Bool {
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
            self.overflow = true;
            return Bool::FALSE;
        }
        let b = Bool(self.nodes.len() as u32);
        self.nodes.push((v, lo, hi));
        self.unique.insert((v, lo, hi), b);
        b
    }

    fn split(&self, b: Bool, v: u32) -> (Bool, Bool) {
        match self.nodes[b.0 as usize] {
            (bv, lo, hi) if bv == v => (lo, hi),
            _ => (b, b),
        }
    }

    fn ite(&mut self, f: Bool, g: Bool, h: Bool) -> Bool {
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

    fn and(&mut self, a: Bool, b: Bool) -> Bool {
        self.ite(a, b, Bool::FALSE)
    }

    fn or(&mut self, a: Bool, b: Bool) -> Bool {
        self.ite(a, Bool::TRUE, b)
    }

    fn not(&mut self, a: Bool) -> Bool {
        self.ite(a, Bool::FALSE, Bool::TRUE)
    }

    fn xor(&mut self, a: Bool, b: Bool) -> Bool {
        let nb = self.not(b);
        self.ite(a, nb, b)
    }

    pub fn meet(&self, all: &[(Bool, bool)]) -> bool {
        self.meet_seen(all, &mut Meets::default())
    }

    pub fn meet_seen(&self, all: &[(Bool, bool)], seen: &mut Meets) -> bool {
        self.meet_within(all, MAX_MEET, seen)
    }

    fn meet_within(&self, all: &[(Bool, bool)], mut budget: usize, seen: &mut Meets) -> bool {
        let mut work = all
            .iter()
            .map(|&(b, holds)| b.0 << 1 | u32::from(holds))
            .collect();
        self.meet_in(0, &mut work, &mut budget, seen)
    }

    fn meet_in(
        &self,
        at: usize,
        work: &mut Vec<u32>,
        budget: &mut usize,
        seen: &mut Meets,
    ) -> bool {
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

    fn implies(&mut self, a: Bool, b: Bool) -> bool {
        let nb = self.not(b);
        self.and(a, nb) == Bool::FALSE
    }

    fn forget(&mut self, b: Bool, from: usize, memo: &mut Map<Bool, Bool>) -> Bool {
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

    fn restrict(&mut self, b: Bool, v: u32, value: bool, memo: &mut Map<Bool, Bool>) -> Bool {
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

    fn given(&mut self, mut b: Bool, known: Bool) -> Bool {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneExit {
    Jump(usize),
    Branch {
        when: Bool,
        taken: usize,
        not_taken: usize,
    },
    End,
    Indirect,
}

#[derive(Debug, Clone)]
pub struct Structured {
    pub bools: Bools,
    pub exits: Vec<LaneExit>,
    pub guards: Vec<Vec<Option<Bool>>>,
    pub reach: Vec<Bool>,
    pub masks: HashMap<Value, Bool>,
    paired_masks: Map<Key, Bool>,
    pub highs: HashMap<Value, Value>,
    pub active: HashSet<Value>,
    pub phi_masks: HashMap<Value, Vec<Bool>>,
    pub wide: HashSet<usize>,
    pub wave_any: Vec<Option<bool>>,
    pub scc_some: HashMap<Value, Bool>,
}

impl Structured {
    pub(crate) fn mask_bits(&self, low: Value, high: Option<Value>) -> Option<Bool> {
        match high {
            Some(high) => self.paired_masks.get(&(low, high)),
            None => self.masks.get(&low),
        }
        .copied()
    }
}

pub fn structure(program: &Program) -> Result<Structured, ShaderError> {
    structure_wide(program, &HashSet::new())
}

pub fn structure_wide(program: &Program, wide: &HashSet<usize>) -> Result<Structured, ShaderError> {
    let mut l = Lanes::new(program)?;
    l.wide = wide.clone();
    loop {
        let execs = l.masks_in_order();
        let cond = l.paths()?;
        let (wrong, new_derived) = l.check_joins(&cond);
        if l.b.overflow {
            return Err(TOO_LARGE);
        }
        if wrong.is_empty() && new_derived.is_empty() {
            return l.finish(&execs, &cond);
        }
        l.unresolved.extend(wrong);
        l.derived_phis.extend(new_derived);
        l.reset();
    }
}

type Key = (Value, Value);

struct Lanes<'a> {
    p: &'a Program,
    order: Vec<usize>,
    index: Vec<usize>,
    incoming: Vec<Vec<(usize, bool)>>,
    defs: Map<Value, (usize, usize)>,
    phis: Map<Value, (usize, usize)>,
    unresolved: HashSet<Value>,
    derived_phis: HashSet<Value>,
    b: Bools,
    masks: Map<Key, Bool>,
    other: Map<Key, Bool>,
    seen: Vec<Key>,
    scc_lane: Map<Value, Bool>,
    joins: Vec<Join>,
    phi_joins: Vec<PhiJoin>,
    phi_join_ids: HashSet<Value>,
    first: Vec<Bool>,
    when: Vec<Option<Bool>>,
    active: HashSet<Value>,
    wide: HashSet<usize>,
    scc_every: HashSet<Value>,
    wave_any: Vec<Option<bool>>,
}

struct Join {
    block: usize,
    id: Value,
    value: bool,
    derived: bool,
    args: Vec<Key>,
}

struct PhiJoin {
    block: usize,
    id: Value,
    derived: bool,
    args: Vec<Key>,
}

struct SopMasks {
    dst: Option<(Key, Bool)>,
    exec: Option<(Key, Bool)>,
    derived: bool,
    scc: Option<Bool>,
}

const MAX_ROUNDS: usize = 256;

impl<'a> Lanes<'a> {
    fn new(p: &'a Program) -> Result<Self, ShaderError> {
        let blocks = &p.blocks;
        let order = crate::ir::reverse_postorder(blocks);
        let mut index = vec![usize::MAX; blocks.len()];
        for (i, &b) in order.iter().enumerate() {
            index[b] = i;
        }
        check_reducible(p, &order, &index)?;
        let mut incoming = vec![Vec::new(); blocks.len()];
        let (mut defs, mut phis) = (Map::default(), Map::default());
        for (b, block) in blocks.iter().enumerate() {
            for (k, s) in block.exit.successors().into_iter().enumerate() {
                incoming[s].push((b, k == 0));
            }
            for (i, phi) in block.phis.iter().enumerate() {
                phis.insert(phi.value, (b, i));
            }
            for (i, inst) in block.insts.iter().enumerate() {
                for &(_, v) in &inst.writes {
                    defs.insert(v, (b, i));
                }
            }
        }
        Ok(Lanes {
            p,
            order,
            index,
            incoming,
            defs,
            phis,
            unresolved: HashSet::new(),
            derived_phis: HashSet::new(),
            b: Bools::new(),
            masks: Map::default(),
            other: Map::default(),
            seen: Vec::new(),
            scc_lane: Map::default(),
            joins: Vec::new(),
            phi_joins: Vec::new(),
            phi_join_ids: HashSet::new(),
            first: vec![Bool::FALSE; blocks.len()],
            when: vec![None; blocks.len()],
            active: HashSet::new(),
            wide: HashSet::new(),
            scc_every: HashSet::new(),
            wave_any: vec![None; blocks.len()],
        })
    }

    fn reset(&mut self) {
        self.b = Bools::new();
        self.masks.clear();
        self.other.clear();
        self.seen.clear();
        self.scc_lane.clear();
        self.scc_every.clear();
        self.wave_any.fill(None);
        self.joins.clear();
        self.phi_joins.clear();
        self.phi_join_ids.clear();
        self.first.fill(Bool::FALSE);
        self.when.fill(None);
        self.active.clear();
    }

    fn masks_in_order(&mut self) -> Vec<Vec<Option<Bool>>> {
        let p = self.p;
        let input = |reg| p.inputs.iter().find(|i| i.0 == reg).map(|i| i.1);
        if let Some(lo) = input(Reg::S(EXEC)) {
            let hi = if p.wave64 {
                input(Reg::S(EXEC + 1)).unwrap_or(lo)
            } else {
                lo
            };
            self.masks.insert((lo, hi), Bool::TRUE);
            self.active.insert(lo);
        }
        let mut execs: Vec<Vec<Option<Bool>>> =
            p.blocks.iter().map(|b| vec![None; b.insts.len()]).collect();
        for n in 0..self.order.len() {
            let b = self.order[n];
            let mut c = if n == 0 { Bool::TRUE } else { Bool::FALSE };
            for k in 0..self.incoming[b].len() {
                let (q, taken) = self.incoming[b][k];
                if self.index[q] < n {
                    let e = self.edge(self.first[q], q, taken);
                    c = self.b.or(c, e);
                }
            }
            self.first[b] = c;
            for phi in &p.blocks[b].phis {
                if phi.reg == Reg::S(EXEC) {
                    self.active.insert(phi.value);
                }
            }
            let mut now = None;
            for (i, inst) in p.blocks[b].insts.iter().enumerate() {
                if inst.read(Reg::S(EXEC)).is_some() {
                    let e = self.exec(inst);
                    execs[b][i] = Some(e);
                    now = Some(e);
                }
                self.step(inst, now);
                if let Some(k) = self.key(|r| inst.write(r), EXEC) {
                    now = self.masks.get(&k).or(self.other.get(&k)).copied();
                }
            }
            if let Exit::Branch { .. } = p.blocks[b].exit
                && let Some(last) = p.blocks[b].insts.last()
            {
                self.when[b] = Some(self.branch(last));
                self.wave_any[b] = self.wave_kind(last);
            }
        }
        execs
    }

    fn edge(&mut self, c: Bool, q: usize, taken: bool) -> Bool {
        if self.wide.contains(&q) {
            return c;
        }
        match self.when[q] {
            Some(w) if taken => self.b.and(c, w),
            Some(w) => {
                let nw = self.b.not(w);
                self.b.and(c, nw)
            }
            None => c,
        }
    }

    fn check_joins(&mut self, cond: &[Bool]) -> (Vec<Value>, Vec<Value>) {
        let mut wrong = Vec::new();
        let mut new_derived = Vec::new();
        let mut seen_derived = HashSet::new();
        let mut j = 0;
        let mut p = 0;
        while j < self.joins.len() || p < self.phi_joins.len() {
            while j < self.joins.len() {
                let (block, id, value, was_derived) = (
                    self.joins[j].block,
                    self.joins[j].id,
                    self.joins[j].value,
                    self.joins[j].derived,
                );
                let mut failed = false;
                for (n, a) in self.joins[j].args.clone().into_iter().enumerate() {
                    let (q, taken) = self.incoming[block][n];
                    let e = self.edge(cond[q], q, taken);
                    let (mut m, d) = self.mask(a);
                    if !was_derived
                        && d
                        && !self.derived_phis.contains(&id)
                        && seen_derived.insert(id)
                    {
                        new_derived.push(id);
                    }
                    if !value {
                        m = self.b.not(m);
                    }
                    if !failed && !self.b.implies(e, m) {
                        wrong.push(id);
                        failed = true;
                    }
                }
                j += 1;
            }
            while p < self.phi_joins.len() {
                let (id, was_derived) = (self.phi_joins[p].id, self.phi_joins[p].derived);
                for a in self.phi_joins[p].args.clone() {
                    let (_, d) = self.mask(a);
                    if !was_derived
                        && d
                        && !self.derived_phis.contains(&id)
                        && seen_derived.insert(id)
                    {
                        new_derived.push(id);
                    }
                }
                p += 1;
            }
        }
        (wrong, new_derived)
    }

    fn finish(
        mut self,
        execs: &[Vec<Option<Bool>>],
        cond: &[Bool],
    ) -> Result<Structured, ShaderError> {
        let phi_joins = std::mem::take(&mut self.phi_joins);
        let mut phi_masks = HashMap::with_capacity(phi_joins.len());
        for pj in phi_joins {
            let incoming: Vec<Bool> = pj
                .args
                .into_iter()
                .enumerate()
                .map(|(n, a)| {
                    let (q, taken) = self.incoming[pj.block][n];
                    let e = self.edge(cond[q], q, taken);
                    let m = self.mask(a).0;
                    self.b.given(m, e)
                })
                .collect();
            phi_masks.insert(pj.id, incoming);
        }
        let blocks = &self.p.blocks;
        let b = &mut self.b;
        let guards = blocks
            .iter()
            .enumerate()
            .map(|(n, block)| {
                block
                    .insts
                    .iter()
                    .zip(&execs[n])
                    .map(|(inst, exec)| match (exec, &inst.fields) {
                        (None, _)
                        | (_, Fields::Sop { .. } | Fields::Sopp { .. } | Fields::Smem { .. }) => {
                            None
                        }
                        (&Some(e), _) if b.implies(cond[n], e) => None,
                        (&Some(e), _) => Some(b.given(e, cond[n])),
                    })
                    .collect()
            })
            .collect();
        let exits = blocks
            .iter()
            .enumerate()
            .map(|(n, block)| match block.exit {
                Exit::Jump(t) => LaneExit::Jump(t),
                Exit::End => LaneExit::End,
                Exit::Indirect => LaneExit::Indirect,
                Exit::Branch { taken, not_taken } => LaneExit::Branch {
                    when: b.given(self.when[n].unwrap_or(Bool::FALSE), cond[n]),
                    taken,
                    not_taken,
                },
            })
            .collect();
        if self.b.overflow {
            return Err(TOO_LARGE);
        }
        let (mut high, mut split) = (Map::default(), HashSet::new());
        let keys = self.masks.keys().chain(self.other.keys());
        for &(lo, hi) in keys.chain(&self.seen) {
            if *high.entry(lo).or_insert(hi) != hi {
                split.insert(lo);
            }
        }
        self.masks.retain(|k, _| !split.contains(&k.0));
        self.active.retain(|lo| !split.contains(lo));
        Ok(Structured {
            masks: self.masks.iter().map(|(&(lo, _), &m)| (lo, m)).collect(),
            highs: self
                .masks
                .keys()
                .filter(|(lo, hi)| lo != hi)
                .map(|&(lo, hi)| (hi, lo))
                .collect(),
            paired_masks: if self.p.wave64 {
                self.masks
            } else {
                Map::default()
            },
            active: self.active,
            wide: self.wide,
            wave_any: self.wave_any,
            scc_some: self
                .scc_lane
                .iter()
                .filter(|(scc, _)| !self.scc_every.contains(scc))
                .map(|(&scc, &m)| (scc, m))
                .collect(),
            phi_masks,
            bools: self.b,
            exits,
            guards,
            reach: cond.to_vec(),
        })
    }

    fn key(&self, value: impl Fn(Reg) -> Option<Value>, code: u8) -> Option<Key> {
        let lo = value(Reg::S(code))?;
        let hi = if self.p.wave64 {
            value(Reg::S(code + 1))?
        } else {
            lo
        };
        Some((lo, hi))
    }

    fn born(&self, v: Value) -> usize {
        match (self.defs.get(&v), self.phis.get(&v)) {
            (Some(&(b, _)), _) | (_, Some(&(b, _))) => self.index[b],
            _ => 0,
        }
    }

    fn exec(&mut self, inst: &Inst) -> Bool {
        match self.key(|r| inst.read(r), EXEC) {
            Some(k) => self.mask(k).0,
            None => Bool::TRUE,
        }
    }

    fn mask(&mut self, k: Key) -> (Bool, bool) {
        if let Some(&m) = self.masks.get(&k) {
            return (m, true);
        }
        if let Some(&m) = self.other.get(&k) {
            return (m, false);
        }
        let p = self.p;
        let (m, derived) = match self.join(k) {
            Some(join) => join,
            None => {
                let (lo, hi) = (self.defs.get(&k.0).copied(), self.defs.get(&k.1).copied());
                let def = |d: Option<(usize, usize)>| d.map(|(b, i)| &p.blocks[b].insts[i]);
                let m = match (lo, hi) {
                    (Some(at), Some(hi)) if at == hi => def(lo)
                        .and_then(|i| self.sop(i))
                        .and_then(|r| r.dst)
                        .map(|d| d.1),
                    _ => None,
                };
                let exec_write = |d: Option<(usize, usize)>, v: Value| {
                    def(d).is_some_and(|i| {
                        i.write(Reg::S(EXEC)) == Some(v) || i.write(Reg::S(EXEC + 1)) == Some(v)
                    })
                };
                let derived = exec_write(lo, k.0) || exec_write(hi, k.1);
                let m = m.unwrap_or_else(|| {
                    let born = self.born(k.0);
                    self.b.atom(Atom::Opaque(k.0), born)
                });
                (m, derived)
            }
        };
        let cache = if derived {
            &mut self.masks
        } else {
            &mut self.other
        };
        cache.insert(k, m);
        (m, derived)
    }

    fn join(&mut self, k: Key) -> Option<(Bool, bool)> {
        let p = self.p;
        let (lo, hi) = (self.phis.get(&k.0).copied(), self.phis.get(&k.1).copied());
        let block = match (lo, hi) {
            (Some((a, _)), Some((b, _))) if a != b => return None,
            (Some((b, _)), _) | (_, Some((b, _))) => b,
            _ => return None,
        };
        let target = &p.blocks[block];
        let arg = |phi: Option<(usize, usize)>, own: Value, n: usize| {
            phi.map_or(own, |(_, i)| target.phis[i].args[n])
        };
        let args: Vec<Key> = (0..target.preds.len())
            .map(|n| (arg(lo, k.0, n), arg(hi, k.1, n)))
            .collect();
        let id = lo.map_or(k.1, |_| k.0);
        let mut derived = self.derived_phis.contains(&id);
        let mut forward = Vec::new();
        for (n, &a) in args.iter().enumerate() {
            let (q, taken) = self.incoming[block][n];
            if self.index[q] < self.index[block] {
                let (m, d) = self.mask(a);
                derived |= d;
                let e = self.edge(self.first[q], q, taken);
                forward.push((e, m));
            } else {
                self.seen.push(a);
            }
        }
        if forward.len() == args.len()
            && let Some(&(_, m)) = forward.first()
            && forward.iter().all(|f| f.1 == m)
        {
            return Some((m, derived));
        }
        if !self.unresolved.contains(&id) {
            for value in [true, false] {
                let b = &mut self.b;
                let holds = forward.iter().all(|&(e, m)| {
                    let m = if value { m } else { b.not(m) };
                    b.implies(e, m)
                });
                if holds {
                    self.joins.push(Join {
                        block,
                        id,
                        value,
                        derived,
                        args,
                    });
                    let m = if value { Bool::TRUE } else { Bool::FALSE };
                    return Some((m, derived));
                }
            }
        }
        if self.phi_join_ids.insert(id) {
            self.phi_joins.push(PhiJoin {
                block,
                id,
                derived,
                args,
            });
        }
        let born = self.index[block];
        Some((self.b.atom(Atom::Phi(id), born), derived))
    }

    fn operand(&mut self, inst: &Inst, op: &Operand) -> Option<(Bool, bool)> {
        let wave64 = self.p.wave64;
        let constant = |b: &mut Bools, v: u64| {
            let v = if wave64 { v } else { v & 0xFFFF_FFFF };
            match v {
                0 => Bool::FALSE,
                _ if v == u64::MAX >> (32 * u8::from(!wave64)) => Bool::TRUE,
                _ => b.atom(Atom::Constant(v), 0),
            }
        };
        Some(match *op {
            Operand::Scalar { code, .. } if code != NULL => {
                let k = self.key(|r| inst.read(r), code)?;
                self.mask(k)
            }
            Operand::Const(c @ 128..=208) => {
                let v = if c <= 192 {
                    i64::from(c) - 128
                } else {
                    192 - i64::from(c)
                };
                (constant(&mut self.b, v as u64), false)
            }
            Operand::Literal(x) => (constant(&mut self.b, u64::from(x)), false),
            _ => return None,
        })
    }

    fn sop(&mut self, inst: &Inst) -> Option<SopMasks> {
        let Fields::Sop { dst, src, .. } = &inst.fields else {
            return None;
        };
        let width = if self.p.wave64 { "_b64" } else { "_b32" };
        let op = inst.name().strip_prefix("s_")?.strip_suffix(width)?;
        let dst = match dst {
            Some(Operand::Scalar { code, .. }) if *code != NULL => {
                self.key(|r| inst.write(r), *code)
            }
            _ => None,
        };
        let exec_out = self.key(|r| inst.write(r), EXEC);
        let (save, f) = match (op.strip_suffix("_saveexec"), op.strip_suffix("_wrexec")) {
            (Some(f), _) => (Some(true), f),
            (_, Some(f)) => (Some(false), f),
            _ => (None, op),
        };
        let mut derived = exec_out.is_some() || save.is_some();
        let mut arg = |n: usize, l: &mut Self| {
            let (m, d) = l.operand(inst, src.get(n)?)?;
            derived |= d;
            Some(m)
        };
        if let Some(saveexec) = save {
            let s = arg(0, self)?;
            let exec = self.exec(inst);
            let new = self.combine(f, s, exec)?;
            let saved = if saveexec { exec } else { new };
            return Some(SopMasks {
                dst: dst.map(|d| (d, saved)),
                exec: exec_out.map(|e| (e, new)),
                derived,
                scc: Some(new),
            });
        }
        let (result, scc) = match op {
            "mov" => (arg(0, self)?, None),
            "not" => {
                let a = arg(0, self)?;
                let r = self.b.not(a);
                (r, Some(r))
            }
            "wqm" => {
                let a = arg(0, self)?;
                let lo = dst.or(exec_out)?.0;
                let born = self.born(lo);
                let q = self.b.atom(Atom::Quad(lo), born);
                let r = self.b.or(a, q);
                (r, Some(r))
            }
            "cselect" => {
                let (a, c) = (arg(0, self)?, arg(1, self)?);
                let scc = inst.read(Reg::Scc)?;
                if self.clear_after_loop(scc) {
                    (c, None)
                } else {
                    let born = self.born(scc);
                    let u = self.b.atom(Atom::Uniform(scc), born);
                    (self.b.ite(u, a, c), None)
                }
            }
            _ => {
                let (a, c) = (arg(0, self)?, arg(1, self)?);
                let r = self.combine(op, a, c)?;
                (r, Some(r))
            }
        };
        Some(SopMasks {
            dst: dst.map(|d| (d, result)),
            exec: exec_out.map(|e| (e, result)),
            derived,
            scc,
        })
    }

    fn clear_after_loop(&self, scc: Value) -> bool {
        let Some(&(b, i)) = self.defs.get(&scc) else {
            return false;
        };
        let block = &self.p.blocks[b];
        let (Some(writer), Some(last)) = (block.insts.get(i), block.insts.last()) else {
            return false;
        };
        let exec = writer.write(Reg::S(EXEC));
        let back = match block.exit {
            Exit::Branch { taken, .. } => self.index[taken] <= self.index[b],
            _ => false,
        };
        back && i + 2 == block.insts.len()
            && exec.is_some()
            && last.name() == "s_cbranch_execnz"
            && last.read(Reg::S(EXEC)) == exec
            && (!self.p.wave64 || last.read(Reg::S(EXEC + 1)) == writer.write(Reg::S(EXEC + 1)))
    }

    fn combine(&mut self, f: &str, a: Bool, c: Bool) -> Option<Bool> {
        let b = &mut self.b;
        Some(match f {
            "and" => b.and(a, c),
            "or" => b.or(a, c),
            "xor" => b.xor(a, c),
            "andn2" => {
                let nc = b.not(c);
                b.and(a, nc)
            }
            "orn2" => {
                let nc = b.not(c);
                b.or(a, nc)
            }
            "andn1" => {
                let na = b.not(a);
                b.and(na, c)
            }
            "orn1" => {
                let na = b.not(a);
                b.or(na, c)
            }
            "nand" => {
                let r = b.and(a, c);
                b.not(r)
            }
            "nor" => {
                let r = b.or(a, c);
                b.not(r)
            }
            "xnor" => {
                let r = b.xor(a, c);
                b.not(r)
            }
            _ => return None,
        })
    }

    fn step(&mut self, inst: &Inst, exec: Option<Bool>) {
        let name = inst.name();
        match &inst.fields {
            Fields::Sop { src, .. } => {
                let mut takes_mask = false;
                for s in src {
                    if let Operand::Scalar { code, .. } = *s
                        && code != NULL
                        && let Some(k) = self.key(|r| inst.read(r), code)
                    {
                        let phi = self.phis.contains_key(&k.0) || self.phis.contains_key(&k.1);
                        takes_mask |= self.masks.contains_key(&k) || phi && self.mask(k).1;
                    }
                }
                let touches_exec =
                    inst.write(Reg::S(EXEC)).is_some() || inst.read(Reg::S(EXEC)).is_some();
                if (takes_mask || touches_exec)
                    && let Some(r) = self.sop(inst).filter(|r| r.derived)
                {
                    for (k, m) in r.dst.into_iter().chain(r.exec) {
                        self.masks.insert(k, m);
                        if r.exec.is_some_and(|e| e.0 == k)
                            || exec.is_some_and(|e| self.b.implies(m, e))
                        {
                            self.active.insert(k.0);
                        }
                    }
                    if let (Some(scc), Some(m)) = (inst.write(Reg::Scc), r.scc) {
                        self.scc_lane.insert(scc, m);
                    }
                }
                let width = if self.p.wave64 { "64" } else { "32" };
                if takes_mask
                    && let (Some(rest), Some(scc)) =
                        (name.strip_prefix("s_cmp_"), inst.write(Reg::Scc))
                    && rest
                        .strip_suffix(width)
                        .is_some_and(|r| r == "eq_u" || r == "lg_u")
                    && let [a, c] = &src[..]
                    && let (Some((a, _)), Some((c, _))) =
                        (self.operand(inst, a), self.operand(inst, c))
                {
                    let ne = self.b.xor(a, c);
                    let m = if rest.starts_with("eq") {
                        self.b.not(ne)
                    } else {
                        ne
                    };
                    self.scc_lane.insert(scc, m);
                    if rest.starts_with("eq") {
                        self.scc_every.insert(scc);
                    }
                }
                if takes_mask
                    && name.starts_with("s_bcnt1_i32_b")
                    && let Some(scc) = inst.write(Reg::Scc)
                    && let [a] = &src[..]
                    && let Some((m, true)) = self.operand(inst, a)
                {
                    let e = exec.unwrap_or(Bool::TRUE);
                    let own = self.b.given(m, e);
                    if self.b.implies(m, e)
                        && self
                            .b
                            .support(own)
                            .iter()
                            .all(|a| matches!(a, Atom::Uniform(_)))
                    {
                        self.scc_lane.insert(scc, own);
                    }
                }
            }
            Fields::Valu(v) => {
                let exec = self.exec(inst);
                let lane = |l: &mut Self, k: Key| {
                    let born = l.born(k.0);
                    let a = l.b.atom(Atom::Lane(k.0), born);
                    let m = l.b.and(exec, a);
                    l.masks.insert(k, m);
                    l.active.insert(k.0);
                };
                if (name == "v_cndmask_b32" || name.contains("_co_ci_"))
                    && let Some(&Operand::Scalar { code, .. }) = v.src.get(2)
                    && code != NULL
                    && let Some(k) = self.key(|r| inst.read(r), code)
                {
                    if name == "v_cndmask_b32" {
                        self.mask(k);
                    } else {
                        self.seen.push(k);
                    }
                }
                if matches!(name, "v_div_fmas_f32" | "v_div_fmas_f64")
                    && let Some(k) = self.key(|r| inst.read(r), VCC)
                {
                    self.mask(k);
                }
                if name.starts_with("v_cmpx") {
                    if let Some(k) = self.key(|r| inst.write(r), EXEC) {
                        lane(self, k);
                    }
                } else {
                    let compare = v.dst.filter(|_| name.starts_with("v_cmp"));
                    for op in compare.iter().chain(&v.carry) {
                        if let Operand::Scalar { code, .. } = *op
                            && let Some(k) = self.key(|r| inst.write(r), code)
                        {
                            lane(self, k);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn wave_kind(&self, inst: &Inst) -> Option<bool> {
        match inst.name() {
            "s_cbranch_execnz" | "s_cbranch_vccnz" => Some(true),
            "s_cbranch_execz" | "s_cbranch_vccz" => Some(false),
            n @ ("s_cbranch_scc0" | "s_cbranch_scc1") => {
                let scc = inst.read(Reg::Scc)?;
                self.scc_lane.get(&scc)?;
                let every = self.scc_every.contains(&scc);
                Some((n == "s_cbranch_scc1") != every)
            }
            _ => None,
        }
    }

    fn branch(&mut self, inst: &Inst) -> Bool {
        let exec = self.exec(inst);
        match inst.name() {
            "s_cbranch_execz" => self.b.not(exec),
            "s_cbranch_execnz" => exec,
            n @ ("s_cbranch_vccz" | "s_cbranch_vccnz") => {
                let vcc = match self.key(|r| inst.read(r), VCC) {
                    Some(k) => self.mask(k).0,
                    None => Bool::FALSE,
                };
                let any = self.b.and(vcc, exec);
                if n.ends_with("nz") {
                    any
                } else {
                    self.b.not(any)
                }
            }
            n => {
                let Some(scc) = inst.read(Reg::Scc) else {
                    return Bool::FALSE;
                };
                let set = match self.scc_lane.get(&scc) {
                    Some(&m) => m,
                    None => {
                        let born = self.born(scc);
                        self.b.atom(Atom::Uniform(scc), born)
                    }
                };
                if n == "s_cbranch_scc1" {
                    set
                } else {
                    self.b.not(set)
                }
            }
        }
    }

    fn paths(&mut self) -> Result<Vec<Bool>, ShaderError> {
        let n = self.p.blocks.len();
        let mut cond = vec![Bool::FALSE; n];
        if let Some(&entry) = self.order.first() {
            cond[entry] = Bool::TRUE;
        }
        for _ in 0..MAX_ROUNDS {
            let mut changed = false;
            for i in 1..self.order.len() {
                let s = self.order[i];
                let mut c = Bool::FALSE;
                for k in 0..self.incoming[s].len() {
                    let (q, taken) = self.incoming[s][k];
                    let mut e = self.edge(cond[q], q, taken);
                    if i <= self.index[q] {
                        e = self.b.forget(e, i, &mut Map::default());
                    }
                    c = self.b.or(c, e);
                }
                if c != cond[s] {
                    cond[s] = c;
                    changed = true;
                }
            }
            if self.b.overflow {
                return Err(TOO_LARGE);
            }
            if !changed {
                return Ok(cond);
            }
        }
        Err(ShaderError::Isa {
            offset: 0,
            reason: "lane conditions do not settle",
        })
    }
}

const TOO_LARGE: ShaderError = ShaderError::Isa {
    offset: 0,
    reason: "lane conditions too large",
};

fn check_reducible(p: &Program, order: &[usize], index: &[usize]) -> Result<(), ShaderError> {
    match crate::ir::irreducible_edge(&p.blocks, order, index) {
        Some((from, _)) => Err(ShaderError::Unsupported {
            offset: p.blocks[from].offset,
            what: "irreducible control flow",
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn build(words: &[u32], wave64: bool) -> Program {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        ir::build(&code, wave64, 256).unwrap()
    }

    fn guarded(s: &Structured) -> usize {
        s.guards.iter().flatten().filter(|g| g.is_some()).count()
    }

    fn meet_anew(b: &Bools, mut all: Vec<(Bool, bool)>, budget: &mut usize) -> bool {
        let fails = |&(b, holds): &(Bool, bool)| b == if holds { Bool::FALSE } else { Bool::TRUE };
        if all.iter().any(fails) {
            return false;
        }
        all.retain(|&(b, _)| b != Bool::FALSE && b != Bool::TRUE);
        if all.is_empty() || *budget == 0 {
            return true;
        }
        *budget -= 1;
        let top = |x: Bool| b.nodes[x.0 as usize].0;
        let v = all.iter().map(|&(x, _)| top(x)).min().expect("not empty");
        [false, true].into_iter().any(|value| {
            let next = all
                .iter()
                .map(|&(x, holds)| {
                    let (lo, hi) = b.split(x, v);
                    (if value { hi } else { lo }, holds)
                })
                .collect();
            meet_anew(b, next, budget)
        })
    }

    #[test]
    fn a_search_that_remembers_answers_as_one_made_anew() {
        let mut b = Bools::new();
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut random = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        let mut f: Vec<Bool> = (0..12).map(|i| b.atom(Atom::Constant(i + 2), 0)).collect();
        f.extend([Bool::FALSE, Bool::TRUE]);
        for _ in 0..120 {
            let (x, y) = (f[random(f.len())], f[random(f.len())]);
            let made = match random(4) {
                0 => b.and(x, y),
                1 => b.or(x, y),
                2 => b.xor(x, y),
                _ => b.not(x),
            };
            f.push(made);
        }
        let mut seen = Meets::default();
        let (mut none, mut some, mut gave_up) = (0, 0, 0);
        for _ in 0..1500 {
            let all: Vec<(Bool, bool)> = (0..2 + random(4))
                .map(|_| (f[random(f.len())], random(2) == 0))
                .collect();
            for budget in [0, 1, 2, 3, 5, 9, 20, 60, 250, 1000, MAX_MEET] {
                let mut left = budget;
                let anew = meet_anew(&b, all.clone(), &mut left);
                for fresh in [false, true] {
                    let mut empty = Meets::default();
                    let seen = if fresh { &mut empty } else { &mut seen };
                    let mut work = all
                        .iter()
                        .map(|&(x, holds)| x.0 << 1 | u32::from(holds))
                        .collect();
                    let mut mine = budget;
                    let found = b.meet_in(0, &mut work, &mut mine, seen);
                    assert_eq!(found, anew, "{all:?} within {budget}");
                    assert!(work.is_empty());
                    if !found {
                        assert_eq!(mine, left, "{all:?} within {budget}");
                    }
                }
                assert_eq!(b.meet_within(&all, budget, &mut seen), anew);
                if !anew {
                    none += 1;
                } else if meet_anew(&b, all.clone(), &mut usize::MAX.clone()) {
                    some += 1;
                } else {
                    gave_up += 1;
                }
            }
        }
        assert!(
            none > 100 && some > 100 && gave_up > 100,
            "{none} {some} {gave_up}"
        );
        assert!(seen.none.len() > 100);
    }

    #[test]
    fn divergent_if_becomes_a_lane_branch() {
        let p = build(
            &[
                0x7D84_0080,
                0xBE80_3C6A,
                0xBF88_0001,
                0x7E02_0280,
                0x887E_007E,
                0x7E04_0280,
                S_ENDPGM,
            ],
            false,
        );
        let s = structure(&p).unwrap();
        let LaneExit::Branch {
            when,
            taken: 2,
            not_taken: 1,
        } = s.exits[0]
        else {
            panic!("{:?}", s.exits)
        };
        let cmp = p.blocks[0].insts[0].write(Reg::S(VCC)).unwrap();
        assert_eq!(
            s.bools.node(when),
            Some((Atom::Lane(cmp), Bool::TRUE, Bool::FALSE))
        );
        assert_eq!(guarded(&s), 0, "{:?}", s.guards);
    }

    #[test]
    fn exec_narrowed_without_a_branch_leaves_a_guard() {
        let p = build(&[0x7DA4_0080, 0x7E02_0280, S_ENDPGM], true);
        let s = structure(&p).unwrap();
        assert_eq!(s.guards[0][0], None, "the compare runs in every lane");
        let g = s.guards[0][1].expect("guard");
        assert!(matches!(s.bools.support(g)[..], [Atom::Lane(_)]));
    }

    #[test]
    fn divergent_loop_exits_per_lane() {
        let p = build(
            &[
                0xBE82_0480,
                0x4A00_0081,
                0x7D84_0080,
                0x8882_026A,
                0x8AFE_027E,
                0xBF89_FFFB,
                0x88FE_027E,
                0x7E02_0280,
                S_ENDPGM,
            ],
            true,
        );
        let s = structure(&p).unwrap();
        assert_eq!(guarded(&s), 0, "{:?}", s.guards);
        let LaneExit::Branch {
            when,
            taken: 1,
            not_taken: 2,
        } = s.exits[1]
        else {
            panic!("{:?}", s.exits)
        };
        assert!(matches!(
            s.bools.node(when),
            Some((Atom::Lane(_), Bool::TRUE, Bool::FALSE))
        ));
    }

    #[test]
    fn a_select_on_the_condition_a_lane_loop_ended_with_takes_its_second_operand() {
        let cnd = 256 | (256 << 9) | (3 << 18);
        let p = build(
            &[
                0xBE83_037E,
                0xBE9C_037E,
                0x7E3A_0507,
                0x7DA4_0E1D,
                0xBE9C_471C,
                0xBF89_FFFC,
                0xBEFE_0303,
                0x8503_807E,
                0xD501_0001,
                cnd,
                S_ENDPGM,
            ],
            false,
        );
        let s = structure(&p).unwrap();
        let select = p
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .find(|i| i.name() == "s_cselect_b32")
            .expect("the select");
        let mask = select.write(Reg::S(3)).unwrap();
        assert_eq!(s.masks.get(&mask), Some(&Bool::FALSE));
    }

    #[test]
    fn scalar_branches_stay_uniform() {
        let p = build(&[0xBF06_0100, 0xBF85_0001, 0x7E02_0280, S_ENDPGM], true);
        let s = structure(&p).unwrap();
        let LaneExit::Branch { when, .. } = s.exits[0] else {
            panic!()
        };
        assert!(matches!(s.bools.support(when)[..], [Atom::Uniform(_)]));
        assert_eq!(guarded(&s), 0);
    }

    #[test]
    fn a_mask_whose_high_dword_is_rewritten_is_not_the_compare() {
        for reader in [0x0202_0480, 0x5002_0480] {
            let p = build(&[0x7D84_0080, 0xBEEB_0380, reader, S_ENDPGM], true);
            let s = structure(&p).unwrap();
            let lo = p.blocks[0].insts[2].read(Reg::S(VCC)).unwrap();
            assert_eq!(p.blocks[0].insts[0].write(Reg::S(VCC)), Some(lo));
            assert_ne!(
                s.masks.get(&lo).and_then(|&m| s.bools.node(m)),
                Some((Atom::Lane(lo), Bool::FALSE, Bool::TRUE)),
                "{reader:#x}: vcc_hi was zeroed after the compare"
            );
        }
    }

    #[test]
    fn a_mask_joined_over_a_back_edge_with_a_rewritten_high_dword_is_not_the_compare() {
        let p = build(
            &[
                0x7D84_0080,
                0x0202_0480,
                0x7D84_0280,
                0xBEEB_0380,
                0xBF85_FFFC,
                S_ENDPGM,
            ],
            true,
        );
        let s = structure(&p).unwrap();
        let lo = p.blocks[1].insts[1].write(Reg::S(VCC)).unwrap();
        assert_ne!(
            s.masks.get(&lo).and_then(|&m| s.bools.node(m)),
            Some((Atom::Lane(lo), Bool::FALSE, Bool::TRUE))
        );
    }

    #[test]
    fn irreducible_loops_are_split() {
        let p = build(
            &[0xBF06_0100, 0xBF85_0001, 0x8002_8102, 0xBF84_FFFE, S_ENDPGM],
            false,
        );
        let copy = &p.blocks[4];
        assert_eq!((copy.offset, copy.exit), (8, ir::Exit::Jump(2)));
        assert_eq!(p.blocks[0].exit.successors(), [2, 4]);
        assert_eq!(p.blocks[1].preds, [2]);
        assert!(structure(&p).is_ok());
    }

    #[test]
    fn mask_phi_joining_constant_and_compare_records_edge_bools() {
        let cnd = 258 | (259 << 9) | (2 << 18);
        let p = build(
            &[
                0xBF06_8000,
                0xBF85_0002,
                0xBE82_0380,
                0xBF82_0002,
                0xD4C2_0002,
                0x0002_0080,
                0xD501_0001,
                cnd,
                S_ENDPGM,
            ],
            false,
        );
        let s = structure(&p).unwrap();
        let phi = p
            .blocks
            .iter()
            .flat_map(|b| &b.phis)
            .find(|phi| phi.reg == Reg::S(2))
            .expect("mask phi for s2");
        assert_eq!(
            s.masks.get(&phi.value).and_then(|&m| s.bools.node(m)),
            Some((Atom::Phi(phi.value), Bool::FALSE, Bool::TRUE))
        );
        let edges = &s.phi_masks[&phi.value];
        assert_eq!(edges.len(), 2);
        assert!(edges.contains(&Bool::FALSE), "{edges:?}");
        assert!(
            edges
                .iter()
                .any(|&e| matches!(s.bools.support(e)[..], [Atom::Lane(_)])),
            "{edges:?}"
        );
    }

    #[test]
    fn loop_mask_accumulator_from_zero_tracks_phi_and_edges() {
        let cnd = 258 | (259 << 9) | (2 << 18);
        let p = build(
            &[
                0xBE82_0480,
                0xBE84_0380,
                0xD501_0001,
                cnd,
                0x7D84_0080,
                0x8882_026A,
                0x8004_8104,
                0xBF0A_8404,
                0xBF85_FFF9,
                0xD501_0004,
                cnd,
                S_ENDPGM,
            ],
            true,
        );
        let s = structure(&p).unwrap();
        let phi = p
            .blocks
            .iter()
            .flat_map(|b| &b.phis)
            .find(|phi| phi.reg == Reg::S(2))
            .expect("loop mask phi for s[2:3]");
        assert!(s.masks.contains_key(&phi.value));
        let edges = &s.phi_masks[&phi.value];
        assert_eq!(edges.len(), 2);
        assert!(edges.contains(&Bool::FALSE), "{edges:?}");
        assert!(edges.iter().any(|&e| {
            let sup = s.bools.support(e);
            sup.contains(&Atom::Phi(phi.value)) && sup.iter().any(|a| matches!(a, Atom::Lane(_)))
        }));
    }

    #[test]
    fn loop_mask_phi_reset_to_constant_on_back_edge() {
        let cnd = 258 | (259 << 9) | (2 << 18);
        let p = build(
            &[
                0xD4C2_0002,
                0x0002_0080,
                0xBE84_0380,
                0xD501_0001,
                cnd,
                0xBE82_0380,
                0x8004_8104,
                0xBF0A_8404,
                0xBF85_FFFA,
                S_ENDPGM,
            ],
            false,
        );
        let s = structure(&p).unwrap();
        let phi = p
            .blocks
            .iter()
            .flat_map(|b| &b.phis)
            .find(|phi| phi.reg == Reg::S(2))
            .expect("loop mask phi for s2");
        assert!(s.masks.contains_key(&phi.value));
        let edges = &s.phi_masks[&phi.value];
        assert_eq!(edges.len(), 2);
        assert!(edges.contains(&Bool::FALSE), "{edges:?}");
        assert!(
            edges
                .iter()
                .any(|&e| matches!(s.bools.support(e)[..], [Atom::Lane(_)])),
            "{edges:?}"
        );
    }
}
