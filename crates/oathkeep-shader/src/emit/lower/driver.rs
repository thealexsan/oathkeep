use super::*;

impl<'a> Lower<'a> {
    pub(in crate::emit::lower) fn run(mut self, counted: bool) -> Result<Lowered, Stop> {
        let p = self.x.p;
        if counted {
            // Before anything else, so that wave counts, reads of the first
            // lane and stores know only the lanes that run.
            let original = self.s.entry;
            let width = self.s.threads.expect("compute thread count")[0];
            let limit = self.host_word(RootType::Other(crate::emit::shader::USER_DISPATCH_THREADS_X), 0, width);
            let Val::I32(group) = self.compute_id("groupId.i32", 94, 0) else {
                unreachable!()
            };
            let Val::I32(thread) = self.thread_id(0) else {
                unreachable!()
            };
            let width = self.s.m.i32(width);
            let first = self.s.m.bin(original, Bin::Mul, group, width);
            let x = self.s.m.bin(original, Bin::Add, first, thread);
            let zero = self.s.m.i32(0);
            let all = self.s.m.cmp(original, pred::EQ, limit, zero);
            let inside = self.s.m.cmp(original, pred::IULT, x, limit);
            let runs = self.s.m.bin(original, Bin::Or, all, inside);
            let (body, done) = (self.s.m.new_block(), self.s.m.new_block());
            self.s.m.terminate(original, Term::CondBr(runs, body, done));
            self.s.m.terminate(done, Term::Ret);
            self.s.entry = body;
        }
        self.find_coherent();
        let order = crate::ir::reverse_postorder(&p.blocks);
        for (n, &b) in order.iter().enumerate() {
            self.rpo[b] = n;
        }
        for (n, &b) in order.iter().enumerate() {
            self.block[b] = Some(if n == 0 {
                self.s.entry
            } else {
                self.s.m.new_block()
            });
        }
        // Loop headers count iterations (`LOOP_LIMIT`).
        for &b in &order {
            let header = p.blocks[b]
                .preds
                .iter()
                .any(|&q| self.block[q].is_some() && self.rpo[b] <= self.rpo[q]);
            if header {
                let blk = self.block[b].expect("reachable");
                let phi = self.s.m.phi(blk, self.s.t.i32);
                self.counters.insert(b, (phi, Vec::new()));
            }
        }
        self.cur = self.s.entry;
        self.find_sections();
        self.inputs()?;
        if let Some(mesh) = self.s.mesh {
            self.mesh_prologue(mesh).map_err(|w| unsupported(0, w))?;
            self.block[order[0]] = Some(self.cur);
        }
        for &b in &order {
            self.cur = self.block[b].expect("reachable");
            self.cur_ir = b;
            let preds: Vec<usize> = p.blocks[b]
                .preds
                .iter()
                .copied()
                .filter(|&q| self.block[q].is_some())
                .collect();
            self.exported[b] = !preds.is_empty() && preds.iter().all(|&q| self.exported_out(q));
            for (i, phi) in p.blocks[b].phis.iter().enumerate() {
                let val = match phi.reg {
                    Reg::S(c) if c == EXEC || c == EXEC + 1 => Val::Missing("a lane mask"),
                    _ if self.bad.contains_key(&phi.value) => Val::Missing(self.bad[&phi.value]),
                    Reg::Scc => {
                        let v = self.s.m.phi(self.cur, self.s.t.i1);
                        self.data_phis.push((v, b, i));
                        Val::Bit(v)
                    }
                    _ => {
                        let v = self.s.m.phi(self.cur, self.s.t.i32);
                        self.data_phis.push((v, b, i));
                        Val::I32(v)
                    }
                };
                self.vals.insert(phi.value, val);
            }
            if self.lds_meet.as_ref().is_some_and(|(_, meet)| meet[b].0) {
                self.lds_meeting();
            }
            let mut i = 0;
            while i < p.blocks[b].insts.len() {
                self.inst(b, i)?;
                if let Some(end) = self.sections.get(&b).and_then(|s| s.get(&i)).map(|s| s.end) {
                    for j in i + 1..end {
                        self.section_inst(b, j, i);
                    }
                    i = end;
                    continue;
                }
                i += 1;
            }
            self.mask_ok = self.mask_data == MaskData::Console;
            if self.lds_meet.as_ref().is_some_and(|(_, meet)| meet[b].1) {
                self.lds_meeting();
            }
            self.tail[b] = Some(self.cur);
            self.exit(b)?;
        }
        self.fill_phis()?;
        let user_data = self.user_reads.values().copied().collect();
        self.s.samplers = self.samplers.len() as u32;
        let root_cbvs = self.s.root_cbvs.iter().map(|r| r.0).collect();
        let root_srvs = self.s.root_srvs.iter().map(|r| r.0).collect();
        let root_uavs = self.s.root_uavs.iter().map(|r| r.0).collect();
        let cmp_samplers = self.s.cmp_samplers;
        let wave_size = self.s.wave_size;
        Ok(Lowered {
            wave_size,
            cmp_samplers,
            root_cbvs,
            root_srvs,
            root_uavs,
            #[cfg(test)]
            nonuniform_handles: self.s.nonuniform_handles,
            #[cfg(test)]
            wave_ops: self.s.flags & flags::WAVE_OPS != 0,
            dxil: self.s.finish(),
            user_data,
            samplers: self.samplers,
        })
    }

    pub(in crate::emit::lower) fn exported_out(&self, b: usize) -> bool {
        self.exported[b] || self.has_done_export(b)
    }

    pub(in crate::emit::lower) fn has_done_export(&self, b: usize) -> bool {
        (0..self.x.p.blocks[b].insts.len())
            .any(|i| self.exports.get(&(b, i)).is_some_and(|e| e.done))
    }
}
