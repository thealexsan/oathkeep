//! Root signatures, command signatures, query heaps, and pipeline recreation.

use super::*;

impl Player {
    pub(super) fn make_pipelines(&mut self, shaders: Option<&Path>) -> Result<()> {
        for (id, blob) in &self.frame.roots {
            if self.roots.contains_key(id) {
                // (Made for another set of shaders: the root signatures are the same.)
                continue;
            }
            // SAFETY: a blob the host took.
            let root = unsafe { self.device.CreateRootSignature(0, &blob.0) }
                .with_context(|| format!("root signature {id}"))?;
            self.roots.insert(*id, root);
        }
        for s in &self.frame.signatures {
            if self.signatures.contains_key(&s.id) {
                continue;
            }
            let desc = D3D12_COMMAND_SIGNATURE_DESC {
                ByteStride: s.stride,
                NumArgumentDescs: s.args.len() as u32,
                pArgumentDescs: s.args.as_ptr().cast(),
                NodeMask: 0,
            };
            let mut made: Option<ID3D12CommandSignature> = None;
            // SAFETY: a description the host took.
            unsafe {
                self.device
                    .CreateCommandSignature(&desc, self.roots.get(&s.root), &mut made)?
            };
            self.signatures.extend(made.map(|m| (s.id, m)));
        }
        for q in &self.frame.query_heaps {
            if self.queries.contains_key(&q.id) {
                continue;
            }
            let desc = D3D12_QUERY_HEAP_DESC {
                Type: D3D12_QUERY_HEAP_TYPE(q.kind),
                Count: q.count,
                NodeMask: 0,
            };
            let mut made: Option<ID3D12QueryHeap> = None;
            // SAFETY: plain creation.
            unsafe { self.device.CreateQueryHeap(&desc, &mut made)? };
            self.queries.extend(made.map(|m| (q.id, m)));
        }
        let file = File::open(self.folder.join("shaders.bin"))?;
        let device: ID3D12Device2 = self.device.cast()?;
        let next = AtomicUsize::new(0);
        type Made = std::result::Result<ID3D12PipelineState, String>;
        let made: std::sync::Mutex<Vec<(u32, Made)>> = std::sync::Mutex::default();
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get() / 2)
            .max(1);
        let (frame, roots) = (&self.frame, &self.roots);
        let (swapped, kept) = (AtomicUsize::new(0), AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    while let Some(p) = frame.pipelines.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let recorded = |i: u32| {
                            let c = frame.codes.get(i as usize)?;
                            let mut bytes = vec![0; c.len as usize];
                            read_at(&file, &mut bytes, c.at).ok()?;
                            Some(bytes)
                        };
                        let root = |id: u32| roots.get(&id).cloned();
                        let taken = std::cell::Cell::new(false);
                        let mut m = super::super::pso::make(&device, &p.parts, root, |i| {
                            let other = shaders.and_then(|dir| {
                                let own = || {
                                    frame.codes.get(i as usize).map(|c| {
                                        c.hash[..8].iter().map(|b| format!("{b:02x}")).collect()
                                    })
                                };
                                let hash = title_hash(p, i).map(str::to_owned).or_else(own)?;
                                std::fs::read(dir.join(format!("{hash}.dxil"))).ok()
                            });
                            taken.set(taken.get() || other.is_some());
                            other.or_else(|| recorded(i))
                        });
                        if taken.get() && m.is_err() {
                            // Not for the root signature or the other stage recorded.
                            kept.fetch_add(1, Ordering::Relaxed);
                            m = super::super::pso::make(&device, &p.parts, root, recorded);
                        } else if taken.get() {
                            swapped.fetch_add(1, Ordering::Relaxed);
                        }
                        made.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((p.id, m));
                    }
                });
            }
        });
        let mut refused = Vec::new();
        for (id, m) in made.into_inner().unwrap_or_else(|e| e.into_inner()) {
            match m {
                Ok(p) => drop(self.pipelines.insert(id, p)),
                Err(e) => refused.push((id, e)),
            }
        }
        if shaders.is_some() {
            let n = format!(
                "{} of {} pipelines made with shaders of the folder given, {} refused with them (as recorded)",
                swapped.into_inner(),
                self.frame.pipelines.len(),
                kept.into_inner()
            );
            self.note(n);
        }
        if let Some((id, e)) = refused.first() {
            let n = format!(
                "{} pipelines could not be made again, first {}: {e}",
                refused.len(),
                self.pipeline_name(*id)
            );
            self.note(n);
        }
        Ok(())
    }

    /// A pipeline's name for reports: what the host layer called it and the
    /// hashes of its shaders.
    pub(super) fn pipeline_name(&self, id: u32) -> String {
        let Some(p) = self.frame.pipelines.iter().find(|p| p.id == id) else {
            return format!("pipeline {id}");
        };
        let code: Vec<String> = self.hashes(p);
        format!(
            "{} (DXIL {})",
            if p.name.is_empty() {
                "unnamed"
            } else {
                &p.name
            },
            code.join(" ")
        )
    }

    pub(super) fn hashes(&self, p: &wire::Pipeline) -> Vec<String> {
        p.parts
            .iter()
            .filter_map(|part| match part {
                wire::Part::Code { blob, .. } => {
                    let c = self.frame.codes.get(*blob as usize)?;
                    Some(c.hash[..8].iter().map(|b| format!("{b:02x}")).collect())
                }
                _ => None,
            })
            .collect()
    }

    pub(super) fn pipelines_named(&self, name: &str) -> HashSet<u32> {
        self.frame
            .pipelines
            .iter()
            .filter(|p| p.name == name)
            .map(|p| p.id)
            .collect()
    }
}
