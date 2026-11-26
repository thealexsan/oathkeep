//! Frame submission, timing breakdown, picture comparison, and resource readback.

use super::*;

impl Player {
    /// Sends the frame, waits for it, and reads the timestamps.
    pub(super) fn send(&mut self, s: &Sends, o: &Options) -> Result<Vec<u64>> {
        let cast = |l: &ID3D12GraphicsCommandList| l.cast::<ID3D12CommandList>().map(Some);
        let mut submissions: Vec<Vec<Option<ID3D12CommandList>>> = Vec::new();
        for (delta, lists) in &s.steps {
            if let Some(d) = delta {
                submissions.push(vec![cast(&d.list)?]);
            }
            let mut all = Vec::new();
            for l in lists {
                all.push(cast(&l.list)?);
            }
            if !all.is_empty() {
                submissions.push(all);
            }
        }
        if o.one_submit {
            let mut all = Vec::new();
            for (i, lists) in submissions.into_iter().enumerate() {
                if i > 0
                    && let Some(joint) = s.joints.get(i)
                {
                    all.push(cast(&joint.1)?);
                }
                all.extend(lists);
            }
            submissions = vec![all];
        }
        // SAFETY: closed lists whose last run has finished.
        unsafe {
            for all in &submissions {
                if !o.wait_lists {
                    self.worker.queue.ExecuteCommandLists(all);
                    if o.pace > 0.0 {
                        let until =
                            Instant::now() + std::time::Duration::from_secs_f64(o.pace / 1e3);
                        while Instant::now() < until {
                            std::thread::yield_now();
                        }
                    }
                    continue;
                }
                for l in all {
                    self.worker.queue.ExecuteCommandLists(&[l.clone()]);
                    self.worker.idle()?;
                }
            }
            self.worker
                .queue
                .ExecuteCommandLists(&[cast(&s.resolve.1)?]);
        }
        self.worker
            .idle()
            .context("the card was lost running the frame")?;
        // SAFETY: the mapping of `count` resolved timestamps.
        Ok(unsafe { std::slice::from_raw_parts(s.data as *const u64, s.count as usize) }.to_vec())
    }

    /// Runs the frame as the options say.
    pub fn run(&mut self, o: &Options) -> Result<Report> {
        let mut made = self.prepare(o)?;
        let _awake = if o.awake {
            crate::backend::clock_keepalive::start(&self.device, || true)
        } else {
            None
        };
        let mut stamps = Vec::new();
        for _ in 0..o.loops.max(1) {
            stamps.push(self.once(o, &mut made)?);
        }
        self.finish(o, made, &stamps)
    }

    /// The lists for a run as the options say.
    pub(super) fn prepare(&mut self, o: &Options) -> Result<(Sends, Report)> {
        let mut report = Report {
            notes: self.notes.clone(),
            ..Default::default()
        };
        self.set_merging(o.merge)?;
        let clamps = if o.skip_clamps {
            self.clamp_results(&mut report)?
        } else {
            Vec::new()
        };
        let submits = self.edited(o, &mut report, &clamps);
        Ok((self.build(&submits, o)?, report))
    }

    /// One run of the frame, noted in the report; its timestamps.
    pub(super) fn once(&mut self, o: &Options, (built, report): &mut (Sends, Report)) -> Result<Vec<u64>> {
        self.restore(o.restore)?;
        self.fresh = false;
        let stamps = self.send(built, o)?;
        if let Some((db, _, hash, _)) = self.picture_now()? {
            report.pictures.push((db, hash));
        }
        let ms = |a: u32, b: u32| {
            stamps[b as usize].saturating_sub(stamps[a as usize]) as f64 * 1e3 / self.frequency
        };
        let lists: Vec<&Built> = built.steps.iter().flat_map(|(_, l)| l).collect();
        let (Some(first), Some(last)) = (lists.first(), lists.last()) else {
            bail!("no lists to run");
        };
        let delta = built.steps.iter().filter_map(|(d, _)| d.as_ref());
        report.delta_ms.push(
            delta
                .filter(|d| d.first > first.first)
                .map(|d| ms(d.first, d.last))
                .sum(),
        );
        report.frame_ms.push(ms(first.first, last.last));
        let each = |l: &[Built]| match (l.first(), l.last()) {
            (Some(a), Some(b)) => ms(a.first, b.last),
            _ => 0.0,
        };
        report.submits = built
            .steps
            .iter()
            .map(|(_, l)| (l.len(), each(l)))
            .collect();
        report
            .busy_ms
            .push(report.submits.iter().map(|s| s.1).sum());
        Ok(stamps)
    }

    /// The report after the last run.
    pub(super) fn finish(
        &mut self,
        o: &Options,
        (built, mut report): (Sends, Report),
        stamps: &[Vec<u64>],
    ) -> Result<Report> {
        if o.detailed {
            self.detail(&built, stamps, o, &mut report);
        }
        if self.merging {
            let notes = self.merge_notes(o.loops)?;
            report.notes.extend(notes);
        }
        self.picture(o, &mut report)?;
        if let Some(q) = &self.info {
            report.notes.extend(debug_messages(q));
        }
        Ok(report)
    }

    /// The time by command: the median of the loops, without the first (which
    /// is slower) where there are others.
    fn detail(&self, built: &Sends, stamps: &[Vec<u64>], o: &Options, report: &mut Report) {
        let mut kinds: BTreeMap<&str, (Vec<f64>, u32)> = BTreeMap::new();
        let mut shaders: HashMap<u32, (Vec<f64>, u32)> = HashMap::new();
        let loops = if stamps.len() > 2 { &stamps[1..] } else { stamps };
        for stamps in loops {
            let mut of_kind: BTreeMap<&str, (f64, u32)> = BTreeMap::new();
            let mut of_shader: HashMap<u32, (f64, u32)> = HashMap::new();
            for l in built.steps.iter().flat_map(|(_, l)| l) {
                let mut before = stamps[l.first as usize];
                for &(at, kind, pso) in &l.stamps {
                    let ms = stamps[at].saturating_sub(before) as f64 * 1e3 / self.frequency;
                    before = before.max(stamps[at]);
                    let k = of_kind.entry(kind).or_default();
                    *k = (k.0 + ms, k.1 + 1);
                    if pso != NONE {
                        let s = of_shader.entry(pso).or_default();
                        *s = (s.0 + ms, s.1 + 1);
                    }
                }
            }
            for (k, (ms, n)) in of_kind {
                let all = kinds.entry(k).or_default();
                all.0.push(ms);
                all.1 = n;
            }
            for (k, (ms, n)) in of_shader {
                let all = shaders.entry(k).or_default();
                all.0.push(ms);
                all.1 = n;
            }
        }
        let kinds = kinds.into_iter().map(|(k, (ms, n))| (k, (median(&ms), n)));
        let shaders = shaders.into_iter().map(|(k, (ms, n))| (k, (median(&ms), n)));
        report.categories = kinds
            .into_iter()
            .map(|(k, (ms, n))| (k.to_owned(), ms, n))
            .collect();
        report.categories.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut all: Vec<(f64, u32, u32)> = shaders
            .into_iter()
            .map(|(pso, (ms, n))| (ms, n, pso))
            .collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0));
        all.truncate(o.top);
        report.shaders = all
            .into_iter()
            .map(|(ms, n, pso)| {
                let name = match self.objects.get(&(pso & !COPY_TO)).filter(|_| pso & COPY_TO != 0)
                {
                    Some(o) => {
                        let r = &self.frame.resources[o.index];
                        format!("copies to #{} {}", pso & !COPY_TO, r.name)
                    }
                    None => self.pipeline_name(pso),
                };
                (ms, n, name)
            })
            .collect();
    }

    /// The presented picture as it is now: dB against the recorded one,
    /// whether its bytes are the same, its hash, and its pixels.
    pub(super) fn picture_now(&mut self) -> Result<Option<(f64, bool, String, Vec<u8>)>> {
        let Some(p) = self.frame.presents.last().cloned() else {
            return Ok(None);
        };
        let Some(bytes) = self.contents(p.res)? else {
            return Ok(None);
        };
        let Some(rgb) = gpu::rgb(p.width, p.height, p.pitch, p.format, &bytes) else {
            return Ok(None);
        };
        let mut recorded = vec![0; bytes.len()];
        read_at(
            &File::open(self.folder.join("present.bin"))?,
            &mut recorded,
            p.file,
        )?;
        let was = gpu::rgb(p.width, p.height, p.pitch, p.format, &recorded).unwrap_or_default();
        let hash = blake3::hash(&rgb).to_hex().to_string();
        Ok(Some((
            gpu::psnr(&rgb, &was),
            rows_equal(&p, &bytes, &recorded),
            hash,
            rgb,
        )))
    }

    /// Saves the presented picture and compares it with the recorded one.
    fn picture(&mut self, o: &Options, report: &mut Report) -> Result<()> {
        let Some(p) = self.frame.presents.last().cloned() else {
            report.notes.push("no picture was presented".into());
            return Ok(());
        };
        let Some(bytes) = self.contents(p.res)? else {
            report
                .notes
                .push("the presented texture is not there".into());
            return Ok(());
        };
        let Some(rgb) = gpu::rgb(p.width, p.height, p.pitch, p.format, &bytes) else {
            report
                .notes
                .push("the presented format is not shown".into());
            return Ok(());
        };
        let path = o
            .image
            .clone()
            .unwrap_or_else(|| self.folder.join("replay.png"));
        std::fs::write(&path, crate::diagnostics::capture::png(p.width, p.height, &rgb))?;
        let mut recorded = vec![0; bytes.len()];
        read_at(
            &File::open(self.folder.join("present.bin"))?,
            &mut recorded,
            p.file,
        )?;
        let was = gpu::rgb(p.width, p.height, p.pitch, p.format, &recorded).unwrap_or_default();
        report.picture = Some((gpu::psnr(&rgb, &was), rows_equal(&p, &bytes, &recorded)));
        report.notes.push(format!(
            "picture {} (blake3 {}; recorded {})",
            path.display(),
            &blake3::hash(&rgb).to_hex()[..16],
            &blake3::hash(&was).to_hex()[..16]
        ));
        Ok(())
    }

    /// What a resource holds now (after a run): a buffer's bytes, or a
    /// texture's first subresource in its copyable layout.
    pub fn contents(&mut self, id: u32) -> Result<Option<Vec<u8>>> {
        self.contents_of(id, 0)
    }

    /// As [`Player::contents`], of a texture's subresource `sub`.
    pub fn contents_of(&mut self, id: u32, sub: u32) -> Result<Option<Vec<u8>>> {
        let Some(o) = self.objects.get(&id) else {
            return Ok(None);
        };
        let (res, r, subs) = (o.res.clone(), &self.frame.resources[o.index], o.subs);
        let desc = gpu::to_desc(&r.desc);
        let is_buffer = r.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0;
        let states = if self.fresh { &self.start } else { &self.end };
        let state = states.get(&id).cloned().unwrap_or(vec![COMMON]);
        let (f, total) = if is_buffer {
            (Vec::new(), r.desc.width)
        } else {
            gpu::footprints(&self.device, &desc, sub, 1)
        };
        let (back, data) = gpu::buffer(
            &self.device,
            D3D12_HEAP_TYPE_READBACK,
            total,
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        self.worker.run(|list| {
            if is_buffer {
                // SAFETY: an open list; live buffers.
                unsafe { list.CopyBufferRegion(&back, 0, &res, 0, total) };
            } else if state.len() == 1 || subs == 1 {
                read_sub(list, &res, sub, state[0], &back, f[0]);
            } else {
                let (mut before, mut after) = (Vec::new(), Vec::new());
                gpu::moves(&res, &state, &[COPY_SOURCE], &mut before);
                gpu::moves(&res, &[COPY_SOURCE], &state, &mut after);
                // SAFETY: an open list; live resources.
                unsafe { barriers(list, &before) };
                read_sub(list, &res, sub, COPY_SOURCE, &back, f[0]);
                // SAFETY: as above.
                unsafe { barriers(list, &after) };
            }
        })?;
        // SAFETY: the buffer's mapping; the copy into it has run.
        Ok(Some(
            unsafe { std::slice::from_raw_parts(data, total as usize) }.to_vec(),
        ))
    }
}
