use std::collections::HashMap;

use crate::gfx10::{Fields, NULL, Operand};
use crate::io::{Input, Io};
use crate::ir::{EXEC, Inst, Reg, Value};
use crate::structure::{Atom, Bool, Structured};

/// The mode of a group barrier the translation adds (host waves meeting):
/// a wait for the group with a shared-memory fence; the fence alone with
/// `OATHKEEP_GROUP_SYNC=0` ([`crate::emit::group_sync`]).
pub(super) fn meeting_mode() -> u32 {
    if crate::emit::group_sync() == 0 { 8 } else { 1 | 8 }
}

/// Lightweight expression of a thread's in-group coordinates (`SV_GroupThreadID`),
/// evaluated across concrete threads in a 32-lane half or 64-lane console wave to
/// prove that a region or branch can only be active in a single half or single wave.
#[derive(Clone)]
enum TidExpr {
    Const(u32),
    Comp(u8),
    Add(Box<TidExpr>, Box<TidExpr>),
    Sub(Box<TidExpr>, Box<TidExpr>),
    And(Box<TidExpr>, Box<TidExpr>),
    Or(Box<TidExpr>, Box<TidExpr>),
    Xor(Box<TidExpr>, Box<TidExpr>),
    Shl(Box<TidExpr>, Box<TidExpr>),
    LShr(Box<TidExpr>, Box<TidExpr>),
    Mul(Box<TidExpr>, Box<TidExpr>),
}

impl TidExpr {
    fn eval(&self, x: u32, y: u32, z: u32) -> u32 {
        match self {
            TidExpr::Const(k) => *k,
            TidExpr::Comp(0) => x,
            TidExpr::Comp(1) => y,
            TidExpr::Comp(_) => z,
            TidExpr::Add(a, b) => a.eval(x, y, z).wrapping_add(b.eval(x, y, z)),
            TidExpr::Sub(a, b) => a.eval(x, y, z).wrapping_sub(b.eval(x, y, z)),
            TidExpr::And(a, b) => a.eval(x, y, z) & b.eval(x, y, z),
            TidExpr::Or(a, b) => a.eval(x, y, z) | b.eval(x, y, z),
            TidExpr::Xor(a, b) => a.eval(x, y, z) ^ b.eval(x, y, z),
            TidExpr::Shl(a, b) => a.eval(x, y, z).wrapping_shl(b.eval(x, y, z) & 31),
            TidExpr::LShr(a, b) => a.eval(x, y, z).wrapping_shr(b.eval(x, y, z) & 31),
            TidExpr::Mul(a, b) => a.eval(x, y, z).wrapping_mul(b.eval(x, y, z)),
        }
    }
}

#[derive(Clone, Copy)]
enum TidCmp {
    Eq,
    Ne,
    LtU,
    LeU,
    GtU,
    GeU,
    LtI,
    LeI,
    GtI,
    GeI,
}

impl TidCmp {
    fn parse(name: &str) -> Option<Self> {
        let rest = name
            .strip_prefix("v_cmpx_")
            .or_else(|| name.strip_prefix("v_cmp_"))?;
        Some(match rest {
            "eq_u32" | "eq_i32" => TidCmp::Eq,
            "ne_u32" | "ne_i32" | "lg_u32" | "lg_i32" => TidCmp::Ne,
            "lt_u32" => TidCmp::LtU,
            "le_u32" => TidCmp::LeU,
            "gt_u32" => TidCmp::GtU,
            "ge_u32" => TidCmp::GeU,
            "lt_i32" => TidCmp::LtI,
            "le_i32" => TidCmp::LeI,
            "gt_i32" => TidCmp::GtI,
            "ge_i32" => TidCmp::GeI,
            _ => return None,
        })
    }

    fn eval(self, a: u32, b: u32) -> bool {
        match self {
            TidCmp::Eq => a == b,
            TidCmp::Ne => a != b,
            TidCmp::LtU => a < b,
            TidCmp::LeU => a <= b,
            TidCmp::GtU => a > b,
            TidCmp::GeU => a >= b,
            TidCmp::LtI => (a as i32) < (b as i32),
            TidCmp::LeI => (a as i32) <= (b as i32),
            TidCmp::GtI => (a as i32) > (b as i32),
            TidCmp::GeI => (a as i32) >= (b as i32),
        }
    }
}

/// Abstract evaluator of lane BDD conditions over thread chunks of a compute
/// thread group (`SV_GroupThreadID` comparisons only; unknown atoms are `(true, true)`).
pub(super) struct ThreadModel {
    threads: [u32; 3],
    group: u32,
    cmps: HashMap<Value, (TidCmp, TidExpr, TidExpr)>,
}

impl ThreadModel {
    pub(super) fn new(p: &crate::ir::Program, io: &Io, threads: [u32; 3]) -> Self {
        let group = threads.iter().product::<u32>();
        let mut exprs: HashMap<Value, TidExpr> = HashMap::new();
        for &(v, inp) in &io.inputs {
            if let Input::ThreadIdInGroup(c) = inp {
                exprs.insert(v, TidExpr::Comp(c));
            }
        }
        let mut cmps = HashMap::new();
        if group == 0 || group > 1024 {
            return ThreadModel { threads, group, cmps };
        }
        let op_expr = |inst: &Inst, op: &Operand, exprs: &HashMap<Value, TidExpr>| -> Option<TidExpr> {
            match *op {
                Operand::Vector { reg, .. } => exprs.get(&inst.read(Reg::V(reg))?).cloned(),
                Operand::Scalar { code, .. } if code != NULL => {
                    exprs.get(&inst.read(Reg::S(code))?).cloned()
                }
                Operand::Const(c) => crate::resource::inline(c).map(TidExpr::Const),
                Operand::Literal(k) => Some(TidExpr::Const(k)),
                _ => None,
            }
        };
        for &b in &crate::ir::reverse_postorder(&p.blocks) {
            for inst in &p.blocks[b].insts {
                let name = inst.name();
                if let Some(cmp) = TidCmp::parse(name)
                    && let Fields::Valu(v) = &inst.fields
                    && v.src.len() >= 2
                    && let (Some(a), Some(b_ex)) = (
                        op_expr(inst, &v.src[0], &exprs),
                        op_expr(inst, &v.src[1], &exprs),
                    )
                {
                    let mask = if name.starts_with("v_cmpx") {
                        inst.write(Reg::S(EXEC))
                    } else if let Some(Operand::Scalar { code, .. }) = v.dst {
                        inst.write(Reg::S(code))
                    } else {
                        None
                    };
                    if let Some(m) = mask {
                        cmps.insert(m, (cmp, a, b_ex));
                    }
                    continue;
                }
                let src: &[Operand] = match &inst.fields {
                    Fields::Valu(v) => &v.src,
                    Fields::Sop { src, .. } => src,
                    _ => continue,
                };
                let dst = match &inst.fields {
                    Fields::Valu(v) => match v.dst {
                        Some(Operand::Vector { reg, .. }) => inst.write(Reg::V(reg)),
                        _ => None,
                    },
                    Fields::Sop { dst, .. } => match dst {
                        Some(Operand::Scalar { code, .. }) => inst.write(Reg::S(*code)),
                        _ => None,
                    },
                    _ => None,
                };
                let Some(dst) = dst else { continue };
                let out = (|| -> Option<TidExpr> {
                    match (name, src) {
                        ("v_mov_b32" | "s_mov_b32", [a]) => op_expr(inst, a, &exprs),
                        ("v_add_nc_u32" | "s_add_u32" | "s_add_i32", [a, b_op]) => Some(TidExpr::Add(
                            Box::new(op_expr(inst, a, &exprs)?),
                            Box::new(op_expr(inst, b_op, &exprs)?),
                        )),
                        ("v_sub_nc_u32" | "s_sub_u32" | "s_sub_i32", [a, b_op]) => Some(TidExpr::Sub(
                            Box::new(op_expr(inst, a, &exprs)?),
                            Box::new(op_expr(inst, b_op, &exprs)?),
                        )),
                        ("v_subrev_nc_u32", [a, b_op]) => Some(TidExpr::Sub(
                            Box::new(op_expr(inst, b_op, &exprs)?),
                            Box::new(op_expr(inst, a, &exprs)?),
                        )),
                        ("v_and_b32" | "s_and_b32", [a, b_op]) => Some(TidExpr::And(
                            Box::new(op_expr(inst, a, &exprs)?),
                            Box::new(op_expr(inst, b_op, &exprs)?),
                        )),
                        ("v_or_b32" | "s_or_b32", [a, b_op]) => Some(TidExpr::Or(
                            Box::new(op_expr(inst, a, &exprs)?),
                            Box::new(op_expr(inst, b_op, &exprs)?),
                        )),
                        ("v_xor_b32" | "s_xor_b32", [a, b_op]) => Some(TidExpr::Xor(
                            Box::new(op_expr(inst, a, &exprs)?),
                            Box::new(op_expr(inst, b_op, &exprs)?),
                        )),
                        ("v_lshrrev_b32", [a, b_op]) => Some(TidExpr::LShr(
                            Box::new(op_expr(inst, b_op, &exprs)?),
                            Box::new(op_expr(inst, a, &exprs)?),
                        )),
                        ("s_lshr_b32", [a, b_op]) => Some(TidExpr::LShr(
                            Box::new(op_expr(inst, a, &exprs)?),
                            Box::new(op_expr(inst, b_op, &exprs)?),
                        )),
                        ("v_lshlrev_b32", [a, b_op]) => Some(TidExpr::Shl(
                            Box::new(op_expr(inst, b_op, &exprs)?),
                            Box::new(op_expr(inst, a, &exprs)?),
                        )),
                        ("s_lshl_b32", [a, b_op]) => Some(TidExpr::Shl(
                            Box::new(op_expr(inst, a, &exprs)?),
                            Box::new(op_expr(inst, b_op, &exprs)?),
                        )),
                        ("v_mul_lo_u32" | "v_mul_u32_u24" | "s_mul_i32", [a, b_op]) => {
                            Some(TidExpr::Mul(
                                Box::new(op_expr(inst, a, &exprs)?),
                                Box::new(op_expr(inst, b_op, &exprs)?),
                            ))
                        }
                        _ => None,
                    }
                })();
                if let Some(ex) = out {
                    exprs.insert(dst, ex);
                }
            }
        }
        ThreadModel { threads, group, cmps }
    }

    /// Evaluates BDD `cond` over flat threads `start..end` (`end <= group`):
    /// returns `(can_be_false, can_be_true)` in the 4-point bilattice.
    fn eval_chunk(&self, st: &Structured, cond: Bool, start: u32, end: u32) -> (bool, bool) {
        let tx = self.threads[0].max(1);
        let ty = self.threads[1].max(1);
        let end = end.min(self.group);
        if start >= end {
            return (true, false);
        }
        let eval_lane = |v: Value| -> (bool, bool) {
            let Some((cmp, a, b)) = self.cmps.get(&v) else {
                return (true, true);
            };
            let (mut f, mut t) = (false, false);
            for flat in start..end {
                let x = flat % tx;
                let y = (flat / tx) % ty;
                let z = flat / (tx * ty);
                if cmp.eval(a.eval(x, y, z), b.eval(x, y, z)) {
                    t = true;
                } else {
                    f = true;
                }
                if f && t {
                    break;
                }
            }
            (f, t)
        };
        let mut phi_vals: HashMap<Value, (bool, bool)> = st
            .phi_masks
            .keys()
            .map(|&id| (id, (false, false)))
            .collect();
        fn eval_bdd(
            st: &Structured,
            b: Bool,
            eval_lane: &impl Fn(Value) -> (bool, bool),
            phi_vals: &HashMap<Value, (bool, bool)>,
            memo: &mut HashMap<Bool, (bool, bool)>,
        ) -> (bool, bool) {
            if b == Bool::FALSE {
                return (true, false);
            }
            if b == Bool::TRUE {
                return (false, true);
            }
            if let Some(&r) = memo.get(&b) {
                return r;
            }
            let Some((atom, lo, hi)) = st.bools.node(b) else {
                return (true, true);
            };
            let av = match atom {
                Atom::Lane(v) => eval_lane(v),
                Atom::Phi(id) => phi_vals.get(&id).copied().unwrap_or((true, true)),
                _ => (true, true),
            };
            let mut r = (false, false);
            if av.0 {
                let l = eval_bdd(st, lo, eval_lane, phi_vals, memo);
                r = (r.0 || l.0, r.1 || l.1);
            }
            if av.1 {
                let h = eval_bdd(st, hi, eval_lane, phi_vals, memo);
                r = (r.0 || h.0, r.1 || h.1);
            }
            memo.insert(b, r);
            r
        }
        if !phi_vals.is_empty() {
            let max_rounds = 2 * phi_vals.len() + 2;
            let mut settled = false;
            for _ in 0..max_rounds {
                let mut changed = false;
                let mut memo = HashMap::new();
                let mut next = phi_vals.clone();
                for (&id, args) in &st.phi_masks {
                    let mut r = (false, false);
                    for &arg in args {
                        let v = eval_bdd(st, arg, &eval_lane, &phi_vals, &mut memo);
                        r = (r.0 || v.0, r.1 || v.1);
                    }
                    if next.get(&id).copied() != Some(r) {
                        next.insert(id, r);
                        changed = true;
                    }
                }
                phi_vals = next;
                if !changed {
                    settled = true;
                    break;
                }
            }
            if !settled {
                for v in phi_vals.values_mut() {
                    *v = (true, true);
                }
            }
        }
        let mut memo = HashMap::new();
        eval_bdd(st, cond, &eval_lane, &phi_vals, &mut memo)
    }

    /// True if within every 64-lane console wave, at most one 32-lane half can
    /// have `cond` true (so the two halves of a console wave never contend).
    pub(super) fn single_half_per_wave(&self, st: &Structured, cond: Bool) -> bool {
        if self.group == 0 || self.group > 1024 {
            return false;
        }
        let waves = self.group.div_ceil(64);
        for w in 0..waves {
            let base = w * 64;
            let lo = self.eval_chunk(st, cond, base, base + 32).1;
            let hi = self.eval_chunk(st, cond, base + 32, base + 64).1;
            if lo && hi {
                return false;
            }
        }
        true
    }

    /// True if at most one 64-lane console wave in the entire thread group can
    /// have `cond` true.
    pub(super) fn single_console_wave(&self, st: &Structured, cond: Bool) -> bool {
        if self.group == 0 || self.group > 1024 {
            return false;
        }
        let waves = self.group.div_ceil(64);
        let mut active = 0u32;
        for w in 0..waves {
            let base = w * 64;
            if self.eval_chunk(st, cond, base, base + 64).1 {
                active += 1;
                if active > 1 {
                    return false;
                }
            }
        }
        true
    }
}
