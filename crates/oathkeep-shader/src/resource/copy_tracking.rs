use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Reg, Value};

use crate::gfx10::NULL;
use super::{MAX_COPIES, Site, Term, Tracer};

impl<'a> Tracer<'a> {
    pub(super) fn scalar_copies(&self, v: Value, out: &mut Vec<Term>) -> bool {
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
            // An entry value, or a number some instruction computed.
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
            // Lanes outside EXEC keep the old value.
            if let Some(Operand::Vector { reg, .. }) = f.dst {
                work.extend(inst.read(Reg::V(reg)));
            }
        }
        true
    }

    /// The load at `site` has a constant offset and takes its base from a
    /// root parameter's SGPR through moves alone: a value that exists
    /// wherever the descriptor is used.
    pub(super) fn base_from_entry(&self, site: Site) -> bool {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Moved {
    Const(u32),
    Value(Value),
}

pub(super) fn plain_lane(v: &crate::gfx10::Valu) -> bool {
    v.abs == 0
        && v.neg == 0
        && v.opsel == 0
        && !v.clamp
        && v.omod == 0
        && v.sdwa.is_none()
        && v.dpp.is_none()
}


impl Tracer<'_> {
    /// For `v_readlane_b32` of a constant lane: the SGPR spilled to that
    /// lane, found by following the VGPR's `v_writelane_b32` writes.
    pub(super) fn restored(&self, inst: &Inst, block: Option<usize>) -> Option<Moved> {
        let Fields::Valu(v) = &inst.fields else {
            return None;
        };
        if inst.name() != "v_readlane_b32" || !plain_lane(v) {
            return None;
        }
        let [Operand::Vector { reg, .. }, lane] = v.src[..] else {
            return None;
        };
        let lane = lane_of(lane)?;
        if lane >= if self.p.wave64 { 64 } else { 32 } {
            return None;
        }
        self.spilled(inst.read(Reg::V(reg))?, lane, block, &mut Vec::new())
            .ok()
            .flatten()
    }

    /// What lane `lane` of VGPR value `v` holds: `Ok(None)` for a phi in
    /// `seen` (already being resolved), `Err` if not a spilled SGPR.
    pub(super) fn spilled(
        &self,
        v: Value,
        lane: u32,
        block: Option<usize>,
        seen: &mut Vec<Value>,
    ) -> Result<Option<Moved>, ()> {
        if let Some((b, i)) = self.phis[v.0 as usize] {
            if block.is_some() {
                return Err(());
            }
            if seen.contains(&v) {
                return Ok(None);
            }
            seen.push(v);
            let mut found = None;
            for &a in &self.p.blocks[b].phis[i].args {
                if let Some(m) = self.spilled(a, lane, block, seen)? {
                    if found.is_some_and(|f| f != m) {
                        return Err(());
                    }
                    found = Some(m);
                }
            }
            return Ok(found);
        }
        let site = self.defs[v.0 as usize].ok_or(())?;
        if block.is_some_and(|b| site.0 != b) {
            return Err(());
        }
        let inst = self.inst(site);
        let Fields::Valu(w) = &inst.fields else {
            return Err(());
        };
        let (Some(Operand::Vector { reg, .. }), [value, at]) = (w.dst, &w.src[..]) else {
            return Err(());
        };
        if inst.name() != "v_writelane_b32" || !plain_lane(w) {
            return Err(());
        }
        let written = lane_of(*at).ok_or(())?;
        if written >= if self.p.wave64 { 64 } else { 32 } {
            return Err(());
        }
        if written != lane {
            return self.spilled(inst.read(Reg::V(reg)).ok_or(())?, lane, block, seen);
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

/// A lane select given as a constant.

pub(super) fn lane_of(op: Operand) -> Option<u32> {
    match op {
        Operand::Const(c) => inline(c),
        Operand::Literal(x) => Some(x),
        _ => None,
    }
}

/// What dword `k` of a move's result copies.
pub(super) fn copy_source(inst: &Inst, k: usize) -> Option<Moved> {
    let Fields::Sop { src, imm, .. } = &inst.fields else {
        return None;
    };
    if inst.name() == "s_movk_i32" {
        return Some(Moved::Const(i32::from((*imm)? as i16) as u32));
    }
    // A mask of S0 bits from bit S1, as the upscaler's compiler makes an
    // immediate sampler's word.
    if let ("s_bfm_b32", 0, [Operand::Const(w), Operand::Const(at)]) = (inst.name(), k, &src[..]) {
        let (w, at) = (inline(*w)? & 31, inline(*at)? & 31);
        return Some(Moved::Const(((1u32 << w) - 1) << at));
    }
    let src = src.first()?;
    match (inst.name(), *src) {
        ("s_mov_b32" | "s_mov_b64", Operand::Scalar { code, .. }) if code != NULL => {
            inst.read(Reg::S(code + k as u8)).map(Moved::Value)
        }
        ("s_mov_b32" | "s_mov_b64", Operand::Scalar { .. }) => Some(Moved::Const(0)),
        ("s_mov_b32", Operand::Literal(x)) => Some(Moved::Const(x)),
        ("s_mov_b32", Operand::Const(c)) => inline(c).map(Moved::Const),
        // shortcut: only the one-operand ways the compiler makes a constant
        // that has no inline form; fold more if descriptors need it.
        ("s_brev_b32", Operand::Const(c)) => inline(c).map(|x| Moved::Const(x.reverse_bits())),
        ("s_not_b32", Operand::Const(c)) => inline(c).map(|x| Moved::Const(!x)),
        // shortcut: a literal below 2^31, whose high dword is 0 whether the
        // hardware extends it with zeros or with its sign.
        ("s_mov_b64", Operand::Literal(x)) if x >> 31 == 0 => {
            Some(Moved::Const(if k == 0 { x } else { 0 }))
        }
        // 64-bit inline integers are sign-extended.
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

/// The 32-bit value of an inline constant operand.
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

