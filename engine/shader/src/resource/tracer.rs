
use crate::ShaderError;
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{Inst, Program, Reg, Value};
use crate::native::{HwStage, RootParam, RootType};
use crate::structure::{Bool, Structured};

use super::copy_tracking::{Moved, copy_source};
use super::{
    Address, Descriptor, MAX_ROUNDS, MEMORY,
    RAW, Role, RootIndexedView, Site, Source, Sum, Term, Tracer, inline,
};

impl<'a> Tracer<'a> {
    pub(super) fn new(
        p: &'a Program,
        layout: &'a [RootParam],
        stage: HwStage,
        lanes: Option<&'a Structured>,
    ) -> Result<Self, ShaderError> {
        let all = || {
            p.inputs
                .iter()
                .map(|i| i.1)
                .chain(p.blocks.iter().flat_map(|b| {
                    b.phis
                        .iter()
                        .map(|phi| phi.value)
                        .chain(b.insts.iter().flat_map(|i| i.writes.iter().map(|w| w.1)))
                }))
        };
        let n = all().map(|v| v.0 as usize + 1).max().unwrap_or(0);
        let mut defs = vec![None; n];
        let mut phis = vec![None; n];
        let mut inputs = vec![None; n];
        for &(reg, v) in &p.inputs {
            inputs[v.0 as usize] = Some(reg);
        }
        for (b, block) in p.blocks.iter().enumerate() {
            for (i, phi) in block.phis.iter().enumerate() {
                phis[phi.value.0 as usize] = Some((b, i));
            }
            for (i, inst) in block.insts.iter().enumerate() {
                for &(_, v) in &inst.writes {
                    defs[v.0 as usize] = Some((b, i));
                }
            }
        }
        let mut rpo = vec![usize::MAX; p.blocks.len()];
        for (n, b) in crate::ir::reverse_postorder(&p.blocks)
            .into_iter()
            .enumerate()
        {
            rpo[b] = n;
        }
        let mut t = Tracer {
            p,
            layout,
            stage,
            defs,
            phis,
            inputs,
            origin: Vec::new(),
            lane_copied: Default::default(),
            lanes,
            read_first_lane: Default::default(),
            meets: Default::default(),
            rpo,
            at: Default::default(),
        };
        t.origin = t.propagate_copies(n)?;
        Ok(t)
    }

    /// Optimistic copy propagation: a move's result is its source's
    /// origin, a phi's the one origin its arguments agree on.
    pub(super) fn propagate_copies(&self, n: usize) -> Result<Vec<Term>, ShaderError> {
        let mut o: Vec<Option<Term>> = vec![None; n];
        for &(_, v) in &self.p.inputs {
            o[v.0 as usize] = Some(Term::Value(v));
        }
        let order = crate::ir::reverse_postorder(&self.p.blocks);
        for _ in 0..MAX_ROUNDS {
            let mut changed = false;
            for &b in &order {
                let block = &self.p.blocks[b];
                for phi in &block.phis {
                    let own = Term::Value(phi.value);
                    let mut args = phi
                        .args
                        .iter()
                        .filter_map(|a| o[a.0 as usize])
                        .filter(|&t| t != own);
                    let new = args
                        .next()
                        .map(|first| if args.all(|t| t == first) { first } else { own });
                    if new.is_some() && new != o[phi.value.0 as usize] {
                        o[phi.value.0 as usize] = new;
                        changed = true;
                    }
                }
                for inst in &block.insts {
                    for (k, &(_, v)) in inst.writes.iter().enumerate() {
                        let new = match copy_source(inst, k).or_else(|| self.restored(inst, None)) {
                            Some(Moved::Const(c)) => Some(Term::Const(c)),
                            Some(Moved::Value(s)) => o[s.0 as usize],
                            None => Some(Term::Value(v)),
                        };
                        if new.is_some() && new != o[v.0 as usize] {
                            o[v.0 as usize] = new;
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                return Ok(o
                    .into_iter()
                    .enumerate()
                    .map(|(v, t)| t.unwrap_or(Term::Value(Value(v as u32))))
                    .collect());
            }
        }
        Err(ShaderError::Unsupported {
            offset: 0,
            what: "copies that do not settle",
        })
    }

    /// Whether some lane can satisfy all of `conditions` ([`Bools::meet`]).
    ///
    /// [`Bools::meet`]: crate::structure::Bools::meet
    pub(super) fn meet(&self, lanes: &Structured, conditions: &[(Bool, bool)]) -> bool {
        lanes
            .bools
            .meet_seen(conditions, &mut self.meets.borrow_mut())
    }

    pub(super) fn inst(&self, (b, i): Site) -> &'a Inst {
        &self.p.blocks[b].insts[i]
    }

    pub(super) fn def(&self, v: Value) -> Option<&'a Inst> {
        self.defs[v.0 as usize].map(|s| self.inst(s))
    }

    pub(super) fn origin(&self, v: Value) -> Term {
        self.origin[v.0 as usize]
    }

    /// The origin of a one-dword operand as `inst` reads it.
    pub(super) fn term(&self, inst: &Inst, op: &Operand) -> Option<Term> {
        match *op {
            Operand::Scalar { code: NULL, .. } => Some(Term::Const(0)),
            Operand::Scalar { code, .. } => Some(self.origin(inst.read(Reg::S(code))?)),
            Operand::Const(c) => inline(c).map(Term::Const),
            Operand::Literal(x) => Some(Term::Const(x)),
            _ => None,
        }
    }

    /// Layout index and dword of the root parameter `reg` holds on entry.
    pub(super) fn param_of(&self, reg: Reg) -> Option<(usize, u32)> {
        let Reg::S(code) = reg else { return None };
        self.layout.iter().enumerate().find_map(|(i, p)| {
            let first = self.stage.user_sgpr(p.user_data);
            let dword = u32::from(code).checked_sub(first.into())?;
            (dword < p.sgprs).then_some((i, dword))
        })
    }

    /// The SOP2 operands of `v` if an instruction with one of `names` wrote it.
    pub(super) fn sop2(&self, v: Value, names: &[&str]) -> Option<(&'a Inst, [Term; 2])> {
        let inst = self.def(v)?;
        if !names.contains(&inst.name()) || inst.writes.first()?.1 != v {
            return None;
        }
        let Fields::Sop { src, .. } = &inst.fields else {
            return None;
        };
        Some((
            inst,
            [
                self.term(inst, src.first()?)?,
                self.term(inst, src.get(1)?)?,
            ],
        ))
    }

    pub(super) fn root_indexed_view(
        &self,
        descriptor: &Descriptor,
        scalar: &[(Site, Address)],
    ) -> Option<RootIndexedView> {
        if descriptor.role != Role::Resource {
            return None;
        }
        let Source::Loaded { load, first } = descriptor.source else {
            return None;
        };
        let i = scalar.binary_search_by_key(&load, |(site, _)| *site).ok()?;
        let address = scalar[i].1;
        let base = self.layout.get(address.param)?;
        if base.root != RootType::Table {
            return None;
        }
        let dynamic = address.dynamic?;
        let (input, shift) = match self.sop2(dynamic, &["s_lshl_b32"]) {
            Some((_, [Term::Value(input), Term::Const(shift)])) => (input, (shift & 31) as u8),
            _ => (dynamic, 0),
        };
        let (param, constant_dword) = self.param_of(self.inputs[input.0 as usize]?)?;
        let constant = self.layout.get(param)?;
        if constant.root != RootType::Constants {
            return None;
        }
        Some(RootIndexedView {
            table_slot: base.slot,
            constant_slot: constant.slot,
            constant_dword,
            byte_offset: address.offset.wrapping_add(u32::from(first) * 4),
            shift,
        })
    }

    pub(super) fn sum(&self, t: Term) -> Sum {
        let v = match t {
            Term::Const(c) => {
                return Sum {
                    base: None,
                    offset: c,
                    dynamic: None,
                };
            }
            Term::Value(v) => v,
        };
        let dynamic = Sum {
            base: None,
            offset: 0,
            dynamic: Some(v),
        };
        if let Some(reg) = self.inputs[v.0 as usize] {
            return match self.param_of(reg) {
                Some((_, 0)) => Sum {
                    base: Some(v),
                    offset: 0,
                    dynamic: None,
                },
                _ => dynamic,
            };
        }
        if self.getpc(v).is_some() {
            return Sum {
                base: Some(v),
                offset: 0,
                dynamic: None,
            };
        }
        // Paths that each add an index to a table in the code and meet before one
        // load: the join itself is the computed term. Lowering gives every
        // `s_getpc_b64` its own offset in the code as its value, so the join is the
        // byte in the code whichever table a path took ([`Self::address`] adds no
        // base to it).
        if let Some(pc) = self.code_join(v) {
            return Sum {
                base: Some(pc),
                offset: 0,
                dynamic: Some(v),
            };
        }
        // An address computed in VGPRs and taken from the first lane (a
        // table in the code indexed by a value held per lane, or a table
        // offset held per lane, the console's waterfall): the sum of what
        // the lanes hold. Loads at one offset in different loops (each with
        // its own `v_readfirstlane_b32`) then have one address, which is
        // what tells lowering that they read one descriptor. With
        // `read_first_lane` a first-lane read of an index alone stays the
        // computed term ([`Self::first_lane_offset`]).
        if let Some(lane) = self.first_lane(v) {
            let per_lane = self.sum(self.vorigin(lane));
            return if per_lane.base.is_some() || !self.read_first_lane.get() {
                per_lane
            } else {
                dynamic
            };
        }
        let Some([a, b]) = self
            .sop2(v, &["s_add_u32", "s_add_i32"])
            .map(|(_, t)| t)
            .or_else(|| self.vadd(v))
        else {
            return dynamic;
        };
        let (x, y) = (self.sum(a), self.sum(b));
        if x.base.is_some() && y.base.is_some() || x.dynamic.is_some() && y.dynamic.is_some() {
            return dynamic;
        }
        Sum {
            base: x.base.or(y.base),
            offset: x.offset.wrapping_add(y.offset),
            dynamic: x.dynamic.or(y.dynamic),
        }
    }

    /// For a join `v` whose every incoming value adds to an `s_getpc_b64` low
    /// dword: the first of those (one level of joins only). The paths may add
    /// to different ones: two tables in the code, one picked by control flow
    /// (the title's fog reconstruction reads its sample offsets from
    /// one of two tables, by the sample's number; refused, the lit frame was
    /// multiplied by a fog transmittance nothing had written).
    pub(super) fn code_join(&self, v: Value) -> Option<Value> {
        let (b, i) = self.phis[v.0 as usize]?;
        let mut base = None;
        for &a in &self.p.blocks[b].phis[i].args {
            let Term::Value(x) = self.vorigin(a) else {
                return None;
            };
            if x == v {
                continue;
            }
            let pc = self.code_base(x, 4)?;
            base = base.or(Some(pc));
        }
        base
    }

    /// `t` is the high dword of an `s_getpc_b64`, or `s_addc_u32`s of one:
    /// the carry may be added in steps (the counter's high dword plus 0
    /// where the table's offset is added, then plus the index's high dword).
    pub(super) fn code_high(&self, t: Term) -> bool {
        self.code_high_in(t, 4)
    }

    /// [`Self::code_high`], at most `depth` additions deep.
    pub(super) fn code_high_in(&self, t: Term, depth: u32) -> bool {
        let Term::Value(x) = t else { return false };
        if self.def(x).is_some_and(|i| {
            i.name() == "s_getpc_b64" && i.writes.get(1).is_some_and(|w| w.1 == x)
        }) {
            return true;
        }
        depth > 0
            && self.sop2(x, &["s_addc_u32"]).is_some_and(|(_, [p, q])| {
                self.code_high_in(p, depth - 1) || self.code_high_in(q, depth - 1)
            })
    }

    /// The `s_getpc_b64` low dword among the terms `v` adds up (`depth`
    /// additions deep; joins are not followed).
    pub(super) fn code_base(&self, v: Value, depth: u32) -> Option<Value> {
        if self.getpc(v).is_some() {
            return Some(v);
        }
        // Computed in VGPRs and taken from the first lane.
        if let Some(lane) = self.first_lane(v) {
            return match self.vorigin(lane) {
                Term::Value(x) => self.code_base(x, depth),
                Term::Const(_) => None,
            };
        }
        if depth == 0 || self.phis[v.0 as usize].is_some() {
            return None;
        }
        let terms = self
            .sop2(v, &["s_add_u32", "s_add_i32"])
            .map(|(_, t)| t)
            .or_else(|| self.vadd(v))?;
        terms.into_iter().find_map(|t| match t {
            Term::Value(x) => self.code_base(x, depth - 1),
            Term::Const(_) => None,
        })
    }

    /// `t` is `high` or an `s_addc_u32` of it.
    pub(super) fn is_high(&self, t: Term, high: Value) -> bool {
        match t {
            Term::Value(x) => {
                x == high
                    || self
                        .sop2(x, &["s_addc_u32"])
                        .is_some_and(|(_, [p, q])| p == Term::Value(high) || q == Term::Value(high))
            }
            Term::Const(_) => false,
        }
    }

    /// The VGPR value `v_readfirstlane_b32` reads to write the scalar `v`.
    pub(super) fn first_lane(&self, v: Value) -> Option<Value> {
        let inst = self.def(v)?;
        if inst.name() != "v_readfirstlane_b32" || inst.writes.first()?.1 != v {
            return None;
        }
        let Fields::Valu(f) = &inst.fields else {
            return None;
        };
        if f.abs != 0 || f.neg != 0 || f.sdwa.is_some() || f.dpp.is_some() {
            return None;
        }
        match *f.src.first()? {
            Operand::Vector { reg, .. } => inst.read(Reg::V(reg)),
            _ => None,
        }
    }

    /// The `s_getpc_b64` writing `lo` as its low dword.
    pub(super) fn getpc(&self, lo: Value) -> Option<&'a Inst> {
        let inst = self.def(lo)?;
        (inst.name() == "s_getpc_b64" && inst.writes.first().is_some_and(|w| w.1 == lo))
            .then_some(inst)
    }

    /// The operands of a VALU addition (`v_add_co_u32`, `v_add_nc_u32`)
    /// writing `v`, through VGPR copies of scalars.
    pub(super) fn vadd(&self, v: Value) -> Option<[Term; 2]> {
        let inst = self.def(v)?;
        let Fields::Valu(f) = &inst.fields else {
            return None;
        };
        if !matches!(inst.name(), "v_add_co_u32" | "v_add_nc_u32")
            || f.dst.is_none_or(|d| !matches!(d, Operand::Vector { .. }))
            || inst.write(Reg::V(match f.dst? {
                Operand::Vector { reg, .. } => reg,
                _ => return None,
            })) != Some(v)
            || f.abs != 0
            || f.neg != 0
            || f.sdwa.is_some()
            || f.dpp.is_some()
        {
            return None;
        }
        Some([
            self.vterm(inst, f.src.first()?)?,
            self.vterm(inst, f.src.get(1)?)?,
        ])
    }

    /// The origin of a VALU operand.
    pub(super) fn vterm(&self, inst: &Inst, op: &Operand) -> Option<Term> {
        match *op {
            Operand::Vector { reg, .. } => Some(self.vorigin(inst.read(Reg::V(reg))?)),
            op => self.term(inst, &op),
        }
    }

    /// A VGPR value's origin through `v_mov_b32` copies.
    pub(super) fn vorigin(&self, v: Value) -> Term {
        if let Some(inst) = self.def(v)
            && inst.name() == "v_mov_b32"
            && let Fields::Valu(f) = &inst.fields
            && f.abs == 0
            && f.neg == 0
            && f.sdwa.is_none()
            && f.dpp.is_none()
            && let Some(t) = f.src.first().and_then(|s| self.vterm(inst, s))
        {
            return t;
        }
        self.origin(v)
    }

    /// The address of an access that traces to no root parameter (`why`
    /// says how it failed): title memory at the address as it stands, in a
    /// compute program.
    pub(super) fn by_address(&self, why: &'static str) -> Result<Address, &'static str> {
        if self.stage == HwStage::Cs && RAW.contains(&why) {
            Ok(Address {
                param: MEMORY,
                offset: 0,
                dynamic: None,
            })
        } else {
            Err(why)
        }
    }
}
