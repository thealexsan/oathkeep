use crate::ShaderError;
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Inst, Reg, VCC};

use super::{Atom, Bool, Key, Lanes, MAX_ROUNDS, TOO_LARGE, bools::Map};

impl<'a> Lanes<'a> {
    /// Records the masks `inst` defines; `exec` is EXEC there, if known.
    pub(super) fn step(&mut self, inst: &Inst, exec: Option<Bool>) {
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
                        // EXEC itself holds the lanes that run here.
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
                // `s_cmp_eq/lg` of a mask: "no lane differs" per lane.
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
                // `s_bcnt1` of a mask every running lane sees alike (EXEC under a scalar
                // condition, or EXEC itself): its SCC, "some lane is set", is each lane's own
                // bit. A branch on it then reads no ballot.
                if takes_mask
                    && name.starts_with("s_bcnt1_i32_b")
                    && let Some(scc) = inst.write(Reg::Scc)
                    && let [a] = &src[..]
                    && let Some((m, true)) = self.operand(inst, a)
                {
                    let e = exec.unwrap_or(Bool::TRUE);
                    let own = self.b.given(m, e);
                    if self.b.implies(m, e)
                        && self.b.support(own).iter().all(|a| matches!(a, Atom::Uniform(_)))
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
                // A select reads a mask nothing else may: a join of compare
                // results is resolved here, as for EXEC. A carry-in is only
                // recorded for `finish`: resolving its join would turn a
                // refusal into a per-lane phi, wrong after a divergent branch.
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

    /// The per-lane condition under which a block's closing branch is taken.
    /// How the console decides the branch `inst` on a lane mask, in terms of the lanes' own
    /// `when` ([`Self::branch`]): taken when some lane's holds (`true`) or when every lane's
    /// does (`false`). `execnz`/`vccnz` ask for some set bit, `execz`/`vccz` for none; SCC
    /// of a mask operation is "some lane's bit is set", SCC of `s_cmp_eq` of two masks is
    /// "every lane's is" ([`Self::scc_every`]), and `scc0` branches on the opposite. None
    /// for a branch on a scalar condition.
    pub(super) fn wave_kind(&self, inst: &Inst) -> Option<bool> {
        match inst.name() {
            "s_cbranch_execnz" | "s_cbranch_vccnz" => Some(true),
            "s_cbranch_execz" | "s_cbranch_vccz" => Some(false),
            n @ ("s_cbranch_scc0" | "s_cbranch_scc1") => {
                let scc = inst.read(Reg::Scc)?;
                self.scc_lane.get(&scc)?;
                // set: SCC is 1 when some lane's bit is set (every lane's, for a compare)
                let every = self.scc_every.contains(&scc);
                Some((n == "s_cbranch_scc1") != every)
            }
            _ => None,
        }
    }

    pub(super) fn branch(&mut self, inst: &Inst) -> Bool {
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

    /// What a lane knows on entering each block, to a fixpoint. A loop
    /// header forgets what the back edge says about atoms the loop redefines.
    pub(super) fn paths(&mut self) -> Result<Vec<Bool>, ShaderError> {
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
