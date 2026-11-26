//! Experimental command-list edits, clamp batching, and saved clamp results.

use super::*;

impl Player {
    /// The frame's lists, edited: for each submission, its lists' commands.
    pub(super) fn edited(
        &mut self,
        o: &Options,
        report: &mut Report,
        clamps: &[Vec<Clamp>],
    ) -> Vec<Vec<Vec<Cmd>>> {
        let skip: HashSet<u32> = self
            .frame
            .pipelines
            .iter()
            .filter(|p| {
                let hashes = self.hashes(p);
                o.skip_shaders.iter().any(|s| {
                    !s.is_empty()
                        && (p.name.contains(s.as_str())
                            || hashes.iter().any(|h| h.contains(s.as_str())))
                })
            })
            .map(|p| p.id)
            .collect();
        if !o.skip_shaders.is_empty() {
            report
                .notes
                .push(format!("{} pipelines match --skip-shader", skip.len()));
        }
        let hash = self.pipelines_named(COMPARE);
        let clamp = self.pipelines_named(CLAMP);
        let compute: HashSet<u32> = self
            .frame
            .pipelines
            .iter()
            .filter(|p| p.parts.iter().any(|part| matches!(part, wire::Part::Code { kind, .. } if *kind == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS.0)))
            .map(|p| p.id)
            .collect();
        let mut batched = edit::Batched::default();
        let mut jobs = Vec::new();
        let mut scratches = Vec::new();
        let (mut skipped, mut hashed, mut uavs, mut merged) = (0, 0, 0, 0);
        let mut place = 0;
        let mut out = Vec::new();
        for s in &self.frame.submits {
            let mut lists = Vec::new();
            for &l in s.lists.iter().filter(|&&l| l != MISSING) {
                let mut cmds = self.commands[l as usize].clone();
                let here = place;
                place += 1;
                if o.only.is_some_and(|(from, to)| here < from || here > to) {
                    lists.push(edit::barriers_only(cmds));
                    continue;
                }
                if let Some((list, command)) = o.until
                    && here >= list
                {
                    let from = if here > list {
                        0
                    } else {
                        (command + 1).min(cmds.len())
                    };
                    let rest = cmds.split_off(from);
                    cmds.extend(edit::barriers_only(rest));
                }
                if o.skip_clamps
                    && let Some(c) = clamps.get(here as usize)
                {
                    cmds = edit::skip_clamps(cmds, c);
                }
                if o.batch_clamps > 0 {
                    let scratch = edit::BATCH_SCRATCH - here;
                    let done;
                    (cmds, done) = edit::batch_clamps(
                        cmds,
                        &clamp,
                        &compute,
                        scratch,
                        o.batch_clamps > 1,
                        &mut jobs,
                    );
                    batched.clamps += done.clamps;
                    batched.batches += done.batches;
                    batched.jobs += done.jobs;
                    if done.scratch > 0 {
                        scratches.push((scratch, done.scratch));
                    }
                }
                let mut n;
                if !skip.is_empty() {
                    (cmds, n) = edit::skip_pipelines(cmds, &skip);
                    skipped += n;
                }
                if o.skip_hash_passes {
                    (cmds, n) = edit::skip_pipelines(cmds, &hash);
                    hashed += n;
                }
                if o.no_barriers {
                    (cmds, n) = edit::no_uav_barriers(cmds);
                    uavs += n;
                }
                if o.merge_barriers {
                    (cmds, n) = edit::merge_barriers(cmds);
                    merged += n;
                }
                lists.push(cmds);
            }
            out.push(lists);
        }
        if o.batch_clamps > 0 {
            report.notes.push(format!(
                "--batch-clamps: {} clamp passes became {}, of {} records",
                batched.clamps, batched.batches, batched.jobs
            ));
            if let Err(e) = self.make_batches(&jobs, &scratches) {
                report.notes.push(format!("--batch-clamps failed: {e:#}"));
            }
        }
        if !skip.is_empty() {
            report
                .notes
                .push(format!("--skip-shader left out {skipped} commands"));
        }
        if o.skip_hash_passes {
            report
                .notes
                .push(format!("--skip-hash-passes left out {hashed} dispatches"));
        }
        if o.no_barriers {
            report.notes.push(format!(
                "--no-barriers left out {uavs} unordered-access barriers"
            ));
        }
        if o.merge_barriers {
            report.notes.push(format!(
                "--merge-barriers joined {merged} barrier calls to ones before them"
            ));
        }
        out
    }

    /// What lists edited by [`edit::batch_clamps`] name: the pass, its
    /// tables and scratch buffers.
    fn make_batches(&mut self, jobs: &[u8], scratches: &[(u32, u64)]) -> Result<()> {
        let (root, pso) = crate::indirect::batch_for(&self.device)?;
        self.roots.insert(edit::BATCH_ROOT, root);
        self.pipelines.insert(edit::BATCH, pso);
        let (table, data) = gpu::buffer(
            &self.device,
            D3D12_HEAP_TYPE_UPLOAD,
            jobs.len().max(4) as u64,
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        // SAFETY: a mapping of as many bytes.
        unsafe { std::ptr::copy_nonoverlapping(jobs.as_ptr(), data.cast(), jobs.len()) };
        let mut all = vec![(edit::BATCH_JOBS, table)];
        for &(id, bytes) in scratches {
            let (scratch, _) = gpu::buffer(
                &self.device,
                D3D12_HEAP_TYPE_DEFAULT,
                bytes.next_multiple_of(64 << 10),
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            )?;
            all.push((id, scratch));
        }
        for (id, res) in all {
            // SAFETY: a live buffer.
            let va = unsafe { res.GetGPUVirtualAddress() };
            let object = Object {
                res,
                va,
                index: 0,
                subs: 1,
            };
            self.objects.insert(id, object);
        }
        Ok(())
    }

    /// The clamp passes of each list of the frame, in order, and their
    /// results in [`RESULTS`] (by a run that keeps them).
    pub(super) fn clamp_results(&mut self, report: &mut Report) -> Result<Vec<Vec<Clamp>>> {
        let psos = self.pipelines_named(CLAMP);
        let mut next = 0;
        let mut clamps = Vec::new();
        let mut submits = Vec::new();
        for s in &self.frame.submits {
            let mut lists = Vec::new();
            for &l in s.lists.iter().filter(|&&l| l != MISSING) {
                let cmds = self.commands[l as usize].clone();
                let found = edit::find_clamps(&cmds, &psos, &mut next);
                lists.push(edit::keep_clamped(cmds, &found));
                clamps.push(found);
            }
            submits.push(lists);
        }
        let n: usize = clamps.iter().map(Vec::len).sum();
        report.notes.push(format!(
            "--skip-clamps: {n} clamp passes, {} KB of arguments kept from a first run",
            next >> 10
        ));
        if n == 0 {
            return Ok(clamps);
        }
        self.results = Some(
            gpu::buffer(
                &self.device,
                D3D12_HEAP_TYPE_DEFAULT,
                next,
                D3D12_RESOURCE_FLAG_NONE,
            )?
            .0,
        );
        let built = self.build(&submits, &Options::default())?;
        self.restore(true)?;
        self.send(&built, &Options::default())?;
        self.fresh = false;
        Ok(clamps)
    }
}
