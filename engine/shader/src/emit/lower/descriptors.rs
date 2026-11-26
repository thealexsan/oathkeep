use super::*;

impl<'a> Lower<'a> {
    /// The byte offset a buffer descriptor built around a root descriptor adds
    /// (constant and computed parts); none for other descriptors.
    pub(in crate::emit::lower) fn built_offset(&mut self, src: &Source) -> Result<(u32, Option<V>), Why> {
        match src {
            Source::Built { address, .. } => {
                let mut x = match address.dynamic {
                    Some(v) => Some(self.i32_of(v)?),
                    None => None,
                };
                if let Some(bias) = self.root_bias(address.param) {
                    x = Some(match x {
                        Some(d) => self.s.m.bin(self.cur, Bin::Add, d, bias),
                        None => bias,
                    });
                }
                Ok((address.offset, x))
            }
            _ => Ok((0, None)),
        }
    }

    /// For a root view the host binds as a raw view that may begin before
    /// the title's address (`shader.rs` `USER_VIEW_BIAS`): the bytes to add
    /// to offsets through it. Every root UAV (root SRVs keep the views they
    /// have).
    pub(in crate::emit::lower) fn root_bias(&mut self, param: usize) -> Option<V> {
        use crate::emit::shader::USER_VIEW_BIAS;
        let rp = *self.x.native.layout.get(param)?;
        // (Title memory by address where the user-data table is spilled:
        // [`Lower::root_memory`].)
        if rp.root != RootType::Descriptor || crate::spill::is_spilled(self.x.native) {
            return None;
        }
        let kind = self
            .x
            .native
            .used
            .iter()
            .find(|u| u.user_data == rp.user_data)
            .map(|u| u.kind);
        (kind == Some(ResourceType::Uav)).then(|| self.host_word(RootType::Other(USER_VIEW_BIAS), rp.slot, 0))
    }

    /// The used-resource entry a descriptor loaded from a table falls in.
    pub(in crate::emit::lower) fn used_kind(&self, src: &Source) -> Option<&'a UsedResource> {
        if let Source::Phi { args, .. } | Source::Select { args, .. } | Source::Lanes { args, .. } =
            src
        {
            return self.used_kind(args.first()?);
        }
        if let Source::LanePhi { args, .. } = src {
            return args.iter().flatten().find_map(|a| self.used_kind(a));
        }
        if let Source::Built { address, .. } = src {
            let rp = self.x.native.layout.get(address.param)?;
            return self
                .x
                .native
                .used
                .iter()
                .find(|u| u.root == RootType::Descriptor && u.user_data == rp.user_data);
        }
        let (Source::Loaded { load, first } | Source::Fetched { load, first }) = src else {
            return None;
        };
        let a = self.scalar.get(load)?;
        let rp = self.x.native.layout.get(a.param)?;
        if rp.root != RootType::Table {
            return None;
        }
        let dword = (a.offset / 4) + u32::from(*first);
        self.x
            .native
            .used
            .iter()
            .filter(|u| {
                u.root == RootType::Table
                    && u.user_data == rp.user_data
                    && u.table_offset_dwords <= dword
            })
            .max_by_key(|u| u.table_offset_dwords)
    }

    /// A handle for the descriptor `src` read at `site`.
    pub(in crate::emit::lower) fn handle(
        &mut self,
        site: Site,
        src: &Source,
        sampler: bool,
        props: (u32, u32),
    ) -> Result<V, Why> {
        match src {
            Source::Loaded { load, first } => {
                let key = (self.cur, *load, *first, sampler, props);
                if let Some(&h) = self.handles.get(&key) {
                    return Ok(h);
                }
                let index = self.heap_index(*load, *first, sampler)?;
                let (index, bounded) = self.raw_twin(src, index, sampler, props);
                // A waterfall loop's load at the first lane's offset reads
                // each lane's own entry (see `resource::Waterfall`).
                let per_lane = self.x.res.waterfall.loads.contains(load);
                let h = self
                    .s
                    .heap_handle_at(self.cur, index, sampler, per_lane, props, bounded);
                self.handles.insert(key, h);
                Ok(h)
            }
            Source::Constant(words) if sampler => {
                let n = self.register_sampler(words, props.0 & 0x8000 != 0)?;
                Ok(self.s.sampler_handle(self.cur, n))
            }
            Source::Constant(_) => Err("a constant resource descriptor"),
            Source::Built { .. } if self.root_copy.is_some() => self.root_copy.ok_or("no copy"),
            Source::Built { address, .. } => {
                let rp = *self
                    .x
                    .native
                    .layout
                    .get(address.param)
                    .ok_or("a descriptor built around a table in the code")?;
                let kind = self
                    .x
                    .native
                    .used
                    .iter()
                    .find(|u| u.user_data == rp.user_data)
                    .map(|u| u.kind);
                match (rp.root, kind, props.0) {
                    (RootType::Descriptor, Some(ResourceType::Cbv) | None, 13) => {
                        Ok(self.s.root_cbv_handle(rp.slot))
                    }
                    (RootType::Descriptor, Some(ResourceType::Srv), 11) => {
                        Ok(self.s.root_srv_handle(rp.slot))
                    }
                    // A root UAV: a raw buffer whether the access reads
                    // (11) or writes (11 with the UAV bit). Indexed
                    // accesses reach here as raw ones at index times the
                    // entry's stride (`buffer`).
                    (RootType::Descriptor, Some(ResourceType::Uav), 11 | 0x100b) => {
                        Ok(self.s.root_uav_handle(rp.slot))
                    }
                    (RootType::Descriptor, Some(ResourceType::Srv), 0x100b) => {
                        Err("a write through a root SRV")
                    }
                    _ => Err("a typed root view"),
                }
            }
            Source::Phi { block, args } => {
                let _ = site;
                let phi = self.descriptor_phi(*block, args, sampler, 8)?;
                let (index, bounded) = self.raw_twin(src, phi, sampler, props);
                Ok(self.s.heap_handle_at(self.cur, index, sampler, false, props, bounded))
            }
            Source::Select { scc, args } => {
                // The heap index of each choice, picked by SCC (uniform).
                let [
                    Source::Loaded { load: a, first: fa },
                    Source::Loaded { load: b, first: fb },
                ] = args[..]
                else {
                    return Err("a descriptor chosen by a condition");
                };
                let cond = match self.get(*scc)? {
                    Val::Bit(x) => x,
                    _ => return Err("SCC without a value"),
                };
                let set = self.heap_index(a, fa, sampler)?;
                let clear = self.heap_index(b, fb, sampler)?;
                let index = self.s.m.select(self.cur, cond, set, clear);
                let (index, bounded) = self.raw_twin(src, index, sampler, props);
                Ok(self.s.heap_handle_at(self.cur, index, sampler, false, props, bounded))
            }
            Source::Fetched { .. } | Source::Lanes { .. } | Source::LanePhi { .. } => {
                // A descriptor each lane has its own of (a waterfall loop
                // reads it from the first active lane): each lane's heap
                // index, non-uniform.
                let index = self.lane_heap_index(src, sampler)?;
                let (index, bounded) = self.raw_twin(src, index, sampler, props);
                Ok(self.s.heap_handle_at(self.cur, index, sampler, true, props, bounded))
            }
        }
    }

    /// The heap index to read descriptor `src` at, `index` being the title's:
    /// for a buffer read as raw bytes, through a table or picked from the
    /// heap itself (or picked and read as a constant buffer), the host's raw
    /// twin of that slot ([`crate::emit::shader::raw_views`]), whose reads end where
    /// the view does. Everything else (textures, typed buffers, tables'
    /// constant buffers) stays in place. And whether it is a twin's (inside
    /// the host's heap).
    ///
    /// Tables too: a view that does not begin on 16 bytes cannot be raw, and
    /// a pixel program of the main pass (3711fdad456538a1, stopped at in the
    /// device losses of runs n03 and n04) reads a structured one of 8 byte
    /// elements at 8 as raw bytes per lane.
    pub(in crate::emit::lower) fn raw_twin(&mut self, src: &Source, index: V, sampler: bool, props: (u32, u32)) -> (V, bool) {
        // Raw and constant buffers (DXIL resource kinds 11 and 13).
        const RAW: u32 = 11;
        const CONSTANT: u32 = 13;
        let first = crate::emit::shader::raw_views();
        let picked = |leaf: &&Source| {
            let (Source::Loaded { load, .. } | Source::Fetched { load, .. }) = leaf else {
                return false;
            };
            self.scalar
                .get(load)
                .and_then(|a| self.x.native.layout.get(a.param))
                .is_some_and(|p| {
                    p.root == RootType::ResourceHeap || (p.root == RootType::Table && props.0 & 0xFF == RAW)
                })
        };
        if first == 0 || sampler || !matches!(props.0 & 0xFF, RAW | CONSTANT) || !src.leaves().iter().all(picked) {
            return (index, false);
        }
        let c = self.cur;
        let (last, first) = (self.s.m.i32(first - 1), self.s.m.i32(first));
        let i32t = self.s.t.i32;
        // (As `dx.op.binary` UMin, see `heap_handle`.)
        let index = self.s.op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[index, last]);
        self.s.m.tag(index, Tag::Clamp);
        (self.s.m.bin(c, Bin::Or, index, first), true)
    }

    /// The heap index a descriptor phi picks. Each predecessor computes its
    /// heap index at its end and the join picks one with a phi; a
    /// predecessor whose descriptor is itself chosen by control flow (a
    /// phi of an earlier join, which dominates it) gives that join's phi,
    /// at most `depth` joins deep.
    pub(in crate::emit::lower) fn descriptor_phi(
        &mut self,
        block: usize,
        args: &[Source],
        sampler: bool,
        depth: u32,
    ) -> Result<V, Why> {
        const WHY: Why = "a descriptor chosen by control flow";
        let p = self.x.p;
        let mut incoming = Vec::new();
        for (&q, arg) in p.blocks[block].preds.iter().zip(args) {
            if self.block[q].is_none() {
                continue;
            }
            let tail = self.tail[q].ok_or(WHY)?;
            let at = self.edges.get(&(q, block)).copied().unwrap_or(tail);
            let index = match arg {
                Source::Loaded { load, first } => {
                    let here = std::mem::replace(&mut self.cur, at);
                    let index = self.heap_index(*load, *first, sampler);
                    self.cur = here;
                    index?
                }
                Source::Phi { block: inner, args: inner_args } if depth > 0 => {
                    self.descriptor_phi(*inner, inner_args, sampler, depth - 1)?
                }
                _ => return Err(WHY),
            };
            incoming.push((index, at));
        }
        let join = self.block[block].ok_or(WHY)?;
        let phi = self.s.m.phi(join, self.s.t.i32);
        self.s.m.set_phi(phi, incoming);
        Ok(phi)
    }
}
