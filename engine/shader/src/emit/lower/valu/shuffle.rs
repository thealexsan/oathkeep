use super::super::*;

impl<'a> Lower<'a> {
    pub(super) fn valu_shuffle(
        &mut self,
        inst: &Inst,
        v: &crate::gfx10::Valu,
        name: &'static str,
        guard: Option<V>,
    ) -> Option<Result<(), Why>> {
        if !matches!(
            name,
            "v_cndmask_b32"
                | "v_add_co_u32"
                | "v_sub_co_u32"
                | "v_subrev_co_u32"
                | "v_add_co_ci_u32"
                | "v_sub_co_ci_u32"
                | "v_subrev_co_ci_u32"
                | "v_permlane16_b32"
                | "v_permlanex16_b32"
                | "v_readlane_b32"
                | "v_writelane_b32"
                | "v_readfirstlane_b32"
        ) {
            return None;
        }
        Some(self.valu_shuffle_inner(inst, v, name, guard))
    }

    fn valu_shuffle_inner(
        &mut self,
        inst: &Inst,
        v: &crate::gfx10::Valu,
        name: &'static str,
        guard: Option<V>,
    ) -> Result<(), Why> {
        let c = self.cur;
        let f = |l: &mut Self, k| l.fsrc(inst, k);
        let s = |l: &mut Self, k| l.src(inst, k);
        let float = |l: &mut Self, r: V| l.write_f(inst, r, guard);
        let int = |l: &mut Self, r: V| l.write_i(inst, r, guard);
        match name {
            "v_cndmask_b32" => {
                let mask = match v.src.get(2) {
                    Some(&Operand::Scalar { code, .. }) => inst.read(Reg::S(code)),
                    _ => None,
                };
                const UNTRACKED: Why = "a select on an untracked mask";
                let cond = match mask.and_then(|m| self.x.st.masks.get(&m).copied()) {
                    Some(m) => self.boolean(m, c)?,
                    // A mask the code holds as a number (a constant bit
                    // pattern: every other lane, every other pair...):
                    // this lane's bit of it. Exact where host lanes are
                    // the console's.
                    None if self.mask_data == MaskData::Console => {
                        // Each host wave taken as a wave of its own has no
                        // console lane numbers to pick the bits by.
                        if self.local {
                            return Err("a constant lane mask in a half-wave");
                        }
                        let m = mask.ok_or(UNTRACKED)?;
                        let lo = self.i32_of(m).map_err(|_| UNTRACKED)?;
                        // wave64: the register after it holds lanes 32-63.
                        let high = match v.src.get(2) {
                            Some(&Operand::Scalar { code, .. }) if self.x.native.wave64 => {
                                Some(inst.read(Reg::S(code + 1)).ok_or(UNTRACKED)?)
                            }
                            _ => None,
                        };
                        let hi = match high {
                            Some(h) => Some(self.i32_of(h).map_err(|_| UNTRACKED)?),
                            None => None,
                        };
                        self.cross_lane(false)?;
                        self.lane_bit(c, lo, hi)
                    }
                    None => return Err(UNTRACKED),
                };
                if v.abs != 0 || v.neg != 0 {
                    let (a, bb) = (f(self, 0)?, f(self, 1)?);
                    let r = self.s.m.select(c, cond, bb, a);
                    return float(self, r);
                }
                let (a, bb) = (s(self, 0)?, s(self, 1)?);
                let r = self.s.m.select(c, cond, bb, a);
                int(self, r)
            }
            "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32" | "v_add_co_ci_u32"
            | "v_sub_co_ci_u32" | "v_subrev_co_ci_u32" => {
                // The carry (borrow) out is a lane mask: this lane's bit.
                let (mut a, mut bb) = (s(self, 0)?, s(self, 1)?);
                if name.contains("subrev") {
                    std::mem::swap(&mut a, &mut bb);
                }
                let cin = if name.ends_with("_ci_u32") {
                    let m = match v.src.get(2) {
                        Some(&Operand::Scalar { code, .. }) => inst.read(Reg::S(code)),
                        _ => None,
                    };
                    let m = m
                        .and_then(|m| self.x.st.masks.get(&m).copied())
                        .ok_or("a carry-in from an untracked mask")?;
                    Some(self.boolean(m, c)?)
                } else {
                    None
                };
                let (r, carry) = self.add_sub_carry(c, name.starts_with("v_add"), a, bb, cin);
                int(self, r)?;
                if let Some(Operand::Scalar { code, .. }) = v.carry
                    && let Some(w) = inst.write(Reg::S(code))
                {
                    self.lane_bits.insert(w, carry);
                }
                for &(_, w) in &inst.writes {
                    self.vals
                        .entry(w)
                        .or_insert(Val::Missing("a lane mask read as data"));
                }
                Ok(())
            }
            "v_permlane16_b32" | "v_permlanex16_b32" => {
                // RDNA2 ISA 12.12: each nibble of {src2, src1} selects a lane
                // in this row of 16, or the other row of the same 32-lane half.
                // OP_SEL[0:1] are fetch-inactive and bound-control, not half selects.
                if v.abs != 0 || v.neg != 0 || v.opsel & !3 != 0 {
                    return Err("permlane input modifiers");
                }
                self.cross_lane(false)?;
                let fetch_inactive = v.opsel & 1 != 0;
                if fetch_inactive
                    && !all_lane_blocks(self.x.st).is_some_and(|all| all[self.cur_ir])
                {
                    // DXIL cannot read the last VGPR value of a host invocation
                    // that has left through a lane branch. A guard keeps it here.
                    return Err("a permlane fetch from lanes outside the host branch");
                }
                let (x, lo, hi) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let lane = self.lane_index();
                let (k7, k8, k15, k16, k31, k32, k4, one, zero) = (
                    self.s.m.i32(7),
                    self.s.m.i32(8),
                    self.s.m.i32(15),
                    self.s.m.i32(16),
                    self.s.m.i32(31),
                    self.s.m.i32(32),
                    self.s.m.i32(4),
                    self.s.m.i32(1),
                    self.s.m.i32(0),
                );
                let pos = self.s.m.bin(c, Bin::And, lane, k15);
                let lower = self.s.m.cmp(c, pred::IULT, pos, k8);
                let packed = self.s.m.select(c, lower, lo, hi);
                let shift = self.s.m.bin(c, Bin::And, pos, k7);
                let shift = self.s.m.bin(c, Bin::Mul, shift, k4);
                let pick = self.s.m.bin(c, Bin::LShr, packed, shift);
                let pick = self.s.m.bin(c, Bin::And, pick, k15);
                let not15 = self.s.m.i32(!15);
                let mut row = self.s.m.bin(c, Bin::And, lane, not15);
                if name == "v_permlanex16_b32" {
                    row = self.s.m.bin(c, Bin::Xor, row, k16);
                }
                let src = self.s.m.bin(c, Bin::Or, row, pick);
                let r = if fetch_inactive {
                    self.read_lane(x, src)
                } else {
                    // Ballot the console EXEC guard as well as the host's active
                    // invocations. Never shuffle from an inactive host lane.
                    let enabled = guard.unwrap_or_else(|| self.s.m.bool(true));
                    let i32t = self.s.t.i32;
                    let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
                    let mask = self.s.op(
                        c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[enabled],
                    );
                    let lo = self.s.m.extract(c, mask, 0);
                    let hi = self.s.m.extract(c, mask, 1);
                    let lower = self.s.m.cmp(c, pred::IULT, src, k32);
                    let word = self.s.m.select(c, lower, lo, hi);
                    let bit = self.s.m.bin(c, Bin::And, src, k31);
                    let bit = self.s.m.bin(c, Bin::LShr, word, bit);
                    let bit = self.s.m.bin(c, Bin::And, bit, one);
                    let active = self.s.m.cmp(c, pred::NE, bit, zero);
                    let safe = self.s.m.select(c, active, src, lane);
                    let read = self.read_lane(x, safe);
                    let fallback = if v.opsel & 2 != 0 {
                        zero
                    } else {
                        let Some(Operand::Vector { reg, .. }) = v.dst else {
                            return Err("a permlane result outside a VGPR");
                        };
                        self.i32_of(inst.read(Reg::V(reg)).ok_or("a permlane old value")?)?
                    };
                    self.s.m.select(c, active, read, fallback)
                };
                int(self, r)
            }
            "v_readlane_b32" => {
                let (a, lane) = (s(self, 0)?, s(self, 1)?);
                // A computed lane in a pixel or vertex program is a host lane
                // (from a mask or `v_mbcnt`); a constant one is the console's.
                if self.mask_data == MaskData::Host && !matches!(lane, V::Const(_)) {
                    self.s.flags |= flags::WAVE_OPS;
                } else {
                    self.cross_lane(false)?;
                }
                if let Some(slot) = self.exchange.as_ref().map(|e| e.0) {
                    if self.local && self.local_any {
                        // this host wave's lane of that number (lanes 32..63 are 0..31 here)
                        let k31 = self.s.m.i32(31);
                        let here = self.s.m.bin(self.cur, Bin::And, lane, k31);
                        let r = self.read_lane(a, here);
                        return int(self, r);
                    }
                    if self.local {
                        return Err("a lane read in a half-wave");
                    }
                    if self.exchange.as_ref().is_some_and(|e| e.1) {
                        // a group of 32 or fewer: the lane is in this host wave
                        let r = self.read_lane(a, lane);
                        return int(self, r);
                    }
                    // the lane that holds it may be in the other host wave: it gives it through LDS
                    if !self.all_lanes(self.cur_ir) {
                        need(self.cur_ir);
                        return Err("a lane read where not every lane runs");
                    }
                    let cl = self.console_lane();
                    let is = self.s.m.cmp(self.cur, pred::EQ, cl, lane);
                    let k = self.exchange_pair(slot);
                    let at = self.begin_guarded(Some(is));
                    let lds = self.s.lds();
                    let ptr = self.s.m.element(self.cur, lds, k);
                    self.s.m.store(self.cur, ptr, a);
                    self.end_guarded(at, &[]);
                    let c2 = self.cur;
                    self.group_sync(c2);
                    let ptr = self.s.m.element(c2, lds, k);
                    let r = self.s.m.load(c2, ptr);
                    self.group_sync(c2);
                    return int(self, r);
                }
                let r = self.read_lane(a, lane);
                int(self, r)
            }
            "v_writelane_b32" => {
                // The lane given takes the scalar; the others keep their value.
                let (a, lane) = (s(self, 0)?, s(self, 1)?);
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("a lane write");
                };
                let old = match inst.read(Reg::V(reg)) {
                    Some(o) => self.i32_of(o)?,
                    None => self.s.m.i32(0),
                };
                self.cross_lane(false)?;
                if self.local && !self.local_any {
                    return Err("a lane write in a half-wave");
                }
                let me = self.console_lane();
                let is = self.s.m.cmp(c, pred::EQ, me, lane);
                let r = self.s.m.select(c, is, a, old);
                int(self, r)
            }
            "v_readfirstlane_b32"
                if inst
                    .writes
                    .first()
                    .is_some_and(|w| self.x.res.waterfall.reads.contains(&w.1)) =>
            {
                // A waterfall loop's read of a descriptor dword: the lane's own.
                let a = s(self, 0)?;
                int(self, a)
            }
            "v_readfirstlane_b32"
                if matches!(v.src.first(), Some(&Operand::Vector { reg, .. })
                    if inst.read(Reg::V(reg)).is_some_and(|x| self.own_bits.alike.contains(&x))) =>
            {
                // The bits a loop over a wave OR has still to visit (`own_bits`): every lane of
                // the host wave holds the same, so the first lane's is the lane's own.
                let a = s(self, 0)?;
                int(self, a)
            }
            "v_readfirstlane_b32" => {
                let a = s(self, 0)?;
                let (mine, any) = self.first_lane(a, guard);
                // two host waves that exchange: the console's first lane may be in the other
                // one, unless every lane holds the same value
                let alike = match v.src.first() {
                    Some(&Operand::Vector { reg, .. }) => {
                        inst.read(Reg::V(reg)).is_some_and(|x| self.uniform.contains(&x))
                    }
                    _ => true,
                };
                let halves = self.exchange.as_ref().is_some_and(|e| !e.1) && !self.local;
                let r = if halves && !alike {
                    if !self.all_lanes(self.cur_ir) {
                        // shortcut: `OATHKEEP_FIRSTLANE_OWN=1` takes the host wave's own
                        // first lane, exact when the value is the same in every lane of
                        // the console wave (the ray-tracing builder's scans read a sum of
                        // scalars this way, inside a loop whose exit is per console wave).
                        static OWN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                        if *OWN.get_or_init(|| std::env::var_os("OATHKEEP_FIRSTLANE_OWN").is_some_and(|v| v == "1")) {
                            return int(self, mine);
                        }
                        need(self.cur_ir);
                        return Err("a first-lane read where not every lane runs");
                    }
                    let c = self.cur;
                    if guard.is_none() && crate::emit::shader_opt() & crate::emit::bitcode::opt::FIRST != 0 {
                        // Every lane is in EXEC: the console's first lane is
                        // lane 0, which the lower half of every console wave
                        // has. No half needs to ask the other whether it has
                        // a lane (an exchange of the constant 1: two group
                        // barriers), and the lower half's value is the answer.
                        let (lo, _) = self.exchange_halves(c, mine);
                        lo
                    } else {
                        let i32t = self.s.t.i32;
                        let has = self.s.m.cast(c, Cast::ZExt, any, i32t);
                        let (lo_has, hi_has) = self.exchange_halves(c, has);
                        let (lo, hi) = self.exchange_halves(c, mine);
                        let z = self.s.m.i32(0);
                        let in_lo = self.s.m.cmp(c, pred::NE, lo_has, z);
                        let in_hi = self.s.m.cmp(c, pred::NE, hi_has, z);
                        // neither half has a lane in EXEC: the console reads lane 0
                        let hi = self.s.m.select(c, in_hi, hi, lo);
                        self.s.m.select(c, in_lo, lo, hi)
                    }
                } else {
                    mine
                };
                int(self, r)
            }
            _ => unreachable!(),
        }
    }
}
