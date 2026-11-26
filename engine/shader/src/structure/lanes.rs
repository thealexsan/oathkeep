use std::collections::{HashMap, HashSet};

use crate::ShaderError;
use crate::gfx10::Fields;
use crate::ir::{EXEC, Exit, Program, Reg, Value};

use super::{
    Bool, Bools, LaneExit, Lanes, bools::Map, Structured, TOO_LARGE,
    check_reducible,
};

impl<'a> Lanes<'a> {
    pub(super) fn new(p: &'a Program) -> Result<Self, ShaderError> {
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

    pub(super) fn reset(&mut self) {
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

    /// Lane masks, branch conditions and forward-edge knowledge, block by
    /// block in reverse postorder; returns EXEC at each instruction reading it
    /// and EXEC at the end of each block.
    pub(super) fn masks_in_order(&mut self) -> (Vec<Vec<Option<Bool>>>, Vec<Bool>) {
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
        let mut exit_exec = vec![Bool::TRUE; p.blocks.len()];
        for n in 0..self.order.len() {
            let b = self.order[n];
            let mut c = if n == 0 { Bool::TRUE } else { Bool::FALSE };
            let mut pred_exec = None;
            for k in 0..self.incoming[b].len() {
                let (q, taken) = self.incoming[b][k];
                if self.index[q] < n {
                    let e = self.edge(self.first[q], q, taken);
                    c = self.b.or(c, e);
                    pred_exec.get_or_insert(exit_exec[q]);
                }
            }
            self.first[b] = c;
            // EXEC joined at the block's entry holds the lanes that arrive.
            let mut phi_lo = None;
            let mut phi_hi = None;
            for phi in &p.blocks[b].phis {
                if phi.reg == Reg::S(EXEC) {
                    self.active.insert(phi.value);
                    phi_lo = Some(phi.value);
                } else if p.wave64 && phi.reg == Reg::S(EXEC + 1) {
                    phi_hi = Some(phi.value);
                }
            }
            // EXEC where known in this block (read or written so far).
            let mut now = match phi_lo {
                Some(lo) => {
                    let hi = if p.wave64 { phi_hi.unwrap_or(lo) } else { lo };
                    Some(self.mask((lo, hi)).0)
                }
                None => pred_exec,
            };
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
            exit_exec[b] = now.unwrap_or(Bool::TRUE);
            if let Exit::Branch { .. } = p.blocks[b].exit
                && let Some(last) = p.blocks[b].insts.last()
            {
                self.when[b] = Some(self.branch(last));
                self.wave_any[b] = self.wave_kind(last);
            }
        }
        (execs, exit_exec)
    }

    /// `c` narrowed by the branch at the end of `q` towards one successor.
    pub(super) fn edge(&mut self, c: Bool, q: usize, taken: bool) -> Bool {
        if self.wide.contains(&q) {
            // the wave's decision: it says nothing about this lane's own bit
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

    /// Joins whose constant fails on some edge, and joins that need another
    /// pass because a later edge turned out to derive from EXEC or VALU.
    pub(super) fn check_joins(&mut self, cond: &[Bool]) -> (Vec<Value>, Vec<Value>) {
        let mut wrong = Vec::new();
        let mut new_derived = Vec::new();
        let mut seen_derived = HashSet::new();
        let mut j = 0;
        let mut p = 0;
        // Checking an argument can resolve another join; it is checked too.
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

    pub(super) fn finish(
        mut self,
        execs: &[Vec<Option<Bool>>],
        exit_exec: &[Bool],
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
                // Reachability is fixed within the block. Vector instructions
                // with the same EXEC therefore need the same guard.
                let mut last_guard = None;
                block
                    .insts
                    .iter()
                    .zip(&execs[n])
                    .map(|(inst, exec)| match (exec, &inst.fields) {
                        (None, _)
                        | (_, Fields::Sop { .. } | Fields::Sopp { .. } | Fields::Smem { .. }) => {
                            None
                        }
                        (&Some(e), _) => {
                            if let Some((prev, guard)) = last_guard
                                && prev == e
                            {
                                return guard;
                            }
                            let guard = (!b.implies(cond[n], e)).then(|| b.given(e, cond[n]));
                            last_guard = Some((e, guard));
                            guard
                        }
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
        let exit_exec: Vec<Bool> = exit_exec
            .iter()
            .enumerate()
            .map(|(n, &e)| b.and(cond[n], e))
            .collect();
        if self.b.overflow {
            return Err(TOO_LARGE);
        }
        // Exports go by low dword. A low dword read with two high dwords
        // (one rewritten alone) names no one mask: it is left out, so
        // lowering refuses what reads it instead of taking the other
        // pair's lanes 32-63. Not where the low dword holds a number: see
        // the shortcut in `mask`.
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
            paired_masks: if self.p.wave64 { self.masks } else { Map::default() },
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
            exit_exec,
        })
    }

}
