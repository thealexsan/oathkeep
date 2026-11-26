
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{Inst, Reg, Value};
use crate::native::RootType;
use crate::structure::LaneExit;

use super::{
    Address, CODE, Known, MAX_JOINS, Site, Source, Sum, Term, Tracer,
};

impl<'a> Tracer<'a> {
    pub(super) fn flat_address(
        &self,
        inst: &Inst,
        addr: Option<&Operand>,
        saddr: Option<&Operand>,
        offset: i16,
    ) -> Result<Address, &'static str> {
        let Some(&Operand::Vector { reg, .. }) = addr else {
            return Err("a global address not in VGPRs");
        };
        let off = offset as i32 as u32;
        match saddr {
            Some(&Operand::Scalar { code, .. }) if code != NULL => {
                let word = |k| Some(self.origin(inst.read(Reg::S(code + k))?));
                let (Some(lo), Some(hi)) = (word(0), word(1)) else {
                    return Err("an address not in SGPRs");
                };
                let v = inst.read(Reg::V(reg)).ok_or("a global address")?;
                let mut extra = self.sum(self.vorigin(v));
                if extra.base.is_some() {
                    return Err("an offset from a root parameter");
                }
                extra.offset = extra.offset.wrapping_add(off);
                self.address(lo, hi, extra)
            }
            _ => {
                let word = |k| inst.read(Reg::V(reg + k)).map(|v| self.vorigin(v));
                let (Some(lo), Some(hi)) = (word(0), word(1)) else {
                    return Err("a global address");
                };
                self.address(
                    lo,
                    hi,
                    Sum {
                        base: None,
                        offset: off,
                        dynamic: None,
                    },
                )
            }
        }
    }

    /// The address `lo` and `hi` hold, plus `extra` (an instruction's
    /// immediate and offset register).
    pub(super) fn address(&self, lo: Term, hi: Term, extra: Sum) -> Result<Address, &'static str> {
        // The high half as the lanes that read through the address hold
        // it, where that is a constant or what a register held on entry
        // (the code clears a table address's high half inside a branch
        // and uses the address after the join).
        let hi = match self.live(hi) {
            t @ Term::Const(_) => t,
            Term::Value(v) if self.inputs[v.0 as usize].is_some() => Term::Value(v),
            _ => hi,
        };
        let s = self.sum(lo);
        if s.dynamic.is_some() && extra.dynamic.is_some() {
            return Err("an address with two computed terms");
        }
        let base = s.base.ok_or_else(|| self.why_no_base(s.dynamic))?;
        if let Some(pc) = self.getpc(base) {
            // A table in the code: the base is the next instruction's offset.
            let high = pc.writes.get(1).map(|w| w.1);
            if !self.high_half(hi, high) && !high.is_some_and(|h| self.carried(hi, h)) {
                return Err("an address with an untracked high half");
            }
            // A join of paths into tables in the code is the byte in the code
            // already: each path's `s_getpc_b64` counts as its own offset.
            let joined = s.dynamic.is_some_and(|d| self.code_join(d).is_some());
            let first = if joined { 0 } else { pc.machine.offset as u32 + 4 };
            return Ok(Address {
                param: CODE,
                offset: first.wrapping_add(s.offset).wrapping_add(extra.offset),
                dynamic: s.dynamic.or(extra.dynamic),
            });
        }
        let reg = self.inputs[base.0 as usize].ok_or("an address not from a root parameter")?;
        let (param, _) = self
            .param_of(reg)
            .ok_or("an address not from a root parameter")?;
        let high = match self.layout[param].root {
            RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap => None,
            RootType::Descriptor => {
                let high = Reg::S(self.stage.user_sgpr(self.layout[param].user_data) + 1);
                let input = self.p.inputs.iter().find(|i| i.0 == high);
                Some(input.ok_or("a root descriptor without its high half")?.1)
            }
            _ => return Err("an address from a root parameter that is no address"),
        };
        // shortcut: with a computed term, a high half that adds anything to the
        // base's is taken as the carry of that term (offsets below 4 GB).
        let dynamic = s.dynamic.or(extra.dynamic);
        if !self.high_half(hi, high)
            && !(dynamic.is_some() && high.is_some_and(|h| self.carried(hi, h)))
        {
            return Err("an address with an untracked high half");
        }
        Ok(Address {
            param,
            offset: s.offset.wrapping_add(extra.offset),
            dynamic: s.dynamic.or(extra.dynamic),
        })
    }

    /// `hi` is `high` (0 for a table), possibly plus a carry.
    pub(super) fn high_half(&self, hi: Term, high: Option<Value>) -> bool {
        let want = high.map_or(Term::Const(0), Term::Value);
        hi == want
            || matches!(hi, Term::Value(v) if self.sop2(v, &["s_addc_u32"]).is_some_and(
                |(_, [a, b])| a == want && b == Term::Const(0) || b == want && a == Term::Const(0)
            ))
    }

    /// `hi` adds something (with carry) to `high`: `s_addc_u32` or
    /// `v_add_co_ci_u32` with `high` as an operand.
    pub(super) fn carried(&self, hi: Term, high: Value) -> bool {
        let Term::Value(v) = hi else { return false };
        if v == high {
            return true;
        }
        // A join ([`Self::code_join`]): every path carries into the table's high half.
        if let Some((b, i)) = self.phis[v.0 as usize] {
            // A path may carry into another table's high half ([`Self::code_join`]).
            let high_of = |t: Term| self.is_high(t, high) || self.code_high(t);
            return self.p.blocks[b].phis[i].args.iter().all(|&a| {
                let t = self.vorigin(a);
                if t == Term::Value(v) || high_of(t) {
                    return true;
                }
                let Term::Value(x) = t else {
                    return false;
                };
                // Computed in VGPRs and taken from the first lane.
                let x = match self.first_lane(x).map(|lane| self.vorigin(lane)) {
                    Some(Term::Value(y)) => y,
                    Some(Term::Const(_)) => return false,
                    None => x,
                };
                let Some(inst) = self.def(x) else {
                    return false;
                };
                let Fields::Valu(f) = &inst.fields else {
                    return false;
                };
                inst.name() == "v_add_co_ci_u32"
                    && f.src[..2.min(f.src.len())]
                        .iter()
                        .any(|o| self.vterm(inst, o).is_some_and(high_of))
            });
        }
        if let Some(lane) = self.first_lane(v) {
            return self.carried(self.vorigin(lane), high);
        }
        if let Some((_, [a, b])) = self.sop2(v, &["s_addc_u32"]) {
            return self.carried(a, high) || self.carried(b, high);
        }
        let Some(inst) = self.def(v) else {
            return false;
        };
        let Fields::Valu(f) = &inst.fields else {
            return false;
        };
        inst.name() == "v_add_co_ci_u32"
            && f.src[..2.min(f.src.len())]
                .iter()
                .any(|o| self.vterm(inst, o).is_some_and(|t| self.carried(t, high)))
    }

    pub(super) fn why_no_base(&self, dynamic: Option<Value>) -> &'static str {
        let Some(v) = dynamic else {
            return "a constant address";
        };
        if self.inputs[v.0 as usize].is_some() {
            return "an address in an SGPR that is no root parameter";
        }
        if self
            .leaves(v)
            .any(|l| self.def(l).is_some_and(|i| i.name() == "s_getpc_b64"))
        {
            return "an address relative to the program counter";
        }
        match self.def(v).map(|i| i.name()) {
            Some(n) if n.starts_with("s_load_") => "an address loaded from memory",
            None => "an address from a phi",
            _ => "an address not from a root parameter",
        }
    }

    /// The terms of the additions that compute `v`.
    pub(super) fn leaves(&self, v: Value) -> Box<dyn Iterator<Item = Value> + '_> {
        match self.sop2(v, &["s_add_u32", "s_add_i32"]) {
            Some((_, terms)) => Box::new(terms.into_iter().flat_map(move |t| match t {
                Term::Value(x) => self.leaves(x),
                Term::Const(_) => Box::new(std::iter::empty()),
            })),
            None => Box::new(std::iter::once(v)),
        }
    }

    pub(super) fn smem_address(
        &self,
        inst: &Inst,
        base: &Operand,
        offset: i32,
        soffset: &Operand,
    ) -> Result<Address, &'static str> {
        let Operand::Scalar { code, .. } = *base else {
            return Err("an address not in SGPRs");
        };
        let word = |k| Some(self.origin(inst.read(Reg::S(code + k))?));
        let (Some(lo), Some(hi)) = (word(0), word(1)) else {
            return Err("an address not in SGPRs");
        };
        let mut extra = self.sum(
            self.term(inst, soffset)
                .ok_or("an unknown offset operand")?,
        );
        if extra.base.is_some() {
            return Err("an offset from a root parameter");
        }
        extra.offset = extra.offset.wrapping_add(offset as u32);
        self.address(lo, hi, extra)
    }

    /// The source of the descriptor operand `op` of `inst`, at `at`.
    pub(super) fn descriptor(&self, inst: &Inst, op: &Operand, at: Site) -> Result<Source, &'static str> {
        let Operand::Scalar { code, dwords } = *op else {
            return Err("a descriptor not in SGPRs");
        };
        let mut words = (0..dwords)
            .map(|k| inst.read(Reg::S(code + k)).map(|v| self.origin(v)))
            .collect::<Option<Vec<_>>>()
            .ok_or("a descriptor not in SGPRs")?;
        // A byte access through a buffer descriptor made raw reads what the
        // descriptor's view does with a raw host view (Oathkeep upstream 0b35b7f).
        if !inst.name().contains("format")
            && let Some(raw) = self.made_raw(&words)
        {
            words = raw.to_vec();
        }
        // Only the lanes that reach and run the access read the descriptor.
        let mut known = Vec::new();
        if let Some(l) = self.lanes {
            known.push((l.reach[at.0], true));
            if let Some(guard) = l.guards[at.0][at.1] {
                known.push((guard, true));
            }
        }
        self.classify(&words, &mut Vec::new(), &mut known)
    }

    /// The source of a descriptor's words; `joins` are the phi blocks
    /// being resolved, `known` holds in every lane that reads them.
    pub(super) fn classify(
        &self,
        words: &[Term],
        joins: &mut Vec<usize>,
        known: &mut Known,
    ) -> Result<Source, &'static str> {
        if let Some(source) = self.phi(words, joins, known)? {
            return Ok(source);
        }
        if let Some(source) = self.select(words, joins, known)? {
            return Ok(source);
        }
        let read = |w: &Term| match *w {
            Term::Value(v) => self.first_lane_source(v),
            Term::Const(_) => None,
        };
        if let Some(sources) = words.iter().map(read).collect::<Option<Vec<_>>>() {
            // One descriptor the code keeps per lane: the lanes agree.
            if let Some(source) = self.lane_copies(words) {
                return Ok(source);
            }
            // Read from the first active lane, as a waterfall loop does: the
            // dwords each lane holds.
            let lanes: Vec<Term> = sources.into_iter().map(Term::Value).collect();
            return self.per_lane(&lanes, joins, known);
        }
        if words.iter().any(|w| read(w).is_some()) {
            return Err("a descriptor partly read from the first lane");
        }
        if let Some(consts) = words
            .iter()
            .map(|w| match *w {
                Term::Const(c) => Some(c),
                Term::Value(_) => None,
            })
            .collect::<Option<Vec<_>>>()
        {
            return Ok(Source::Constant(consts));
        }
        if let Some(source) = self.loaded(words) {
            return Ok(source);
        }
        if let [lo, hi, Term::Const(w2), Term::Const(w3)] = *words {
            let (mut hi, mut bits) = (hi, 0);
            while let Term::Value(v) = hi
                && let Some((x, more)) = self.set_bits(v)
            {
                (hi, bits) = (x, bits | more);
            }
            let address = self.address(
                lo,
                hi,
                Sum {
                    base: None,
                    offset: 0,
                    dynamic: None,
                },
            )?;
            return Ok(Source::Built {
                address,
                bits,
                words: [w2, w3],
            });
        }
        let loaded = |w: &Term| {
            matches!(*w, Term::Value(v)
                if self.def(v).is_some_and(|i| i.name().starts_with("s_load_")))
        };
        Err(if words.iter().any(loaded) {
            "a descriptor changed after loading"
        } else {
            "a descriptor neither loaded nor built"
        })
    }

    /// The one edge into the join `block` that lanes satisfying `known`
    /// can have taken, if the lanes' paths ([`Structured`]) rule out every
    /// other: the console compiler loads a descriptor (or clears an
    /// address's high half) inside a branch that skips code when no lane
    /// is left for it, and uses it after the join, where only lanes that
    /// ran the branch are left (the title's
    /// `CS_Alpha_Schedule`: the registers' values from before the branch,
    /// which the join also sees, are what a wave with no such lane holds).
    /// None at loop headers and without the lanes' paths.
    pub(super) fn only_edge(&self, block: usize, known: &mut Known) -> Option<usize> {
        let lanes = self.lanes?;
        let preds = &self.p.blocks[block].preds;
        if preds.len() < 2 || preds.iter().any(|&q| self.rpo[q] >= self.rpo[block]) {
            return None;
        }
        let mut only = None;
        for (j, &q) in preds.iter().enumerate() {
            let n = known.len();
            known.push((lanes.reach[q], true));
            if let LaneExit::Branch {
                when,
                taken,
                not_taken,
            } = lanes.exits[q]
                && taken != not_taken
            {
                known.push((when, taken == block));
            }
            let possible = self.meet(lanes, known);
            known.truncate(n);
            if possible {
                if only.is_some() {
                    return None;
                }
                only = Some(j);
            }
        }
        only
    }

    /// Whether lowering can make a handle for `source` in any block: its
    /// heap index comes from root parameters and constants alone (a table
    /// offset the code computes exists only where the load is).
    pub(super) fn anywhere(&self, source: &Source) -> bool {
        match source {
            Source::Constant(_) => true,
            Source::Built { address, .. } => address.dynamic.is_none(),
            Source::Loaded { load, .. } => {
                let inst = self.inst(*load);
                match &inst.fields {
                    Fields::Smem {
                        base,
                        offset,
                        soffset,
                        ..
                    } => self
                        .smem_address(inst, base, *offset, soffset)
                        .is_ok_and(|a| a.dynamic.is_none()),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// `t` as the lanes that reach the access being traced hold it: through
    /// joins they can have entered by one edge only ([`Self::only_edge`]).
    pub(super) fn live(&self, mut t: Term) -> Term {
        let (Some(at), Some(lanes)) = (self.at.get(), self.lanes) else {
            return t;
        };
        let mut known = vec![(lanes.reach[at.0], true)];
        if let Some(guard) = lanes.guards[at.0][at.1] {
            known.push((guard, true));
        }
        for _ in 0..MAX_JOINS {
            let Term::Value(v) = t else { break };
            let Some((block, i)) = self.phis[v.0 as usize] else {
                break;
            };
            let Some(j) = self.only_edge(block, &mut known) else {
                break;
            };
            t = self.origin(self.p.blocks[block].phis[i].args[j]);
        }
        t
    }

    /// Words that phis of one block join, traced per predecessor (words
    /// defined before the join hold on every edge).
    pub(super) fn phi(
        &self,
        words: &[Term],
        joins: &mut Vec<usize>,
        known: &mut Known,
    ) -> Result<Option<Source>, &'static str> {
        let phi_of = |w: &Term| match *w {
            Term::Value(v) => self.phis[v.0 as usize],
            Term::Const(_) => None,
        };
        let Some((block, _)) = words.iter().find_map(phi_of) else {
            return Ok(None);
        };
        if words.iter().filter_map(phi_of).any(|(b, _)| b != block) {
            return Err("a descriptor joined in several blocks");
        }
        if joins.contains(&block) {
            return Err("a descriptor that changes in a loop");
        }
        // Each level multiplies the sources by the predecessor count.
        if joins.len() >= MAX_JOINS {
            return Err("a descriptor joined too often");
        }
        joins.push(block);
        let phis = &self.p.blocks[block].phis;
        let edge = |j: usize| -> Vec<Term> {
            words
                .iter()
                .map(|w| match phi_of(w) {
                    Some((_, i)) => self.origin(phis[i].args[j]),
                    None => *w,
                })
                .collect()
        };
        let args = (0..self.p.blocks[block].preds.len())
            .map(|j| self.classify(&edge(j), joins, known))
            .collect::<Result<_, _>>();
        let source = match args {
            Ok(args) => Ok(Source::Phi { block, args }),
            // An edge holds no descriptor. With only one edge the lanes
            // that read the descriptor can have come by, it is what the
            // registers hold on that one (a wave that took another edge
            // has no lane left that reads it), if that can be named where
            // it is read.
            Err(e) => match self.only_edge(block, known) {
                Some(j) => match self.classify(&edge(j), joins, known) {
                    Ok(source) if self.anywhere(&source) => Ok(source),
                    _ => Err(e),
                },
                None => Err(e),
            },
        };
        joins.pop();
        source.map(Some)
    }
}
