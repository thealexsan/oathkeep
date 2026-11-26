
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{Inst, Reg, Value};

use super::{
    Known, MAX_JOINS, Site, Source, Term, Tracer, inline,
};

impl<'a> Tracer<'a> {
    pub(super) fn select(
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
        // Each level doubles the sources; selects count like joins (a
        // marker no block index reaches).
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

    /// For `v` written by `s_cselect_b32`/`_b64` or `s_cmov_b32`/`_b64`:
    /// the SCC it reads and the dword it is when SCC is set, then clear.
    pub(super) fn picked(&self, v: Value) -> Option<(Value, Term, Term)> {
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
        // A dword of a 32- or 64-bit operand: 64-bit inline integers are
        // sign-extended; 64-bit literals and float constants are not
        // followed.
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

    /// `v` as some other dword with constant bits set.
    pub(super) fn set_bits(&self, v: Value) -> Option<(Term, u32)> {
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

    /// The words of a buffer descriptor that `words` makes raw, as the
    /// FidelityFX upscaler's compiler does before byte accesses: the stride
    /// (dword 1 bits 16-29) cleared, the record count times max(stride, 1),
    /// out-of-bounds checking by bytes (dword 3 bits 28-29 set). The bounds
    /// are then the view's size in bytes either way.
    pub(super) fn made_raw(&self, words: &[Term]) -> Option<[Term; 4]> {
        let [w0, Term::Value(w1), Term::Value(w2), Term::Value(w3)] = *words else {
            return None;
        };
        // `x op c` with `c` either side.
        let with = |v: Value, name: &str, c: u32| match self.sop2(v, &[name])?.1 {
            [x, Term::Const(k)] | [Term::Const(k), x] if k == c => Some(x),
            _ => None,
        };
        let d1 = with(w1, "s_and_b32", 0xC000_FFFF)?;
        let Term::Value(m3) = with(w3, "s_or_b32", 0x3000_0000)? else {
            return None;
        };
        let d3 = with(m3, "s_and_b32", 0xCFFF_FFFF)?;
        let [a, b] = self.sop2(w2, &["s_mul_i32"])?.1;
        let max = |t: Term| match t {
            Term::Value(s) => with(s, "s_max_u32", 1),
            Term::Const(_) => None,
        };
        let (d2, field) = match (max(a), max(b)) {
            (_, Some(f)) => (a, f),
            (Some(f), None) => (b, f),
            (None, None) => return None,
        };
        let Term::Value(field) = field else {
            return None;
        };
        // s_bfe_u32: offset 16, width 14.
        let (_, [x, Term::Const(0x000E_0010)]) = self.sop2(field, &["s_bfe_u32"])? else {
            return None;
        };
        (x == d1).then_some([w0, d1, d2, d3])
    }


    /// For a `v_readfirstlane_b32` result: the VGPR value it reads.
    pub(super) fn first_lane_source(&self, v: Value) -> Option<Value> {
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

    /// The `v_readfirstlane_b32` result the scalar load at `load` adds to
    /// its address as a table offset (the console's waterfall,
    /// `s_load_dwordx8 .., s[4:5], s20`): `track` proves the waterfall from
    /// it, and lowering reads it per lane there.
    pub(super) fn first_lane_offset(&self, load: Site) -> Option<Value> {
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

    /// Whether the descriptor operand `op` of `inst` is one the code keeps
    /// per lane ([`Self::lane_copies`]).
    pub(super) fn kept_per_lane(&self, inst: &Inst, op: &Operand) -> bool {
        let Operand::Scalar { code, dwords } = *op else {
            return false;
        };
        (0..dwords)
            .map(|k| inst.read(Reg::S(code + k)).map(|v| self.origin(v)))
            .collect::<Option<Vec<_>>>()
            .is_some_and(|words| self.lane_copies(&words).is_some())
    }

    /// The `v_readfirstlane_b32` results the descriptor operand `op` of
    /// `inst` is, when every dword is one.
    pub(super) fn first_lane_words(&self, inst: &Inst, op: &Operand) -> Option<Vec<Value>> {
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

    /// `t` through VGPR copies (of VGPRs or of scalars) and copies.
    pub(super) fn lane_term(&self, mut t: Term) -> Term {
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

    /// Whether `t` is a value each lane holds its own of (one in VGPRs).
    pub(super) fn varies(&self, t: Term) -> bool {
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

    /// [`Self::lane_term`], where every lane in `known` runs each VGPR copy
    /// it follows.
    pub(super) fn lane_value(&self, mut t: Term, known: &Known) -> Result<Term, &'static str> {
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

    /// One copy of [`Self::vorigin`]: what the `v_mov_b32` writing `v`
    /// reads, or `v`'s origin.
    pub(super) fn vstep(&self, v: Value) -> Term {
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

    /// A VALU operand as `inst` reads it: a VGPR's value itself, other
    /// operands' origins.
    pub(super) fn raw(&self, inst: &Inst, op: &Operand) -> Option<Term> {
        match *op {
            Operand::Vector { reg, .. } => Some(Term::Value(inst.read(Reg::V(reg))?)),
            op => self.term(inst, &op),
        }
    }

    /// Fails unless every lane in `known` that reaches the vector
    /// instruction at `site` runs it: one with its EXEC bit clear there
    /// keeps what the registers held before.
    pub(super) fn runs(&self, site: Site, known: &Known) -> Result<(), &'static str> {
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

    /// The source of descriptor dwords each lane holds its own of (a
    /// waterfall loop reads them from the first active lane): the same in
    /// every lane, loaded per lane from memory a heap address points to, or
    /// picked per lane between such sources, by a lane mask or by the path
    /// the lane took. `known` holds in every lane that accesses them.
    pub(super) fn per_lane(
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

    /// Dwords of `global_load_dword*` that line up: `first..` of one load,
    /// then the bytes that follow (an image descriptor takes two
    /// `global_load_dwordx4`).
    pub(super) fn fetched(&self, words: &[Term], known: &Known) -> Result<Option<Source>, &'static str> {
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
}
