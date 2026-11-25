use crate::ShaderError;
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Exit, Inst, Program, Reg, Value};
use crate::native::{HwStage, RootParam, RootType};
use crate::structure::{Bool, LaneExit, Meets, Structured};

pub type Site = (usize, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub param: usize,
    pub offset: u32,
    pub dynamic: Option<Value>,
}

pub const CODE: usize = usize::MAX;

pub const MEMORY: usize = usize::MAX - 1;

const RAW: [&str; 5] = [
    "an address not from a root parameter",
    "an address from a root parameter that is no address",
    "an address in an SGPR that is no root parameter",
    "an address loaded from memory",
    "an address from a phi",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Loaded {
        load: Site,
        first: u8,
    },
    Built {
        address: Address,
        bits: u32,
        words: [u32; 2],
    },
    Constant(Vec<u32>),
    Phi {
        block: usize,
        args: Vec<Source>,
    },
    Select {
        scc: Value,
        args: Vec<Source>,
    },
    Fetched {
        load: Site,
        first: u8,
    },
    Lanes {
        mask: Value,
        args: Vec<Source>,
    },
    LanePhi {
        block: usize,
        args: Vec<Option<Source>>,
    },
}

impl Source {
    pub fn leaves(&self) -> Vec<&Source> {
        match self {
            Source::Phi { args, .. } | Source::Select { args, .. } | Source::Lanes { args, .. } => {
                args.iter().flat_map(Source::leaves).collect()
            }
            Source::LanePhi { args, .. } => {
                args.iter().flatten().flat_map(Source::leaves).collect()
            }
            s => vec![s],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Resource,
    Sampler,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub site: Site,
    pub role: Role,
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootIndexedView {
    pub table_slot: u8,
    pub constant_slot: u8,
    pub constant_dword: u32,
    pub byte_offset: u32,
    pub shift: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    pub scalar: Vec<(Site, Address)>,
    pub descriptors: Vec<Descriptor>,
    pub root_indexed_views: Vec<RootIndexedView>,
    pub params: Vec<(Value, usize, u32)>,
    pub lane_copied: Vec<Site>,
    pub waterfall: Waterfall,
    pub bvh: Vec<(Site, BvhNodes)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BvhNodes {
    Resource,
    Memory { word1: u32, word3: u32 },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Waterfall {
    pub compares: Vec<Site>,
    pub reads: Vec<Value>,
    pub loads: Vec<Site>,
}

pub fn track(
    p: &Program,
    layout: &[RootParam],
    stage: HwStage,
    lanes: Option<&Structured>,
) -> Result<Resources, ShaderError> {
    let t = Tracer::new(p, layout, stage, lanes)?;
    let mut out = Resources {
        params: p
            .inputs
            .iter()
            .filter_map(|&(reg, v)| {
                let (param, dword) = t.param_of(reg)?;
                Some((v, param, dword))
            })
            .collect(),
        ..Resources::default()
    };
    let mut first_lane_at = std::collections::BTreeMap::<usize, (Vec<usize>, Vec<Value>)>::new();
    let mut offset_at =
        std::collections::BTreeMap::<usize, (Vec<usize>, Vec<Value>, Vec<Site>)>::new();
    for (b, block) in p.blocks.iter().enumerate() {
        t.at.set(Some(b));
        for (i, inst) in block.insts.iter().enumerate() {
            let site = (b, i);
            let fail = |what| ShaderError::Unsupported {
                offset: inst.machine.offset,
                what,
            };
            let mut first_lane = Vec::new();
            let mut offset = Vec::new();
            let mut descriptor = |op: &Operand, role| -> Result<(), ShaderError> {
                let source = t.descriptor(inst, op, b).map_err(fail)?;
                if !t.kept_per_lane(inst, op) {
                    first_lane.extend(t.first_lane_words(inst, op).unwrap_or_default());
                }
                if let Source::Loaded { load, .. } = source
                    && out.scalar.iter().any(|s| s.0 == load)
                    && let Some(v) = t.first_lane_offset(load)
                {
                    offset.push((v, load));
                }
                out.descriptors.push(Descriptor { site, role, source });
                Ok(())
            };
            match &inst.fields {
                Fields::Image { rsrc, .. } if inst.name().starts_with("image_bvh") => {
                    let word = |k: u8| match *rsrc {
                        Operand::Scalar { code, dwords } if k < dwords => {
                            inst.read(Reg::S(code + k)).map(|v| t.origin(v))
                        }
                        _ => None,
                    };
                    match (word(1), word(3)) {
                        (Some(Term::Const(word1)), Some(Term::Const(word3)))
                            if word3 >> 28 == crate::bvh::DESCRIPTOR_TYPE =>
                        {
                            out.bvh.push((site, BvhNodes::Memory { word1, word3 }));
                        }
                        _ => {
                            descriptor(rsrc, Role::Resource)?;
                            out.bvh.push((site, BvhNodes::Resource));
                        }
                    }
                }
                Fields::Image { rsrc, sampler, .. } => {
                    descriptor(rsrc, Role::Resource)?;
                    if let Some(s) = sampler {
                        descriptor(s, Role::Sampler)?;
                    }
                }
                Fields::Buffer { rsrc, .. } if !inst.name().ends_with("_inv") => {
                    descriptor(rsrc, Role::Resource)?
                }
                Fields::Buffer { .. } => {}
                Fields::Smem { base, .. } if inst.name().starts_with("s_buffer_") => {
                    descriptor(base, Role::Resource)?;
                }
                Fields::Smem {
                    base,
                    offset,
                    soffset,
                    ..
                } if inst.name().contains("_load_") || inst.name().contains("_store_") => {
                    let address = t
                        .smem_address(inst, base, *offset, soffset)
                        .or_else(|why| t.by_address(why))
                        .map_err(fail)?;
                    out.scalar.push((site, address));
                }
                Fields::Flat {
                    addr,
                    saddr,
                    offset,
                    ..
                } if inst.name().starts_with("global_") => {
                    let address = t
                        .flat_address(inst, addr.as_ref(), saddr.as_ref(), *offset)
                        .or_else(|why| t.by_address(why))
                        .map_err(fail)?;
                    out.scalar.push((site, address));
                }
                _ => {}
            }
            if !first_lane.is_empty() {
                let (accesses, reads) = first_lane_at.entry(b).or_default();
                accesses.push(i);
                reads.extend(first_lane);
            }
            if !offset.is_empty() {
                let (accesses, reads, loads) = offset_at.entry(b).or_default();
                accesses.push(i);
                for (v, load) in offset {
                    reads.push(v);
                    loads.push(load);
                }
            }
        }
    }
    t.at.set(None);
    if t.lanes.is_none() {
        return Ok(out);
    }
    for (block, (accesses, mut reads)) in first_lane_at {
        reads.sort_unstable();
        reads.dedup();
        let proof =
            t.waterfall(block, &accesses, &reads)
                .map_err(|what| ShaderError::Unsupported {
                    offset: t.inst((block, accesses[0])).machine.offset,
                    what,
                })?;
        out.waterfall.compares.extend(proof.compares);
        out.waterfall.reads.extend(proof.reads);
    }
    for (block, (accesses, mut reads, loads)) in offset_at {
        reads.sort_unstable();
        reads.dedup();
        if let Ok(proof) = t.waterfall(block, &accesses, &reads) {
            out.waterfall.compares.extend(proof.compares);
            out.waterfall.reads.extend(proof.reads);
            out.waterfall.loads.extend(loads);
        }
    }
    out.lane_copied = t.lane_copied.take();
    out.lane_copied.sort_unstable();
    out.lane_copied.dedup();
    out.root_indexed_views = out
        .descriptors
        .iter()
        .filter_map(|d| t.root_indexed_view(d, &out.scalar))
        .collect();
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Term {
    Const(u32),
    Value(Value),
}

struct Sum {
    base: Option<Value>,
    offset: u32,
    dynamic: Option<Value>,
}

struct Tracer<'a> {
    p: &'a Program,
    layout: &'a [RootParam],
    stage: HwStage,
    defs: Vec<Option<Site>>,
    phis: Vec<Option<(usize, usize)>>,
    inputs: Vec<Option<Reg>>,
    origin: Vec<Term>,
    lane_copied: std::cell::RefCell<Vec<Site>>,
    lanes: Option<&'a Structured>,
    read_first_lane: std::cell::Cell<bool>,
    meets: std::cell::RefCell<Meets>,
    rpo: Vec<usize>,
    at: std::cell::Cell<Option<usize>>,
}

type Known = Vec<(Bool, bool)>;

const MAX_ROUNDS: usize = 64;
const MAX_JOINS: usize = 8;
const MAX_COPIES: usize = 64;

impl<'a> Tracer<'a> {
    fn new(
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

    fn propagate_copies(&self, n: usize) -> Result<Vec<Term>, ShaderError> {
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
                        let new = match copy_source(inst, k).or_else(|| self.restored(inst)) {
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

    fn meet(&self, lanes: &Structured, conditions: &[(Bool, bool)]) -> bool {
        lanes
            .bools
            .meet_seen(conditions, &mut self.meets.borrow_mut())
    }

    fn inst(&self, (b, i): Site) -> &'a Inst {
        &self.p.blocks[b].insts[i]
    }

    fn def(&self, v: Value) -> Option<&'a Inst> {
        self.defs[v.0 as usize].map(|s| self.inst(s))
    }

    fn origin(&self, v: Value) -> Term {
        self.origin[v.0 as usize]
    }

    fn term(&self, inst: &Inst, op: &Operand) -> Option<Term> {
        match *op {
            Operand::Scalar { code: NULL, .. } => Some(Term::Const(0)),
            Operand::Scalar { code, .. } => Some(self.origin(inst.read(Reg::S(code))?)),
            Operand::Const(c) => inline(c).map(Term::Const),
            Operand::Literal(x) => Some(Term::Const(x)),
            _ => None,
        }
    }

    fn param_of(&self, reg: Reg) -> Option<(usize, u32)> {
        let Reg::S(code) = reg else { return None };
        self.layout.iter().enumerate().find_map(|(i, p)| {
            let first = self.stage.user_sgpr(p.user_data);
            let dword = u32::from(code).checked_sub(first.into())?;
            (dword < p.sgprs).then_some((i, dword))
        })
    }

    fn sop2(&self, v: Value, names: &[&str]) -> Option<(&'a Inst, [Term; 2])> {
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

    fn root_indexed_view(
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

    fn sum(&self, t: Term) -> Sum {
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
        if let Some(pc) = self.code_join(v) {
            return Sum {
                base: Some(pc),
                offset: 0,
                dynamic: Some(v),
            };
        }
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

    fn code_join(&self, v: Value) -> Option<Value> {
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

    fn code_high(&self, t: Term) -> bool {
        self.code_high_in(t, 4)
    }

    fn code_high_in(&self, t: Term, depth: u32) -> bool {
        let Term::Value(x) = t else { return false };
        if self
            .def(x)
            .is_some_and(|i| i.name() == "s_getpc_b64" && i.writes.get(1).is_some_and(|w| w.1 == x))
        {
            return true;
        }
        depth > 0
            && self.sop2(x, &["s_addc_u32"]).is_some_and(|(_, [p, q])| {
                self.code_high_in(p, depth - 1) || self.code_high_in(q, depth - 1)
            })
    }

    fn code_base(&self, v: Value, depth: u32) -> Option<Value> {
        if self.getpc(v).is_some() {
            return Some(v);
        }
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

    fn is_high(&self, t: Term, high: Value) -> bool {
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

    fn first_lane(&self, v: Value) -> Option<Value> {
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

    fn getpc(&self, lo: Value) -> Option<&'a Inst> {
        let inst = self.def(lo)?;
        (inst.name() == "s_getpc_b64" && inst.writes.first().is_some_and(|w| w.1 == lo))
            .then_some(inst)
    }

    fn vadd(&self, v: Value) -> Option<[Term; 2]> {
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

    fn vterm(&self, inst: &Inst, op: &Operand) -> Option<Term> {
        match *op {
            Operand::Vector { reg, .. } => Some(self.vorigin(inst.read(Reg::V(reg))?)),
            op => self.term(inst, &op),
        }
    }

    fn vorigin(&self, v: Value) -> Term {
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

    fn by_address(&self, why: &'static str) -> Result<Address, &'static str> {
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

    fn flat_address(
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

    fn address(&self, lo: Term, hi: Term, extra: Sum) -> Result<Address, &'static str> {
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
            let high = pc.writes.get(1).map(|w| w.1);
            if !self.high_half(hi, high) && !high.is_some_and(|h| self.carried(hi, h)) {
                return Err("an address with an untracked high half");
            }
            let joined = s.dynamic.is_some_and(|d| self.code_join(d).is_some());
            let first = if joined {
                0
            } else {
                pc.machine.offset as u32 + 4
            };
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

    fn high_half(&self, hi: Term, high: Option<Value>) -> bool {
        let want = high.map_or(Term::Const(0), Term::Value);
        hi == want
            || matches!(hi, Term::Value(v) if self.sop2(v, &["s_addc_u32"]).is_some_and(
                |(_, [a, b])| a == want && b == Term::Const(0) || b == want && a == Term::Const(0)
            ))
    }

    fn carried(&self, hi: Term, high: Value) -> bool {
        let Term::Value(v) = hi else { return false };
        if v == high {
            return true;
        }
        if let Some((b, i)) = self.phis[v.0 as usize] {
            let high_of = |t: Term| self.is_high(t, high) || self.code_high(t);
            return self.p.blocks[b].phis[i].args.iter().all(|&a| {
                let t = self.vorigin(a);
                if t == Term::Value(v) || high_of(t) {
                    return true;
                }
                let Term::Value(x) = t else {
                    return false;
                };
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

    fn why_no_base(&self, dynamic: Option<Value>) -> &'static str {
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

    fn leaves(&self, v: Value) -> Box<dyn Iterator<Item = Value> + '_> {
        match self.sop2(v, &["s_add_u32", "s_add_i32"]) {
            Some((_, terms)) => Box::new(terms.into_iter().flat_map(move |t| match t {
                Term::Value(x) => self.leaves(x),
                Term::Const(_) => Box::new(std::iter::empty()),
            })),
            None => Box::new(std::iter::once(v)),
        }
    }

    fn smem_address(
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

    fn descriptor(&self, inst: &Inst, op: &Operand, at: usize) -> Result<Source, &'static str> {
        let Operand::Scalar { code, dwords } = *op else {
            return Err("a descriptor not in SGPRs");
        };
        let words = (0..dwords)
            .map(|k| inst.read(Reg::S(code + k)).map(|v| self.origin(v)))
            .collect::<Option<Vec<_>>>()
            .ok_or("a descriptor not in SGPRs")?;
        let mut known = Vec::from_iter(self.lanes.map(|l| (l.reach[at], true)));
        self.classify(&words, &mut Vec::new(), &mut known)
    }

    fn classify(
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
            if let Some(source) = self.lane_copies(words) {
                return Ok(source);
            }
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

    fn only_edge(&self, block: usize, known: &mut Known) -> Option<usize> {
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

    fn anywhere(&self, source: &Source) -> bool {
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

    fn live(&self, mut t: Term) -> Term {
        let (Some(at), Some(lanes)) = (self.at.get(), self.lanes) else {
            return t;
        };
        let mut known = vec![(lanes.reach[at], true)];
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

    fn phi(
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

    fn select(
        &self,
        words: &[Term],
        joins: &mut Vec<usize>,
        known: &mut Known,
    ) -> Result<Option<Source>, &'static str> {
        let picks: Vec<_> = words
            .iter()
            .map(|w| match *w {
                Term::Value(v) => self.picked(v),
                Term::Const(_) => None,
            })
            .collect();
        let Some(scc) = picks.iter().flatten().map(|p| p.0).next() else {
            return Ok(None);
        };
        if picks.iter().flatten().any(|p| p.0 != scc) {
            return Err("a descriptor chosen by several conditions");
        }
        if joins.len() >= MAX_JOINS {
            return Err("a descriptor joined too often");
        }
        joins.push(usize::MAX);
        let side = |set: bool| -> Vec<Term> {
            words
                .iter()
                .zip(&picks)
                .map(|(w, p)| match p {
                    Some((_, t, f)) => {
                        if set {
                            *t
                        } else {
                            *f
                        }
                    }
                    None => *w,
                })
                .collect()
        };
        let args = [true, false]
            .into_iter()
            .map(|set| self.classify(&side(set), joins, known))
            .collect::<Result<_, _>>();
        joins.pop();
        Ok(Some(Source::Select { scc, args: args? }))
    }

    fn picked(&self, v: Value) -> Option<(Value, Term, Term)> {
        let inst = self.def(v)?;
        let Fields::Sop { dst, src, .. } = &inst.fields else {
            return None;
        };
        let k = inst.writes.iter().position(|w| w.1 == v)? as u8;
        let wide = match inst.name() {
            "s_cselect_b32" | "s_cmov_b32" => false,
            "s_cselect_b64" | "s_cmov_b64" => true,
            _ => return None,
        };
        let dword = |op: &Operand| match *op {
            Operand::Scalar { code: NULL, .. } => Some(Term::Const(0)),
            Operand::Scalar { code, .. } => Some(self.origin(inst.read(Reg::S(code + k))?)),
            Operand::Const(c) if !wide => inline(c).map(Term::Const),
            Operand::Const(c @ 128..=208) => {
                let low = inline(c)?;
                Some(Term::Const(if k == 0 {
                    low
                } else {
                    ((low as i32) >> 31) as u32
                }))
            }
            Operand::Literal(x) if !wide => Some(Term::Const(x)),
            _ => None,
        };
        let set = dword(src.first()?)?;
        let clear = if inst.name().starts_with("s_cmov") {
            dword(dst.as_ref()?)?
        } else {
            dword(src.get(1)?)?
        };
        Some((inst.read(Reg::Scc)?, set, clear))
    }

    fn set_bits(&self, v: Value) -> Option<(Term, u32)> {
        if let Some((_, [x, Term::Const(bits)] | [Term::Const(bits), x])) =
            self.sop2(v, &["s_or_b32"])
        {
            return Some((x, bits));
        }
        let inst = self.def(v)?;
        let Fields::Sop {
            dst: Some(Operand::Scalar { code, .. }),
            src,
            ..
        } = &inst.fields
        else {
            return None;
        };
        match (inst.name(), self.term(inst, src.first()?)?) {
            ("s_bitset1_b32", Term::Const(bit)) => {
                Some((self.origin(inst.read(Reg::S(*code))?), 1 << (bit & 31)))
            }
            _ => None,
        }
    }

    fn first_lane_source(&self, v: Value) -> Option<Value> {
        let inst = self.def(v)?;
        let Fields::Valu(f) = &inst.fields else {
            return None;
        };
        if inst.name() != "v_readfirstlane_b32" || f.sdwa.is_some() || f.dpp.is_some() {
            return None;
        }
        let Some(&Operand::Vector { reg, .. }) = f.src.first() else {
            return None;
        };
        inst.read(Reg::V(reg))
    }

    fn first_lane_offset(&self, load: Site) -> Option<Value> {
        let inst = self.inst(load);
        let Fields::Smem {
            base,
            offset,
            soffset,
            ..
        } = &inst.fields
        else {
            return None;
        };
        self.read_first_lane.set(true);
        let address = self.smem_address(inst, base, *offset, soffset);
        self.read_first_lane.set(false);
        match self.origin(address.ok()?.dynamic?) {
            Term::Value(v) if self.first_lane_source(v).is_some() => Some(v),
            _ => None,
        }
    }

    fn kept_per_lane(&self, inst: &Inst, op: &Operand) -> bool {
        let Operand::Scalar { code, dwords } = *op else {
            return false;
        };
        (0..dwords)
            .map(|k| inst.read(Reg::S(code + k)).map(|v| self.origin(v)))
            .collect::<Option<Vec<_>>>()
            .is_some_and(|words| self.lane_copies(&words).is_some())
    }

    fn first_lane_words(&self, inst: &Inst, op: &Operand) -> Option<Vec<Value>> {
        let Operand::Scalar { code, dwords } = *op else {
            return None;
        };
        (0..dwords)
            .map(|k| match self.origin(inst.read(Reg::S(code + k))?) {
                Term::Value(v) => self.first_lane_source(v).map(|_| v),
                Term::Const(_) => None,
            })
            .collect()
    }

    fn lane_term(&self, mut t: Term) -> Term {
        for _ in 0..MAX_JOINS * 2 {
            let Term::Value(v) = t else { break };
            let next = self.vorigin(v);
            if next == t {
                break;
            }
            t = next;
        }
        t
    }

    fn varies(&self, t: Term) -> bool {
        let Term::Value(v) = t else { return false };
        let reg = match (self.inputs[v.0 as usize], self.phis[v.0 as usize]) {
            (Some(reg), _) => Some(reg),
            (None, Some((b, i))) => Some(self.p.blocks[b].phis[i].reg),
            (None, None) => self
                .def(v)
                .and_then(|i| i.writes.iter().find(|w| w.1 == v))
                .map(|w| w.0),
        };
        matches!(reg, Some(Reg::V(_)))
    }

    fn lane_value(&self, mut t: Term, known: &Known) -> Result<Term, &'static str> {
        for _ in 0..MAX_JOINS * 2 {
            let Term::Value(v) = t else { break };
            let next = self.vstep(v);
            if next == t {
                break;
            }
            if self.varies(t)
                && let Some(site) = self.defs[v.0 as usize]
            {
                self.runs(site, known)?;
            }
            t = next;
        }
        Ok(t)
    }

    fn vstep(&self, v: Value) -> Term {
        if let Some(inst) = self.def(v)
            && inst.name() == "v_mov_b32"
            && let Fields::Valu(f) = &inst.fields
            && f.abs == 0
            && f.neg == 0
            && f.sdwa.is_none()
            && f.dpp.is_none()
            && let Some(t) = f.src.first().and_then(|s| self.raw(inst, s))
        {
            return t;
        }
        self.origin(v)
    }

    fn raw(&self, inst: &Inst, op: &Operand) -> Option<Term> {
        match *op {
            Operand::Vector { reg, .. } => Some(Term::Value(inst.read(Reg::V(reg))?)),
            op => self.term(inst, &op),
        }
    }

    fn runs(&self, site: Site, known: &Known) -> Result<(), &'static str> {
        let Some(lanes) = self.lanes else {
            return Ok(());
        };
        let Some(&Some(guard)) = lanes.guards.get(site.0).and_then(|g| g.get(site.1)) else {
            return Ok(());
        };
        let mut skips = known.clone();
        skips.extend([(lanes.reach[site.0], true), (guard, false)]);
        if self.meet(lanes, &skips) {
            return Err("a per-lane descriptor written where some lanes may keep the old one");
        }
        Ok(())
    }

    fn per_lane(
        &self,
        words: &[Term],
        joins: &mut Vec<usize>,
        known: &mut Known,
    ) -> Result<Source, &'static str> {
        let words = words
            .iter()
            .map(|&w| self.lane_value(w, known))
            .collect::<Result<Vec<_>, _>>()?;
        let words = &words[..];
        let Some(&lane) = words.iter().find(|&&w| self.varies(w)) else {
            return self.classify(words, joins, known);
        };
        if let Some(source) = self.fetched(words, known)? {
            return Ok(source);
        }
        if let Some(source) = self.lane_select(words, joins, known)? {
            return Ok(source);
        }
        if let Some(source) = self.lane_phi(words, joins, known)? {
            return Ok(source);
        }
        let Term::Value(v) = lane else { unreachable!() };
        let name = self.def(v).map(|i| i.name());
        Err(match name {
            _ if self.inputs[v.0 as usize].is_some() => {
                "a descriptor read from the first lane of an input VGPR"
            }
            None => "a descriptor read from the first lane of a phi",
            Some(n) if n.starts_with("ds_read") => {
                "a descriptor read from the first lane of an LDS read"
            }
            Some(n) if n.starts_with("buffer_load") => {
                "a descriptor read from the first lane of a buffer load"
            }
            Some(n) if n.starts_with("global_load") || n.starts_with("flat_load") => {
                "a descriptor read from the first lane of a partial or mixed load"
            }
            Some(_) => "a descriptor read from the first lane of computed VGPRs",
        })
    }

    fn fetched(&self, words: &[Term], known: &Known) -> Result<Option<Source>, &'static str> {
        let at = |w: &Term| -> Option<(Site, usize)> {
            let Term::Value(v) = *w else { return None };
            let site = self.defs[v.0 as usize]?;
            let inst = self.inst(site);
            if !inst.name().starts_with("global_load_dword") {
                return None;
            }
            let k = inst
                .writes
                .iter()
                .filter(|w| matches!(w.0, Reg::V(_)))
                .position(|w| w.1 == v)?;
            Some((site, k))
        };
        let Some(sites) = words.iter().map(at).collect::<Option<Vec<_>>>() else {
            return Ok(None);
        };
        let address = |site: Site| {
            let inst = self.inst(site);
            let Fields::Flat {
                addr,
                saddr,
                offset,
                ..
            } = &inst.fields
            else {
                return Err("a global load");
            };
            self.flat_address(inst, addr.as_ref(), saddr.as_ref(), *offset)
        };
        let (load, first) = sites[0];
        let a0 = address(load)?;
        for (k, &(site, dword)) in sites.iter().enumerate() {
            self.runs(site, known)?;
            let a = address(site)?;
            let want = a0.offset.wrapping_add(4 * (first + k) as u32);
            if (a.param, a.dynamic) != (a0.param, a0.dynamic)
                || a.offset.wrapping_add(4 * dword as u32) != want
            {
                return Err("a descriptor read from the first lane of loads that do not line up");
            }
        }
        Ok(Some(Source::Fetched {
            load,
            first: first as u8,
        }))
    }

    fn lane_select(
        &self,
        words: &[Term],
        joins: &mut Vec<usize>,
        known: &mut Known,
    ) -> Result<Option<Source>, &'static str> {
        let picks: Vec<_> = words
            .iter()
            .map(|w| match *w {
                Term::Value(v) => self.lane_picked(v),
                Term::Const(_) => None,
            })
            .collect();
        let Some(mask) = picks.iter().flatten().map(|p| p.0).next() else {
            return Ok(None);
        };
        if picks.iter().flatten().any(|p| p.0 != mask) {
            return Err("a descriptor chosen per lane by several masks");
        }
        for (w, p) in words.iter().zip(&picks) {
            if let (Term::Value(v), Some(_)) = (*w, p)
                && let Some(site) = self.defs[v.0 as usize]
            {
                self.runs(site, known)?;
            }
        }
        if joins.len() >= MAX_JOINS {
            return Err("a descriptor joined too often");
        }
        joins.push(usize::MAX);
        let bit = self.lanes.and_then(|l| l.masks.get(&mask).copied());
        let side = |set: bool| -> Vec<Term> {
            words
                .iter()
                .zip(&picks)
                .map(|(w, p)| match p {
                    Some((_, t, f)) => {
                        if set {
                            *t
                        } else {
                            *f
                        }
                    }
                    None => *w,
                })
                .collect()
        };
        let args = [true, false]
            .into_iter()
            .map(|set| {
                let n = known.len();
                known.extend(bit.map(|b| (b, set)));
                let arg = self.per_lane(&side(set), joins, known);
                known.truncate(n);
                arg
            })
            .collect::<Result<_, _>>();
        joins.pop();
        Ok(Some(Source::Lanes { mask, args: args? }))
    }

    fn lane_picked(&self, v: Value) -> Option<(Value, Term, Term)> {
        let inst = self.def(v)?;
        let Fields::Valu(f) = &inst.fields else {
            return None;
        };
        let Some(Operand::Vector { reg, .. }) = f.dst else {
            return None;
        };
        if inst.name() != "v_cndmask_b32"
            || inst.write(Reg::V(reg)) != Some(v)
            || f.abs != 0
            || f.neg != 0
            || f.clamp
            || f.omod != 0
            || f.sdwa.is_some()
            || f.dpp.is_some()
        {
            return None;
        }
        let [clear, set, Operand::Scalar { code, .. }] = f.src[..] else {
            return None;
        };
        let mask = inst.read(Reg::S(code))?;
        Some((mask, self.raw(inst, &set)?, self.raw(inst, &clear)?))
    }

    fn lane_phi(
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
        let preds = &self.p.blocks[block].preds;
        if preds.iter().any(|&q| self.rpo[q] >= self.rpo[block]) {
            return Err("a descriptor read from the first lane of a phi at a loop header");
        }
        let Some(lanes) = self.lanes else {
            return Err("a descriptor read from the first lane of a phi, without the lanes' paths");
        };
        for w in words {
            if let Term::Value(v) = *w
                && phi_of(w).is_none()
                && let Some((d, _)) = self.defs[v.0 as usize]
                && (d == block || self.reaches(block, d))
            {
                return Err("a descriptor partly joined by control flow");
            }
        }
        if joins.len() >= MAX_JOINS {
            return Err("a descriptor joined too often");
        }
        joins.push(block);
        let phis = &self.p.blocks[block].phis;
        let mut args = Vec::new();
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
            let arg = if self.meet(lanes, known) {
                let edge: Vec<Term> = words
                    .iter()
                    .map(|w| match phi_of(w) {
                        Some((_, i)) => Term::Value(phis[i].args[j]),
                        None => *w,
                    })
                    .collect();
                self.per_lane(&edge, joins, known).map(Some)
            } else {
                Ok(None)
            };
            known.truncate(n);
            match arg {
                Ok(a) => args.push(a),
                Err(e) => {
                    joins.pop();
                    return Err(e);
                }
            }
        }
        joins.pop();
        Ok(Some(Source::LanePhi { block, args }))
    }

    fn reaches(&self, from: usize, to: usize) -> bool {
        let mut seen = vec![false; self.p.blocks.len()];
        let mut stack = self.p.blocks[from].exit.successors();
        while let Some(x) = stack.pop() {
            if x == to {
                return true;
            }
            if std::mem::replace(&mut seen[x], true) {
                continue;
            }
            stack.extend(self.p.blocks[x].exit.successors());
        }
        false
    }

    fn waterfall(
        &self,
        b: usize,
        accesses: &[usize],
        reads: &[Value],
    ) -> Result<Waterfall, &'static str> {
        const LOOP: &str = "a descriptor read from the first lane outside a waterfall loop";
        const SPLIT: &str = "a waterfall loop nested in another or split over blocks";
        const SELECT: &str = "a waterfall loop whose compares do not select the access's lanes";
        const RETRY: &str = "a waterfall loop whose retry does not keep the lanes still waiting";
        let access = accesses[0];
        let block = &self.p.blocks[b];
        let lanes = self.lanes.ok_or(LOOP)?;
        let exit = block.insts.last().ok_or(LOOP)?;
        if !matches!(block.exit, Exit::Branch { taken, not_taken } if taken == b && not_taken != b)
            || exit.name() != "s_cbranch_execnz"
        {
            return Err(LOOP);
        }
        let mask = |low: Option<Value>, high: Option<Value>| {
            let high = if self.p.wave64 { Some(high?) } else { None };
            lanes.mask_bits(low?, high)
        };
        let exec = |inst: &Inst| mask(inst.read(Reg::S(EXEC)), inst.read(Reg::S(EXEC + 1)));
        if reads
            .iter()
            .any(|v| self.defs[v.0 as usize].is_none_or(|at| at.0 != b))
        {
            return Err(SPLIT);
        }
        let selected = exec(&block.insts[access]).ok_or(SELECT)?;
        if accesses
            .iter()
            .any(|&i| exec(&block.insts[i]) != Some(selected))
        {
            return Err(SELECT);
        }
        let retry = exec(exit).ok_or(RETRY)?;
        let first = self.defs[reads[0].0 as usize].ok_or(LOOP)?;
        let remaining = exec(self.inst(first)).ok_or(SELECT)?;
        if reads.iter().any(|v| {
            self.defs[v.0 as usize]
                .is_none_or(|at| at.1 >= access || exec(self.inst(at)) != Some(remaining))
        }) {
            return Err(SELECT);
        }
        let reach = (lanes.reach[b], true);
        let meet = |conditions: &[(Bool, bool)]| self.meet(lanes, conditions);
        let mut out = Waterfall::default();
        let mut required = vec![(remaining, true)];
        for (i, inst) in block.insts[..access].iter().enumerate() {
            let Fields::Valu(f) = &inst.fields else {
                continue;
            };
            let Some(ty) = inst
                .name()
                .strip_prefix("v_cmp_eq_")
                .or(inst.name().strip_prefix("v_cmpx_eq_"))
            else {
                continue;
            };
            let dwords = match ty {
                "u32" | "i32" => 1,
                "u64" | "i64" => 2,
                _ => continue,
            };
            if f.abs != 0 || f.neg != 0 || f.sdwa.is_some() || f.dpp.is_some() {
                continue;
            }
            let pair = |first: &Operand, own: &Operand| -> Option<Vec<Value>> {
                let (&Operand::Scalar { code, .. }, &Operand::Vector { reg, .. }) = (first, own)
                else {
                    return None;
                };
                (0..dwords)
                    .map(|k| {
                        let Term::Value(x) = self.origin(inst.read(Reg::S(code + k))?) else {
                            return None;
                        };
                        let lane = inst.read(Reg::V(reg + k))?;
                        let (_, read) = self.defs[x.0 as usize]?;
                        let same = reads.contains(&x)
                            && read < i
                            && self.lane_term(Term::Value(self.first_lane_source(x)?))
                                == self.lane_term(Term::Value(lane));
                        same.then_some(x)
                    })
                    .collect()
            };
            let (Some(a), Some(c)) = (f.src.first(), f.src.get(1)) else {
                continue;
            };
            if let Some(xs) = pair(a, c).or_else(|| pair(c, a)) {
                let code = if inst.name().starts_with("v_cmpx_") {
                    EXEC
                } else if let Some(Operand::Scalar { code, .. }) = f.dst {
                    code
                } else {
                    continue;
                };
                let Some(matched) = mask(inst.write(Reg::S(code)), inst.write(Reg::S(code + 1)))
                else {
                    continue;
                };
                if meet(&[reach, (selected, true), (matched, false)]) {
                    continue;
                }
                required.push((matched, true));
                out.compares.push((b, i));
                for x in xs {
                    if !out.reads.contains(&x) {
                        out.reads.push(x);
                    }
                }
            }
        }
        if reads.iter().any(|v| !out.reads.contains(v))
            || meet(&[reach, (selected, true), (remaining, false)])
        {
            return Err(SELECT);
        }
        required.extend([reach, (selected, false)]);
        if meet(&required) {
            return Err(SELECT);
        }
        if meet(&[reach, (retry, true), (remaining, false)])
            || meet(&[reach, (retry, true), (selected, true)])
            || meet(&[reach, (remaining, true), (selected, false), (retry, false)])
        {
            return Err(RETRY);
        }
        Ok(out)
    }

    fn loaded(&self, words: &[Term]) -> Option<Source> {
        let Term::Value(v0) = words[0] else {
            return None;
        };
        let load = self.defs[v0.0 as usize]?;
        let inst = self.inst(load);
        if !inst.name().starts_with("s_load_") {
            return None;
        }
        let first = inst.writes.iter().position(|w| w.1 == v0)?;
        let same = words.iter().enumerate().all(|(k, w)| {
            inst.writes
                .get(first + k)
                .is_some_and(|x| *w == Term::Value(x.1))
        });
        same.then_some(Source::Loaded {
            load,
            first: first as u8,
        })
    }

    fn lane_copies(&self, words: &[Term]) -> Option<Source> {
        let mut loads: Vec<Site> = Vec::new();
        let mut first = None;
        for (k, w) in words.iter().enumerate() {
            let Term::Value(v) = *w else { return None };
            let mut copies = Vec::new();
            if !self.scalar_copies(self.first_lane(v)?, &mut copies) {
                return None;
            }
            let mut found = false;
            for c in copies {
                let Term::Value(c) = c else { continue };
                let Some(site) = self.defs[c.0 as usize] else {
                    continue;
                };
                let load = self.inst(site);
                if !load.name().starts_with("s_load_") {
                    continue;
                }
                let dword = load.writes.iter().position(|x| x.1 == c)?;
                let f = dword.checked_sub(k)?;
                if *first.get_or_insert(f) != f {
                    return None;
                }
                if !loads.contains(&site) {
                    loads.push(site);
                }
                found = true;
            }
            if !found {
                return None;
            }
        }
        let address = |site: Site| {
            let inst = self.inst(site);
            match &inst.fields {
                Fields::Smem {
                    base,
                    offset,
                    soffset,
                    ..
                } => self.smem_address(inst, base, *offset, soffset).ok(),
                _ => None,
            }
        };
        let a = address(*loads.first()?)?;
        if a.dynamic.is_some() || loads.iter().any(|&l| address(l) != Some(a)) {
            return None;
        }
        loads.sort_unstable();
        let load = *loads.iter().find(|&&l| self.base_from_entry(l))?;
        self.lane_copied.borrow_mut().extend(&loads);
        Some(Source::Loaded {
            load,
            first: first? as u8,
        })
    }

    fn scalar_copies(&self, v: Value, out: &mut Vec<Term>) -> bool {
        let (mut seen, mut work) = (Vec::new(), vec![v]);
        while let Some(v) = work.pop() {
            if seen.contains(&v) {
                continue;
            }
            if seen.len() == MAX_COPIES {
                return false;
            }
            seen.push(v);
            if let Some((b, i)) = self.phis[v.0 as usize] {
                work.extend(&self.p.blocks[b].phis[i].args);
                continue;
            }
            let Some(inst) = self.def(v) else { continue };
            if inst.name().starts_with("global_load_") {
                return false;
            }
            let Fields::Valu(f) = &inst.fields else {
                continue;
            };
            let sources = match inst.name() {
                "v_mov_b32" => 1,
                "v_cndmask_b32" => 2,
                _ => continue,
            };
            if f.abs != 0 || f.neg != 0 || f.sdwa.is_some() || f.dpp.is_some() {
                continue;
            }
            for op in f.src.iter().take(sources) {
                match *op {
                    Operand::Vector { reg, .. } => work.extend(inst.read(Reg::V(reg))),
                    Operand::Scalar { code, .. } if code != NULL => {
                        out.extend(inst.read(Reg::S(code)).map(|s| self.origin(s)));
                    }
                    _ => {}
                }
            }
            if let Some(Operand::Vector { reg, .. }) = f.dst {
                work.extend(inst.read(Reg::V(reg)));
            }
        }
        true
    }

    fn base_from_entry(&self, site: Site) -> bool {
        let inst = self.inst(site);
        let Fields::Smem {
            base: Operand::Scalar { code, .. },
            soffset: Operand::Scalar { code: NULL, .. },
            ..
        } = &inst.fields
        else {
            return false;
        };
        let Some(mut v) = inst.read(Reg::S(*code)) else {
            return false;
        };
        loop {
            if self.inputs[v.0 as usize].is_some() {
                return true;
            }
            let Some(def) = self.def(v) else { return false };
            let Some(k) = def.writes.iter().position(|w| w.1 == v) else {
                return false;
            };
            match copy_source(def, k) {
                Some(Moved::Value(s)) => v = s,
                _ => return false,
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Moved {
    Const(u32),
    Value(Value),
}

impl Tracer<'_> {
    fn restored(&self, inst: &Inst) -> Option<Moved> {
        let Fields::Valu(v) = &inst.fields else {
            return None;
        };
        if inst.name() != "v_readlane_b32" {
            return None;
        }
        let [Operand::Vector { reg, .. }, lane] = v.src[..] else {
            return None;
        };
        self.spilled(inst.read(Reg::V(reg))?, lane_of(lane)?, &mut Vec::new())
            .ok()
            .flatten()
    }

    fn spilled(&self, v: Value, lane: u32, seen: &mut Vec<Value>) -> Result<Option<Moved>, ()> {
        if let Some((b, i)) = self.phis[v.0 as usize] {
            if seen.contains(&v) {
                return Ok(None);
            }
            seen.push(v);
            let mut found = None;
            for &a in &self.p.blocks[b].phis[i].args {
                if let Some(m) = self.spilled(a, lane, seen)? {
                    if found.is_some_and(|f| f != m) {
                        return Err(());
                    }
                    found = Some(m);
                }
            }
            return Ok(found);
        }
        let inst = self.def(v).ok_or(())?;
        let Fields::Valu(w) = &inst.fields else {
            return Err(());
        };
        let (Some(Operand::Vector { reg, .. }), [value, at]) = (w.dst, &w.src[..]) else {
            return Err(());
        };
        if inst.name() != "v_writelane_b32" {
            return Err(());
        }
        if lane_of(*at).ok_or(())? != lane {
            return self.spilled(inst.read(Reg::V(reg)).ok_or(())?, lane, seen);
        }
        match *value {
            Operand::Scalar { code, .. } if code != NULL => {
                inst.read(Reg::S(code)).map(Moved::Value)
            }
            Operand::Literal(x) => Some(Moved::Const(x)),
            Operand::Const(c) => inline(c).map(Moved::Const),
            _ => None,
        }
        .map(Some)
        .ok_or(())
    }
}

fn lane_of(op: Operand) -> Option<u32> {
    match op {
        Operand::Const(c) => inline(c),
        Operand::Literal(x) => Some(x),
        _ => None,
    }
}

fn copy_source(inst: &Inst, k: usize) -> Option<Moved> {
    let Fields::Sop { src, imm, .. } = &inst.fields else {
        return None;
    };
    if inst.name() == "s_movk_i32" {
        return Some(Moved::Const(i32::from((*imm)? as i16) as u32));
    }
    let src = src.first()?;
    match (inst.name(), *src) {
        ("s_mov_b32" | "s_mov_b64", Operand::Scalar { code, .. }) if code != NULL => {
            inst.read(Reg::S(code + k as u8)).map(Moved::Value)
        }
        ("s_mov_b32" | "s_mov_b64", Operand::Scalar { .. }) => Some(Moved::Const(0)),
        ("s_mov_b32", Operand::Literal(x)) => Some(Moved::Const(x)),
        ("s_mov_b32", Operand::Const(c)) => inline(c).map(Moved::Const),
        ("s_brev_b32", Operand::Const(c)) => inline(c).map(|x| Moved::Const(x.reverse_bits())),
        ("s_not_b32", Operand::Const(c)) => inline(c).map(|x| Moved::Const(!x)),
        ("s_mov_b64", Operand::Const(c @ 128..=208)) => {
            let low = inline(c)?;
            Some(Moved::Const(if k == 0 {
                low
            } else {
                ((low as i32) >> 31) as u32
            }))
        }
        _ => None,
    }
}

pub(crate) fn inline(code: u8) -> Option<u32> {
    let float = |f: f32| Some(f.to_bits());
    match code {
        128..=192 => Some(u32::from(code) - 128),
        193..=208 => Some((192 - i32::from(code)) as u32),
        240 => float(0.5),
        241 => float(-0.5),
        242 => float(1.0),
        243 => float(-1.0),
        244 => float(2.0),
        245 => float(-2.0),
        246 => float(4.0),
        247 => float(-4.0),
        248 => Some(0x3E22_F983),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::native::RootType::*;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn track_words(words: &[u32], layout: &[RootParam], stage: HwStage) -> Resources {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, false, 256).unwrap();
        track(&p, layout, stage, lanes(&p).as_ref()).unwrap()
    }

    fn lanes(p: &Program) -> Option<Structured> {
        crate::structure::structure(p).ok()
    }

    fn param(root: RootType, user_data: u8, sgprs: u32) -> RootParam {
        RootParam {
            root,
            table_offset_dwords: 0,
            sgprs,
            base_register: 0,
            user_data,
            slot: 0,
        }
    }

    fn at(param: usize, offset: u32) -> Address {
        Address {
            param,
            offset,
            dynamic: None,
        }
    }

    #[test]
    fn table_descriptors_are_loaded_from_their_range() {
        let r = track_words(
            &[
                0xBE82_0309,
                0xBE89_0380,
                0xBE83_0380,
                0xF408_0204,
                0xFA00_0000,
                0xF40C_0001,
                0xFA00_0000,
                0xF080_0F08,
                0x0040_0002,
                S_ENDPGM,
            ],
            &[param(Table, 8, 1), param(Table, 9, 1)],
            HwStage::Ps,
        );
        assert_eq!(r.scalar, [((0, 3), at(0, 0)), ((0, 4), at(1, 0))]);
        let roles: Vec<_> = r.descriptors.iter().map(|d| (d.role, &d.source)).collect();
        assert_eq!(
            roles,
            [
                (
                    Role::Resource,
                    &Source::Loaded {
                        load: (0, 4),
                        first: 0
                    }
                ),
                (
                    Role::Sampler,
                    &Source::Loaded {
                        load: (0, 3),
                        first: 0
                    }
                ),
            ]
        );
    }

    #[test]
    fn root_descriptors_are_built_in_code() {
        let r = track_words(
            &[
                0x8801_FF0B,
                0x0010_0000,
                0xBE80_030A,
                0xBE82_03C1,
                0xBE83_03FF,
                0x1101_4FAC,
                0xF430_0000,
                0xFA00_0000,
                S_ENDPGM,
            ],
            &[param(Descriptor, 10, 2)],
            HwStage::Vs,
        );
        assert_eq!(
            r.descriptors[0].source,
            Source::Built {
                address: at(0, 0),
                bits: 0x10_0000,
                words: [u32::MAX, 0x1101_4FAC]
            }
        );
        assert_eq!(
            r.params.iter().map(|p| (p.1, p.2)).collect::<Vec<_>>(),
            [(0, 0), (0, 1)]
        );
    }

    #[test]
    fn heap_indices_from_root_constants_in_a_merged_stage() {
        let code = [
            0x8F08_850E,
            0xBE93_0380,
            0xF408_0209,
            0x1000_0000,
            0xE000_2000,
            0x8002_0400,
            S_ENDPGM,
        ];
        let r = track_words(
            &code,
            &[param(Constants, 6, 1), param(ResourceHeap, 10, 1)],
            HwStage::Ms,
        );
        let index = r.params.iter().find(|p| p.1 == 0).unwrap().0;
        let shift = r.scalar[0].1.dynamic.unwrap();
        assert_eq!(r.scalar[0].1.param, 1);
        assert_ne!(shift, index);
        assert_eq!(
            r.descriptors[0].source,
            Source::Loaded {
                load: (0, 2),
                first: 0
            }
        );
        assert!(r.root_indexed_views.is_empty());
        let table = RootParam {
            slot: 7,
            ..param(Table, 10, 1)
        };
        let indexed = track_words(&code, &[param(Constants, 6, 1), table], HwStage::Ms);
        assert_eq!(
            indexed.root_indexed_views,
            [RootIndexedView {
                table_slot: 7,
                constant_slot: 0,
                constant_dword: 0,
                byte_offset: 0,
                shift: 5,
            }]
        );
        let nonconstant = track_words(&code, &[param(Table, 6, 1), table], HwStage::Ms);
        assert!(nonconstant.root_indexed_views.is_empty());
    }

    #[test]
    fn spilled_sgprs_are_followed_through_vgpr_lanes() {
        let r = track_words(
            &[
                0xD761_0000,
                0x0001_0609,
                0xD760_0002,
                0x0001_0700,
                0xBE83_0380,
                0xF40C_0001,
                0xFA00_0020,
                S_ENDPGM,
            ],
            &[param(Table, 9, 1)],
            HwStage::Ps,
        );
        assert_eq!(r.scalar, [((0, 3), at(0, 0x20))]);
    }

    #[test]
    fn descriptors_joined_by_control_flow_are_traced_per_edge() {
        let r = track_words(
            &[
                0xBE82_0309,
                0xBE83_0380,
                0xBF06_8001,
                0xBF85_0003,
                0xF408_0001,
                0xFA00_0000,
                0xBF82_0002,
                0xF408_0001,
                0xFA00_0010,
                0xF420_0100,
                0xFA00_0000,
                S_ENDPGM,
            ],
            &[param(Table, 9, 1)],
            HwStage::Ps,
        );
        let Source::Phi { args, .. } = &r.descriptors[0].source else {
            panic!("{:?}", r.descriptors)
        };
        let offsets: Vec<_> = args
            .iter()
            .map(|a| match a {
                Source::Loaded { load, .. } => {
                    r.scalar.iter().find(|s| s.0 == *load).unwrap().1.offset
                }
                a => panic!("{a:?}"),
            })
            .collect();
        assert_eq!(offsets.len(), 2);
        assert!(
            offsets.contains(&0) && offsets.contains(&0x10),
            "{offsets:?}"
        );
    }

    #[test]
    fn two_paths_into_one_code_table_load() {
        let r = track_words(
            &[
                0xBE82_1F00,
                0x8002_FF02,
                0x0000_0040,
                0x8203_8003,
                0xBF06_8001,
                0xBF85_0005,
                0xD70F_0457,
                0x0000_054A,
                0xD528_0458,
                0x0010_0680,
                0xBF82_0004,
                0xD70F_0457,
                0x0000_054B,
                0xD528_0458,
                0x0010_0680,
                0xDC34_8000,
                0x577D_0057,
                S_ENDPGM,
            ],
            &[],
            HwStage::Cs,
        );
        assert_eq!(r.scalar.len(), 1);
        let a = r.scalar[0].1;
        assert_eq!((a.param, a.offset), (CODE, 0));
        assert!(
            a.dynamic.is_some(),
            "the join is the computed term, the byte in the code"
        );
    }

    #[test]
    fn a_code_table_address_taken_from_the_first_lane() {
        let r = track_words(
            &[
                0xBE82_1F00,
                0xD70F_0457,
                0x0000_054A,
                0xD528_0458,
                0x0010_0680,
                0x7E0C_0557,
                0x7E0E_0558,
                0xF400_0203,
                0xFA00_0000,
                S_ENDPGM,
            ],
            &[],
            HwStage::Cs,
        );
        assert_eq!(r.scalar.len(), 1);
        let a = r.scalar[0].1;
        assert_eq!((a.param, a.offset), (CODE, 4));
        assert!(a.dynamic.is_some(), "the lane's index is the computed term");
    }

    #[test]
    fn a_descriptor_kept_per_lane_is_its_load_and_needs_no_waterfall() {
        let mut words = vec![0xBE82_0309, 0xBE83_0380, 0xF40C_0001, 0xFA00_0000];
        words.extend((0..8).map(|k| 0x7E00_0200 | (10 + k) << 17 | k));
        words.extend((0..8).map(|k| 0x7E00_0400 | (16 + k) << 17 | (0x100 + 10 + k)));
        words.extend([0xF000_1700, 0x0004_0D06, S_ENDPGM]);
        let r = track_words(&words, &[param(Table, 9, 1)], HwStage::Ps);
        let load = (0, 2);
        assert_eq!(r.scalar, [(load, at(0, 0))]);
        let sources: Vec<_> = r.descriptors.iter().map(|d| &d.source).collect();
        assert_eq!(sources, [&Source::Loaded { load, first: 0 }]);
        assert_eq!(r.lane_copied, [load]);
        assert_eq!(r.waterfall, Waterfall::default());
    }

    #[test]
    fn loads_at_one_first_lane_offset_have_one_address() {
        let r = track_words(
            &[
                0xBE82_0309,
                0xBE83_0380,
                0x7E08_0504,
                0xF40C_0201,
                0x0800_0000,
                0x7E0A_0504,
                0xF40C_0401,
                0x0A00_0000,
                0xF000_1700,
                0x0004_0D06,
                S_ENDPGM,
            ],
            &[param(Table, 9, 1)],
            HwStage::Ps,
        );
        let [(_, a), (_, b)] = r.scalar[..] else {
            panic!("{:?}", r.scalar)
        };
        assert_eq!(a, b);
        assert_eq!((a.param, a.offset), (0, 0));
        assert!(
            a.dynamic.is_some(),
            "the lane's offset is the computed term"
        );
    }

    #[test]
    fn descriptors_selected_by_scc_are_traced_per_outcome() {
        let r = track_words(
            &[
                0xBE8C_0309,
                0xBE8D_0380,
                0xF408_0006,
                0xFA00_0000,
                0xF408_0106,
                0xFA00_0010,
                0xBF06_800E,
                0x8588_0400,
                0x858A_0602,
                0xF420_0404,
                0xFA00_0000,
                S_ENDPGM,
            ],
            &[param(Table, 9, 1)],
            HwStage::Ps,
        );
        let Source::Select { args, .. } = &r.descriptors[0].source else {
            panic!("{:?}", r.descriptors)
        };
        assert_eq!(
            args[..],
            [
                Source::Loaded {
                    load: (0, 2),
                    first: 0
                },
                Source::Loaded {
                    load: (0, 3),
                    first: 0
                },
            ]
        );
        assert_eq!(r.scalar, [((0, 2), at(0, 0)), ((0, 3), at(0, 0x10))]);
    }

    #[test]
    fn a_selection_that_mixes_two_descriptors_fails() {
        let code: Vec<u8> = [
            0xBE8C_0309,
            0xBE8D_0380,
            0xF408_0006,
            0xFA00_0000,
            0xF408_0106,
            0xFA00_0010,
            0xBF06_800E,
            0x8588_0400,
            0x858A_0202,
            0xF420_0404,
            0xFA00_0000,
            S_ENDPGM,
        ]
        .iter()
        .flat_map(|w: &u32| w.to_le_bytes())
        .collect();
        let p = crate::ir::build(&code, false, 256).unwrap();
        let err = track(&p, &[param(Table, 9, 1)], HwStage::Ps, lanes(&p).as_ref()).unwrap_err();
        assert_eq!(
            err,
            ShaderError::Unsupported {
                offset: 36,
                what: "a descriptor changed after loading"
            }
        );
    }

    #[test]
    fn untraceable_addresses_fail_with_a_reason() {
        let code: Vec<u8> = [0xF408_0001, 0xFA00_0000, S_ENDPGM]
            .iter()
            .flat_map(|w: &u32| w.to_le_bytes())
            .collect();
        let p = crate::ir::build(&code, false, 256).unwrap();
        let err = track(&p, &[param(Table, 9, 1)], HwStage::Ps, lanes(&p).as_ref()).unwrap_err();
        assert_eq!(
            err,
            ShaderError::Unsupported {
                offset: 0,
                what: "an address in an SGPR that is no root parameter"
            }
        );
    }

    pub(crate) const WATERFALL_FETCHED_WAVE32: &[u32] = &[
        0xBE84_0300,
        0xBE85_0380,
        0xBE86_0301,
        0xBE87_0380,
        0x3402_0085,
        0xDC38_8000,
        0x0404_0001,
        0xDC38_8010,
        0x0804_0001,
        0xF408_0203,
        0xFA00_0000,
        0x7E04_0D00,
        0x7E06_0302,
        0xBF8C_0070,
        0xBE94_037E,
        0x7E18_0504,
        0x7E1A_0505,
        0x7E1C_0506,
        0x7E1E_0507,
        0x7E20_0508,
        0x7E22_0509,
        0x7E24_050A,
        0x7E26_050B,
        0xD4E2_0015,
        0x0002_080C,
        0xD4E2_0016,
        0x0002_0C0E,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1010,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1412,
        0x8715_1615,
        0xBE95_3C15,
        0xBF80_0000,
        0xF09C_0F08,
        0x0043_0C02,
        0xBF8C_3F70,
        0x897E_157E,
        0xBF89_FFE6,
        0xBEFE_0314,
        0xD834_0000,
        0x0000_0C00,
        0xBF81_0000,
    ];

    pub(crate) const WATERFALL_FETCHED_WAVE64: &[u32] = &[
        0xBE84_0300,
        0xBE85_0380,
        0xBE86_0301,
        0xBE87_0380,
        0x3402_0085,
        0xDC38_8000,
        0x0404_0001,
        0xDC38_8010,
        0x0804_0001,
        0xF408_0203,
        0xFA00_0000,
        0x7E04_0D00,
        0x7E06_0302,
        0xBF8C_0070,
        0xBE94_047E,
        0x7E18_0504,
        0x7E1A_0505,
        0x7E1C_0506,
        0x7E1E_0507,
        0x7E20_0508,
        0x7E22_0509,
        0x7E24_050A,
        0x7E26_050B,
        0xD4E2_0016,
        0x0002_080C,
        0xD4E2_0018,
        0x0002_0C0E,
        0x8796_1816,
        0xD4E2_0018,
        0x0002_1010,
        0x8796_1816,
        0xD4E2_0018,
        0x0002_1412,
        0x8796_1816,
        0xBE96_2416,
        0xBF80_0000,
        0xF09C_0F08,
        0x0043_0C02,
        0xBF8C_3F70,
        0x89FE_167E,
        0xBF89_FFE6,
        0xBEFE_0414,
        0xD834_0000,
        0x0000_0C00,
        0xBF81_0000,
    ];

    pub(crate) const WATERFALL_PICKED_WAVE32: &[u32] = &[
        0xBE84_0300,
        0xBE85_0380,
        0xBE86_0301,
        0xBE87_0380,
        0xF40C_0602,
        0xFA00_0000,
        0xF40C_0802,
        0xFA00_0020,
        0xF408_0203,
        0xFA00_0000,
        0x7E04_0D00,
        0x7E06_0302,
        0x7D88_0090,
        0xBF8C_C07F,
        0x7E08_0218,
        0x0208_0820,
        0x7E0A_0219,
        0x020A_0A21,
        0x7E0C_021A,
        0x020C_0C22,
        0x7E0E_021B,
        0x020E_0E23,
        0x7E10_021C,
        0x0210_1024,
        0x7E12_021D,
        0x0212_1225,
        0x7E14_021E,
        0x0214_1426,
        0x7E16_021F,
        0x0216_1627,
        0xBE94_037E,
        0x7E18_0504,
        0x7E1A_0505,
        0x7E1C_0506,
        0x7E1E_0507,
        0x7E20_0508,
        0x7E22_0509,
        0x7E24_050A,
        0x7E26_050B,
        0xD4E2_0015,
        0x0002_080C,
        0xD4E2_0016,
        0x0002_0C0E,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1010,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1412,
        0x8715_1615,
        0xBE95_3C15,
        0xBF80_0000,
        0xF09C_0F08,
        0x0043_0C02,
        0xBF8C_3F70,
        0x897E_157E,
        0xBF89_FFE6,
        0xBEFE_0314,
        0xD834_0000,
        0x0000_0C00,
        0xBF81_0000,
    ];

    pub(crate) const WATERFALL_JOINED_WAVE32: &[u32] = &[
        0xBE84_0300,
        0xBE85_0380,
        0xBE86_0301,
        0xBE87_0380,
        0xF40C_0602,
        0xFA00_0000,
        0xF40C_0802,
        0xFA00_0020,
        0xF408_0203,
        0xFA00_0000,
        0xF408_0A02,
        0xFA00_0040,
        0x7E04_0D00,
        0x7E06_0302,
        0x3420_0082,
        0x7D88_0090,
        0xBF8C_C07F,
        0xBE94_3C6A,
        0x8914_147E,
        0xBF88_0008,
        0x7E08_0218,
        0x7E0A_0219,
        0x7E0C_021A,
        0x7E0E_021B,
        0x7E10_021C,
        0x7E12_021D,
        0x7E14_021E,
        0x7E16_021F,
        0xBE94_3D14,
        0x897E_147E,
        0xBF88_0008,
        0x7E08_0220,
        0x7E0A_0221,
        0x7E0C_0222,
        0x7E0E_0223,
        0x7E10_0224,
        0x7E12_0225,
        0x7E14_0226,
        0x7E16_0227,
        0x887E_147E,
        0xBE97_037E,
        0x7E18_0504,
        0x7E1A_0505,
        0x7E1C_0506,
        0x7E1E_0507,
        0x7E20_0508,
        0x7E22_0509,
        0x7E24_050A,
        0x7E26_050B,
        0xD4E2_0015,
        0x0002_080C,
        0xD4E2_0016,
        0x0002_0C0E,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1010,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1412,
        0x8715_1615,
        0xBE95_3C15,
        0xBF80_0000,
        0xF09C_0F08,
        0x0043_0C02,
        0xBF8C_3F70,
        0x897E_157E,
        0xBF89_FFE6,
        0xBEFE_0317,
        0xE070_1000,
        0x800A_0C10,
        0xBF81_0000,
    ];

    pub(crate) const WATERFALL_INDEXED_WAVE32: &[u32] = &[
        0xBE84_0300,
        0xBE85_0380,
        0xBE86_0301,
        0xBE87_0380,
        0xF408_0203,
        0xFA00_0000,
        0xF408_0A02,
        0xFA00_0040,
        0x7E04_0D00,
        0x7E06_0302,
        0x3420_0082,
        0x360E_0081,
        0x340E_0E85,
        0xBF8C_C07F,
        0xBE97_037E,
        0xBE95_037E,
        0x7E28_0507,
        0x7DA4_0E14,
        0xF40C_0302,
        0x2800_0000,
        0xBF8C_C07F,
        0xF09C_0F08,
        0x0043_0C02,
        0xBF8C_3F70,
        0xBE95_4715,
        0xBF89_FFF6,
        0xBEFE_0317,
        0xE070_1000,
        0x800A_0C10,
        0xBF81_0000,
    ];

    #[test]
    fn the_console_waterfall_over_a_first_lane_offset_is_one() {
        let r = waterfall(WATERFALL_INDEXED_WAVE32, false).unwrap();
        let image = r
            .descriptors
            .iter()
            .find(|d| d.role == Role::Resource)
            .unwrap();
        let Source::Loaded { load, first: 0 } = image.source else {
            panic!("{:?}", image.source)
        };
        let (_, address) = r.scalar.iter().find(|s| s.0 == load).unwrap();
        assert!(address.dynamic.is_some(), "{address:?}");
        assert_eq!(r.waterfall.compares.len(), 1);
        assert_eq!(r.waterfall.reads.len(), 1);
        assert_eq!(r.waterfall.loads, [load]);
    }

    pub(crate) const WATERFALL_JOINED_MIXED_WAVE32: &[u32] = &[
        0xBE84_0300,
        0xBE85_0380,
        0xBE86_0301,
        0xBE87_0380,
        0xF40C_0602,
        0xFA00_0000,
        0xF40C_0802,
        0xFA00_0020,
        0xF408_0203,
        0xFA00_0000,
        0xF408_0A02,
        0xFA00_0040,
        0x7E04_0D00,
        0x7E06_0302,
        0x3420_0082,
        0x3602_0081,
        0x3402_0285,
        0x7D88_0090,
        0xBF8C_C07F,
        0xBE94_3C6A,
        0x8914_147E,
        0xBF88_0004,
        0xDC38_8000,
        0x0404_0001,
        0xDC38_8010,
        0x0804_0001,
        0xBE94_3D14,
        0x897E_147E,
        0xBF88_0011,
        0x7D88_0098,
        0x7E08_0218,
        0x0208_0820,
        0x7E0A_0219,
        0x020A_0A21,
        0x7E0C_021A,
        0x020C_0C22,
        0x7E0E_021B,
        0x020E_0E23,
        0x7E10_021C,
        0x0210_1024,
        0x7E12_021D,
        0x0212_1225,
        0x7E14_021E,
        0x0214_1426,
        0x7E16_021F,
        0x0216_1627,
        0x887E_147E,
        0xBE97_037E,
        0xBF8C_3F70,
        0x7E18_0504,
        0x7E1A_0505,
        0x7E1C_0506,
        0x7E1E_0507,
        0x7E20_0508,
        0x7E22_0509,
        0x7E24_050A,
        0x7E26_050B,
        0xD4E2_0015,
        0x0002_080C,
        0xD4E2_0016,
        0x0002_0C0E,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1010,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1412,
        0x8715_1615,
        0xBE95_3C15,
        0xBF80_0000,
        0xF09C_0F08,
        0x0043_0C02,
        0xBF8C_3F70,
        0x897E_157E,
        0xBF89_FFE6,
        0xBEFE_0317,
        0xE070_1000,
        0x800A_0C10,
        0xBF81_0000,
    ];

    pub(crate) const WATERFALL_LOOPED_WAVE32: &[u32] = &[
        0xBE84_0300,
        0xBE85_0380,
        0xBE86_0301,
        0xBE87_0380,
        0xF40C_0602,
        0xFA00_0000,
        0xF40C_0802,
        0xFA00_0020,
        0xF408_0203,
        0xFA00_0000,
        0xF408_0A02,
        0xFA00_0040,
        0x7E04_0D00,
        0x7E06_0302,
        0x3420_0082,
        0x7E08_0218,
        0x7E0A_0219,
        0x7E0C_021A,
        0x7E0E_021B,
        0x7E10_021C,
        0x7E12_021D,
        0x7E14_021E,
        0x7E16_021F,
        0xBF8C_C07F,
        0xBE97_037E,
        0x7E18_0504,
        0x7E1A_0505,
        0x7E1C_0506,
        0x7E1E_0507,
        0x7E20_0508,
        0x7E22_0509,
        0x7E24_050A,
        0x7E26_050B,
        0xD4E2_0015,
        0x0002_080C,
        0xD4E2_0016,
        0x0002_0C0E,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1010,
        0x8715_1615,
        0xD4E2_0016,
        0x0002_1412,
        0x8715_1615,
        0xBE95_3C15,
        0xBF80_0000,
        0xF09C_0F08,
        0x0043_0C02,
        0xBF8C_3F70,
        0x897E_157E,
        0x7E08_0220,
        0x7E0A_0221,
        0x7E0C_0222,
        0x7E0E_0223,
        0x7E10_0224,
        0x7E12_0225,
        0x7E14_0226,
        0x7E16_0227,
        0xBF89_FFDE,
        0xBEFE_0317,
        0xE070_1000,
        0x800A_0C10,
        0xBF81_0000,
    ];

    pub(crate) const WATERFALL_JOINED_BUFFER_WAVE32: &[u32] = &[
        0xBE84_0300,
        0xBE85_0380,
        0xF408_0602,
        0xFA00_0000,
        0xF408_0802,
        0xFA00_0020,
        0xF408_0A02,
        0xFA00_0040,
        0x3420_0082,
        0xD4C4_001E,
        0x0002_0098,
        0x7D88_0090,
        0xBF8C_C07F,
        0xBE94_3C6A,
        0x8914_147E,
        0xBF88_0004,
        0x7E08_0218,
        0x7E0A_0219,
        0x7E0C_021A,
        0x7E0E_021B,
        0xBE94_3D14,
        0x897E_147E,
        0xBF88_0007,
        0xBE9F_3C1E,
        0xBF88_0004,
        0x7E08_0220,
        0x7E0A_0221,
        0x7E0C_0222,
        0x7E0E_0223,
        0x887E_1F7E,
        0x887E_147E,
        0xBE97_3C1E,
        0xBF88_0013,
        0xBE9D_037E,
        0x7E18_0504,
        0x7E1A_0505,
        0x7E1C_0506,
        0x7E1E_0507,
        0xD4E2_0015,
        0x0002_080C,
        0xD4E2_0016,
        0x0002_0C0E,
        0x8715_1615,
        0xBE95_3C15,
        0xE030_1000,
        0x8003_0C10,
        0xBF8C_3F70,
        0x897E_157E,
        0xBF89_FFF1,
        0xBEFE_031D,
        0xE070_1000,
        0x800A_0C10,
        0x887E_177E,
        0xBF81_0000,
    ];

    fn waterfall(words: &[u32], wave64: bool) -> Result<Resources, ShaderError> {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, wave64, 256).unwrap();
        let layout = [param(Table, 0, 1), param(Table, 1, 1)];
        track(&p, &layout, HwStage::Cs, lanes(&p).as_ref())
    }

    #[test]
    fn descriptors_read_from_the_first_lane_of_per_lane_loads_are_traced_to_them() {
        for (words, wave64) in [
            (WATERFALL_FETCHED_WAVE32, false),
            (WATERFALL_FETCHED_WAVE64, true),
        ] {
            let r = waterfall(words, wave64).unwrap();
            let image = r
                .descriptors
                .iter()
                .find(|d| d.role == Role::Resource)
                .unwrap();
            let Source::Fetched { load, first: 0 } = image.source else {
                panic!("{:?}", image.source)
            };
            let a = r.scalar.iter().find(|s| s.0 == load).unwrap().1;
            assert_eq!((a.param, a.offset), (0, 0));
            assert!(a.dynamic.is_some());
            let sampler = r.descriptors.iter().find(|d| d.role == Role::Sampler);
            assert!(matches!(
                sampler.unwrap().source,
                Source::Loaded { first: 0, .. }
            ));
            assert_eq!(r.waterfall.compares.len(), 4);
            assert_eq!(r.waterfall.reads.len(), 8);
        }
    }

    #[test]
    fn descriptors_read_from_the_first_lane_of_a_per_lane_choice_are_traced_per_lane() {
        let r = waterfall(WATERFALL_PICKED_WAVE32, false).unwrap();
        let image = &r.descriptors[0];
        let Source::Lanes { args, .. } = &image.source else {
            panic!("{:?}", image.source)
        };
        let offsets: Vec<u32> = args
            .iter()
            .map(|a| match *a {
                Source::Loaded { load, first: 0 } => {
                    r.scalar.iter().find(|s| s.0 == load).unwrap().1.offset
                }
                ref a => panic!("{a:?}"),
            })
            .collect();
        assert_eq!(offsets, [0, 0x20]);
        assert_eq!(r.waterfall.compares.len(), 4);
    }

    #[test]
    fn a_first_lane_read_outside_a_loop_is_no_waterfall() {
        let mut words = WATERFALL_FETCHED_WAVE32.to_vec();
        let back = words.iter().position(|&w| w == 0xBF89_FFE6).unwrap();
        words[back] = 0xBF80_0000;
        let err = waterfall(&words, false).unwrap_err();
        assert!(
            matches!(
                err,
                ShaderError::Unsupported {
                    what: "a descriptor read from the first lane outside a waterfall loop",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    fn refusal(words: &[u32], wave64: bool) -> &'static str {
        match waterfall(words, wave64) {
            Err(ShaderError::Unsupported { what, .. }) => what,
            other => panic!("not refused: {other:?}"),
        }
    }

    const NO_LOOP: &str = "a descriptor read from the first lane outside a waterfall loop";
    const NOT_SELECTED: &str = "a waterfall loop whose compares do not select the access's lanes";
    const BAD_RETRY: &str = "a waterfall loop whose retry does not keep the lanes still waiting";
    const SPLIT: &str = "a waterfall loop nested in another or split over blocks";

    fn word_at(words: &[u32], word: u32) -> usize {
        words.iter().position(|&w| w == word).unwrap()
    }

    #[test]
    fn a_first_lane_read_without_its_compare_is_no_waterfall() {
        let mut words = WATERFALL_FETCHED_WAVE32.to_vec();
        let cmp = word_at(&words, 0x0002_080C);
        words[cmp - 1..=cmp].copy_from_slice(&[0xBF80_0000, 0xBF80_0000]);
        assert_eq!(refusal(&words, false), NOT_SELECTED);
    }

    #[test]
    fn a_compare_that_does_not_mask_the_access_is_no_waterfall() {
        let mut words = WATERFALL_JOINED_WAVE32.to_vec();
        let save = word_at(&words, 0xBE95_3C15);
        words[save] = 0xBE95_037E;
        assert_eq!(refusal(&words, false), NOT_SELECTED);
    }

    #[test]
    fn an_access_under_a_further_mask_is_no_waterfall() {
        let mut words = WATERFALL_JOINED_WAVE32.to_vec();
        let nop = word_at(&words, 0xBE95_3C15) + 1;
        assert_eq!(words[nop], 0xBF80_0000);
        words[nop] = 0x877E_6A7E;
        assert_eq!(refusal(&words, false), NOT_SELECTED);
    }

    #[test]
    fn a_loop_that_drops_unmatched_lanes_is_no_waterfall() {
        let mut words = WATERFALL_JOINED_WAVE32.to_vec();
        let retry = word_at(&words, 0x897E_157E);
        words[retry] = 0xBEFE_0380;
        assert_eq!(refusal(&words, false), BAD_RETRY);
    }

    #[test]
    fn a_retry_that_keeps_other_lanes_than_those_waiting_is_no_waterfall() {
        let mut words = WATERFALL_JOINED_WAVE32.to_vec();
        let retry = word_at(&words, 0x897E_157E);
        words[retry] = 0x8A7E_7E6A;
        assert_eq!(refusal(&words, false), BAD_RETRY);
    }

    #[test]
    fn a_wave64_retry_of_one_half_is_no_waterfall() {
        let mut words = WATERFALL_FETCHED_WAVE64.to_vec();
        let retry = word_at(&words, 0x89FE_167E);
        words[retry] = 0x897E_167E;
        assert_eq!(refusal(&words, true), BAD_RETRY);
    }

    #[test]
    fn a_descriptor_used_after_exec_is_restored_is_no_waterfall() {
        let mut words = WATERFALL_JOINED_WAVE32.to_vec();
        let sample = word_at(&words, 0xF09C_0F08);
        let restore = word_at(&words, 0xBEFE_0317);
        let image = [words[sample], words[sample + 1]];
        words[sample..sample + 2].fill(0xBF80_0000);
        words.splice(restore + 1..restore + 1, image);
        assert_eq!(refusal(&words, false), NO_LOOP);
    }

    #[test]
    fn a_wave64_access_with_exec_hi_overwritten_is_no_waterfall() {
        let mut words = WATERFALL_FETCHED_WAVE64.to_vec();
        let save = word_at(&words, 0xBE96_2416);
        words[save + 1] = 0xBEFF_03C1;
        assert_eq!(refusal(&words, true), NOT_SELECTED);
    }

    pub(crate) fn cmpx_waterfall(wave64: bool, folded: bool) -> Vec<u32> {
        let (base, todo, restore): (&[u32], u32, u32) = if wave64 {
            (WATERFALL_FETCHED_WAVE64, 0xBE94_047E, 0xBEFE_0414)
        } else {
            (WATERFALL_JOINED_WAVE32, 0xBE97_037E, 0xBEFE_0317)
        };
        let reads = word_at(base, 0x7E18_0504);
        let sample = word_at(base, 0xF09C_0F08);
        let mut w = base[..=word_at(base, todo)].to_vec();
        w.push(if wave64 { 0xBE96_047E } else { 0xBE95_037E });
        w.extend(&base[reads..reads + 8]);
        w.extend([0x7DE4_080C, 0x7DE4_0C0E, 0x7DE4_1010, 0x7DE4_1412]);
        w.extend(&base[sample..sample + 2]);
        match (wave64, folded) {
            (false, true) => w.extend([0xBE95_4715, 0xBF89_FFF0]),
            (false, false) => w.extend([0x8A15_7E15, 0xBEFE_0315, 0xBF89_FFEF]),
            (true, true) => w.extend([0xBE96_3A16, 0xBF89_FFF0]),
            (true, false) => w.extend([0x8A96_7E16, 0xBEFE_0416, 0xBF89_FFEF]),
        }
        w.extend(&base[word_at(base, restore)..]);
        w
    }

    #[test]
    fn a_waterfall_narrowed_by_v_cmpx_is_one() {
        for wave64 in [false, true] {
            for folded in [false, true] {
                let r = waterfall(&cmpx_waterfall(wave64, folded), wave64)
                    .unwrap_or_else(|e| panic!("wave64 {wave64} folded {folded}: {e:?}"));
                assert_eq!(r.waterfall.compares.len(), 4, "{wave64} {folded}");
                assert_eq!(r.waterfall.reads.len(), 8, "{wave64} {folded}");
            }
        }
    }

    #[test]
    fn a_v_cmpx_waterfall_whose_retry_drops_lanes_is_none() {
        let mut words = cmpx_waterfall(false, false);
        let retry = word_at(&words, 0x8A15_7E15);
        words[retry] = 0x8A15_7E6A;
        assert_eq!(refusal(&words, false), BAD_RETRY);
    }

    #[test]
    fn a_waterfall_nested_in_another_is_refused_by_name() {
        let words = [
            0xBE84_0300,
            0xBE85_0380,
            0xBE86_0301,
            0xBE87_0380,
            0xF408_0A02,
            0xFA00_0040,
            0x7E04_0D00,
            0x7E06_0302,
            0x3420_0082,
            0x3602_0081,
            0x3402_0285,
            0x3622_0080,
            0x3422_2284,
            0xDC38_8000,
            0x0404_0001,
            0xDC38_8010,
            0x0804_0001,
            0xDC38_8000,
            0x1406_0011,
            0xBF8C_0070,
            0xBE94_037E,
            0x7E18_0504,
            0x7E1A_0505,
            0x7E1C_0506,
            0x7E1E_0507,
            0x7E20_0508,
            0x7E22_0509,
            0x7E24_050A,
            0x7E26_050B,
            0xD4E2_0015,
            0x0002_080C,
            0xD4E2_0016,
            0x0002_0C0E,
            0x8715_1615,
            0xD4E2_0016,
            0x0002_1010,
            0x8715_1615,
            0xD4E2_0016,
            0x0002_1412,
            0x8715_1615,
            0xBE95_3C15,
            0xBE9A_037E,
            0x7E10_0514,
            0x7E12_0515,
            0x7E14_0516,
            0x7E16_0517,
            0xD4E2_0018,
            0x0002_2808,
            0xD4E2_0019,
            0x0002_2C0A,
            0x8718_1918,
            0xBE98_3C18,
            0xF09C_0F08,
            0x0043_0C02,
            0x897E_187E,
            0xBF89_FFF2,
            0xBEFE_031A,
            0x897E_157E,
            0xBF89_FFDA,
            0xBEFE_0314,
            0xE070_1000,
            0x800A_0C10,
            0xBF81_0000,
        ];
        assert_eq!(refusal(&words, false), SPLIT);
    }

    #[test]
    fn descriptors_read_from_the_first_lane_of_lds_fail_naming_it() {
        let mut words = WATERFALL_FETCHED_WAVE32.to_vec();
        words[5..9].copy_from_slice(&[0xDBFC_0000, 0x0400_0001, 0xDBFC_0010, 0x0800_0001]);
        let err = waterfall(&words, false).unwrap_err();
        assert!(
            matches!(
                err,
                ShaderError::Unsupported {
                    what: "a descriptor read from the first lane of an LDS read",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    fn block_at(words: &[u32], offset: usize) -> usize {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, false, 256).unwrap();
        p.blocks.iter().position(|b| b.offset == offset).unwrap()
    }

    fn table_offset(r: &Resources, s: &Source) -> u32 {
        match *s {
            Source::Loaded { load, first: 0 } => {
                r.scalar.iter().find(|x| x.0 == load).unwrap().1.offset
            }
            ref s => panic!("{s:?}"),
        }
    }

    #[test]
    fn descriptors_read_from_the_first_lane_of_an_if_else_join_are_traced_per_path() {
        let words = WATERFALL_JOINED_WAVE32;
        let r = waterfall(words, false).unwrap();
        let image = r
            .descriptors
            .iter()
            .find(|d| matches!(d.source, Source::LanePhi { .. }))
            .unwrap();
        let Source::LanePhi { block, args } = &image.source else {
            unreachable!()
        };
        assert_eq!(*block, block_at(words, 0x9c));
        let [Some(flow), Some(other)] = &args[..] else {
            panic!("{args:?}")
        };
        assert_eq!(table_offset(&r, other), 0x20);
        let Source::LanePhi { block, args } = flow else {
            panic!("{flow:?}")
        };
        assert_eq!(*block, block_at(words, 0x70));
        let [None, Some(then)] = &args[..] else {
            panic!("{args:?}")
        };
        assert_eq!(table_offset(&r, then), 0);
        assert_eq!(r.waterfall.compares.len(), 4);
        assert_eq!(r.waterfall.reads.len(), 8);
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, false, 256).unwrap();
        let layout = [param(Table, 0, 1), param(Table, 1, 1)];
        let err = track(&p, &layout, HwStage::Cs, None).unwrap_err();
        assert!(
            matches!(
                err,
                ShaderError::Unsupported {
                    what: "a descriptor read from the first lane of a phi, without the lanes' paths",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn descriptors_joined_from_a_per_lane_load_and_a_per_lane_choice_are_traced_per_path() {
        let words = WATERFALL_JOINED_MIXED_WAVE32;
        let r = waterfall(words, false).unwrap();
        let image = r
            .descriptors
            .iter()
            .find(|d| matches!(d.source, Source::LanePhi { .. }))
            .unwrap();
        let Source::LanePhi { block, args } = &image.source else {
            unreachable!()
        };
        assert_eq!(*block, block_at(words, 0xb8));
        let [
            Some(Source::LanePhi { args: flow, .. }),
            Some(Source::Lanes { args: picked, .. }),
        ] = &args[..]
        else {
            panic!("{args:?}")
        };
        let [None, Some(Source::Fetched { load, first: 0 })] = flow[..] else {
            panic!("{flow:?}")
        };
        let a = r.scalar.iter().find(|s| s.0 == load).unwrap().1;
        assert_eq!((a.param, a.offset), (0, 0));
        assert!(a.dynamic.is_some());
        let offsets: Vec<u32> = picked.iter().map(|s| table_offset(&r, s)).collect();
        assert_eq!(offsets, [0, 0x20]);
        assert_eq!(r.waterfall.compares.len(), 4);
    }

    #[test]
    fn a_descriptor_read_from_the_first_lane_of_a_loop_phi_fails_naming_it() {
        let err = waterfall(WATERFALL_LOOPED_WAVE32, false).unwrap_err();
        assert!(
            matches!(
                err,
                ShaderError::Unsupported {
                    what: "a descriptor read from the first lane of a phi at a loop header",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_per_lane_descriptor_some_lanes_do_not_write_fails() {
        let mut words = WATERFALL_JOINED_WAVE32.to_vec();
        let skip = words.iter().rposition(|&w| w == 0xBF88_0008).unwrap();
        words[skip] = 0xBF80_0000;
        let err = waterfall(&words, false).unwrap_err();
        assert!(
            matches!(
                err,
                ShaderError::Unsupported {
                    what: "a per-lane descriptor written where some lanes may keep the old one",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_buffer_descriptor_joined_per_lane_is_traced_on_the_paths_of_lanes_that_load() {
        let words = WATERFALL_JOINED_BUFFER_WAVE32;
        let r = waterfall(words, false).unwrap();
        let buffer = r
            .descriptors
            .iter()
            .find(|d| matches!(d.source, Source::LanePhi { .. }))
            .unwrap();
        let Source::LanePhi { block, args } = &buffer.source else {
            unreachable!()
        };
        assert_eq!(*block, block_at(words, 0x78));
        let [
            Some(Source::LanePhi { args: flow, .. }),
            Some(Source::LanePhi {
                block: inner,
                args: nested,
            }),
        ] = &args[..]
        else {
            panic!("{args:?}")
        };
        let [None, Some(then)] = &flow[..] else {
            panic!("{flow:?}")
        };
        assert_eq!(table_offset(&r, then), 0);
        assert_eq!(*inner, block_at(words, 0x74));
        let [None, Some(other)] = &nested[..] else {
            panic!("{nested:?}")
        };
        assert_eq!(table_offset(&r, other), 0x20);
        assert_eq!(r.waterfall.compares.len(), 2);
        assert_eq!(r.waterfall.reads.len(), 4);
    }
}
