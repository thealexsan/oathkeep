use super::*;

impl<'a> Lower<'a> {
    pub(in crate::emit::lower) fn exit(&mut self, b: usize) -> Result<(), Stop> {
        let offset = self.x.p.blocks[b]
            .insts
            .last()
            .map_or(self.x.p.blocks[b].offset, |i| i.machine.offset);
        let term = match self.x.st.exits[b] {
            LaneExit::Jump(t) if self.counters.contains_key(&t) && self.rpo[t] <= self.rpo[b] => {
                let ok = self.count_iteration(t, pred::IULT);
                let abort = self.abort_block();
                Term::CondBr(ok, self.block[t].expect("reachable"), abort)
            }
            LaneExit::Jump(t) => {
                self.enter_loop(b, t, self.cur);
                Term::Br(self.block[t].expect("reachable"))
            }
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } => {
                // A negated condition branches the other way round.
                let (cond, taken, not_taken) = match self.x.st.bools.node(when) {
                    _ if self.x.st.wide.contains(&b) => {
                        let c = self.wave_wide(b, when).map_err(|w| unsupported(offset, w))?;
                        (c, taken, not_taken)
                    }
                    _ if self.group_wide
                        && self.all_lanes(b)
                        && self.x.st.exit_exec[b] != Bool::TRUE
                        && self
                            .x
                            .st
                            .bools
                            .support(when)
                            .iter()
                            .all(|a| matches!(a, Atom::Uniform(_))) =>
                    {
                        let active = self
                            .boolean(self.x.st.exit_exec[b], self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        let raw = self
                            .boolean(when, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        let vote = self.s.m.bin(self.cur, Bin::And, active, raw);
                        let any = self.group_any(self.cur, vote);
                        let z = self.s.m.i32(0);
                        let c = self.s.m.cmp(self.cur, pred::NE, any, z);
                        (c, taken, not_taken)
                    }
                    Some((atom, Bool::TRUE, Bool::FALSE)) => {
                        let a = self
                            .atom(atom, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        (a, not_taken, taken)
                    }
                    _ => {
                        let c = self
                            .boolean(when, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        (c, taken, not_taken)
                    }
                };
                // A back edge is taken only below the iteration limit.
                let back =
                    |l: &Self, to: usize| l.counters.contains_key(&to) && l.rpo[to] <= l.rpo[b];
                // (No `xor` of a compare: a loop exit testing one hung the
                // AMD driver.)
                let cond = if back(self, taken) {
                    let ok = self.count_iteration(taken, pred::IULT);
                    self.s.m.bin(self.cur, Bin::And, cond, ok)
                } else if back(self, not_taken) {
                    let stop = self.count_iteration(not_taken, pred::IUGE);
                    self.s.m.bin(self.cur, Bin::Or, cond, stop)
                } else {
                    cond
                };
                // Critical edges get a block of their own, so loops have
                // dedicated exits and preheaders as DXC emits them.
                let (t, f) = (self.edge(b, taken), self.edge(b, not_taken));
                for (to, via) in [(taken, t), (not_taken, f)] {
                    if !back(self, to) {
                        // From an edge block, or from here.
                        let from = if Some(via) == self.block[to] {
                            self.cur
                        } else {
                            via
                        };
                        self.enter_loop(b, to, from);
                    }
                }
                Term::CondBr(cond, t, f)
            }
            LaneExit::End => {
                if self.s.pixel && !self.exported[b] && !self.has_done_export(b) {
                    // A lane leaving without its final export was killed.
                    let t = self.s.m.bool(true);
                    self.s
                        .op(self.cur, "discard", Attr::NoUnwind, self.s.t.void, 82, &[t]);
                }
                Term::Ret
            }
            LaneExit::Indirect => return Err(unsupported(offset, "an indirect jump")),
        };
        let _ = Exit::End;
        self.s.m.terminate(self.cur, term);
        Ok(())
    }

    /// A forward edge into loop header `to` (from DXIL block `via`) starts
    /// its count at 0.
    pub(in crate::emit::lower) fn enter_loop(&mut self, _from: usize, to: usize, via: usize) {
        if let Some((_, incoming)) = self.counters.get_mut(&to) {
            let z = self.s.m.i32(0);
            incoming.push((z, via));
        }
    }

    /// A back edge to header `to` (a back edge keeps its block, no
    /// critical-edge block): the count goes up; the result compares it with
    /// [`LOOP_LIMIT`] by `p` (below: go on; at or above: stop).
    pub(in crate::emit::lower) fn count_iteration(&mut self, to: usize, p: u8) -> V {
        let c = self.cur;
        let (phi, _) = self.counters[&to];
        let one = self.s.m.i32(1);
        let next = self.s.m.bin(c, Bin::Add, phi, one);
        self.s.m.tag(phi, Tag::Counter);
        self.s.m.tag(next, Tag::Counter);
        // Per stage unless the switch names one: a loop that never meets its
        // exit (a count read from a buffer the host has not got yet, say)
        // runs to the limit in every lane. A million iterations a pixel
        // hung the GPU for seconds a draw until Windows reset it
        // (PS_Textured_Eight, 2026-11-24, run 12).
        // shortcut: a pixel loop past 128 iterations, a vertex or mesh loop
        // past 256 or a compute loop past 4,096 is cut short. (They were
        // 1,024, 4,096 and 65,536: the opening cutscene then lost the card
        // 14-25 frames in, in a minimap or shadow mesh draw; with every
        // loop cut at 64 it ran 90 frames. Raise them again once shaders
        // stop reading data that is not there yet.)
        let limit = match crate::emit::loop_limit() {
            LOOP_LIMIT if self.s.pixel => 1 << 7,
            LOOP_LIMIT if self.s.mesh.is_some() || self.s.threads.is_none() => 1 << 8,
            LOOP_LIMIT => 1 << 12,
            named => named,
        };
        let limit = self.s.m.i32(limit);
        self.counters
            .get_mut(&to)
            .expect("header")
            .1
            .push((next, c));
        let over = self.s.m.cmp(c, p, next, limit);
        self.s.m.tag(over, Tag::Counter);
        over
    }

    /// The block a lane stopped by [`LOOP_LIMIT`] leaves through.
    pub(in crate::emit::lower) fn abort_block(&mut self) -> usize {
        if let Some(a) = self.abort {
            return a;
        }
        let a = self.s.m.new_block();
        if self.s.pixel {
            let t = self.s.m.bool(true);
            self.s
                .op(a, "discard", Attr::NoUnwind, self.s.t.void, 82, &[t]);
        }
        self.s.m.terminate(a, Term::Ret);
        self.abort = Some(a);
        a
    }

    /// The DXIL block a branch from IR block `from` to `to` goes to.
    pub(in crate::emit::lower) fn edge(&mut self, from: usize, to: usize) -> usize {
        let target = self.block[to].expect("reachable");
        let preds = self.x.p.blocks[to]
            .preds
            .iter()
            .filter(|&&q| self.block[q].is_some())
            .count();
        // Back edges stay as they are: the exiting block is the latch.
        if preds < 2 || self.rpo[to] <= self.rpo[from] {
            return target;
        }
        let e = self.s.m.new_block();
        self.s.m.terminate(e, Term::Br(target));
        self.edges.insert((from, to), e);
        e
    }

    pub(in crate::emit::lower) fn fill_phis(&mut self) -> Result<(), Stop> {
        let p = self.x.p;
        // Lane-mask joins first (what an edge brings may ask for more joins,
        // of masks and of numbers): each edge's mask as the structure
        // tracked it, a constant or a scalar-built one included.
        let st = self.x.st;
        while let Some((v, b, i)) = self.pending_masks.pop() {
            let phi = &p.blocks[b].phis[i];
            let offset = p.blocks[b].offset;
            let args = st
                .phi_masks
                .get(&phi.value)
                .ok_or_else(|| unsupported(offset, "a lane-mask join of an untracked mask"))?;
            let mut incoming = Vec::new();
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                let Some(qb) = self.tail[q] else { continue };
                let qb = self.edges.get(&(q, b)).copied().unwrap_or(qb);
                let x = self.boolean(args[n], qb).map_err(|w| unsupported(offset, w))?;
                incoming.push((x, qb));
            }
            self.s.m.set_phi(v, incoming);
        }
        // A join that only another join reads is read wherever that one is. Left
        // unmarked, a value without a DXIL form coming into it became zero without a
        // word (the looked-up tint of the material resolve programs, whose size
        // computation had no value, was zero in every lane that way).
        // shortcut: only with `OATHKEEP_STRICT_JOINS` set (the offline pack
        // listing sets it to count them): 40 programs that translate today
        // stop by name with it (vehicle paint pixel shaders at "the first
        // half of an interpolation", foliage mesh shaders); until those are
        // lowered they keep drawing with that zero.
        static STRICT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let strict = *STRICT.get_or_init(|| std::env::var_os("OATHKEEP_STRICT_JOINS").is_some());
        let mut work: Vec<Value> = if strict {
            self.read_phis.iter().copied().collect()
        } else {
            Vec::new()
        };
        while let Some(v) = work.pop() {
            let Some(&(b, i)) = self.phi_at.get(&v) else {
                continue;
            };
            for &a in &p.blocks[b].phis[i].args {
                if self.phi_at.contains_key(&a) && self.read_phis.insert(a) {
                    work.push(a);
                }
            }
        }
        for (v, b, i) in std::mem::take(&mut self.data_phis) {
            let phi = &p.blocks[b].phis[i];
            let bit = phi.reg == Reg::Scc;
            // A join of numbers with dead registers (`dead`): the paths that bring those
            // never read the result, so they bring zero. Not for a join that is itself a
            // lane mask (a ballot gives that one as data).
            let st = self.x.st;
            let numbers = !bit
                && !st.masks.contains_key(&phi.value)
                && !st.highs.contains_key(&phi.value)
                && phi
                    .args
                    .iter()
                    .any(|a| matches!(self.vals.get(a), Some(Val::I32(_))));
            let mut incoming = Vec::new();
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                let Some(qb) = self.tail[q] else { continue };
                let qb = self.edges.get(&(q, b)).copied().unwrap_or(qb);
                let arg = phi.args[n];
                if self.read_phis.contains(&phi.value) && !self.vals.contains_key(&arg) {
                    let _ = self.lazy_barycentric(arg);
                    // A root parameter nothing read before the join (user
                    // data is loaded on first use, in the entry block).
                    if !self.vals.contains_key(&arg)
                        && let Some(&(param, dword)) = self.params.get(&arg)
                    {
                        let val = self.user(param, dword);
                        self.vals.insert(arg, val);
                    }
                }
                let x = match (self.vals.get(&arg).copied(), bit) {
                    (Some(Val::I32(x)), false) | (Some(Val::Bit(x)), true) => x,
                    (Some(Val::Missing(_)) | None, false)
                        if numbers && self.dead(arg, &mut Vec::new()) =>
                    {
                        self.s.m.i32(0)
                    }
                    // A lane mask joined with numbers (a traversal that keeps its lanes' masks on
                    // a stack with its node numbers): the mask comes as the number it is, the
                    // ballot of its lanes where it is written.
                    (Some(Val::Missing(_)) | None, false)
                        if self.read_phis.contains(&phi.value)
                            && self.mask_data == MaskData::Console
                            && (st.masks.contains_key(&arg) || st.highs.contains_key(&arg)) =>
                    {
                        match self.mask_dword(arg) {
                            Ok(x) => x,
                            Err(why) => return Err(Stop::Retry(phi.value, why)),
                        }
                    }
                    // SCC of a mask operation ("some lane is left") joined with a scalar test:
                    // the compiler branches around code that sets SCC and tests SCC after it,
                    // the skip bringing "no lane" along. As data it is "some lane of the wave
                    // has the bit" - right only if the wave took that skip together, as the
                    // console does: a lane that skipped alone would bring "some lane" where the
                    // console has the skipped code's SCC. So the branch must be one the wave
                    // takes together; else its region is named for `emit`'s retry to widen it.
                    (Some(Val::Missing(_)) | None, true)
                        if self.read_phis.contains(&phi.value)
                            && self.mask_data == MaskData::Console
                            && st.scc_some.contains_key(&arg) =>
                    {
                        let per_lane = !st.wide.contains(&q)
                            && matches!(st.exits[q], LaneExit::Branch { when, .. }
                                if !st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_))));
                        if per_lane {
                            for &other in p.blocks[b].preds.iter().filter(|&&o| o != q) {
                                need(other);
                            }
                            return Err(Stop::Retry(phi.value, LANE_MASK));
                        }
                        match self.scc_some(arg) {
                            Ok(x) => x,
                            Err(why) => return Err(Stop::Retry(phi.value, why)),
                        }
                    }
                    (Some(Val::Bit(_)), false) if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, "an SCC joined with SGPR values"));
                    }
                    (Some(Val::Missing(why)), _) if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, why));
                    }
                    _ if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, "a join of mismatched values"));
                    }
                    _ if bit => self.s.m.bool(false),
                    _ => self.s.m.i32(0),
                };
                incoming.push((x, qb));
            }
            self.s.m.set_phi(v, incoming);
        }
        for (phi, incoming) in std::mem::take(&mut self.counters).into_values() {
            self.s.m.set_phi(phi, incoming);
        }
        Ok(())
    }
}
