use super::*;

impl<'a> Lower<'a> {
    /// 28, dword 4 depth or layers - 1; buffer dword 2 the record count).
    // shortcut: the base level reads 0 (the host view starts at its own
    // level 0, so sizes the code shifts by it come out right) and the other
    // dwords (addresses, formats, tiling) have no host value: reading them
    // stops the program.
    pub(in crate::emit::lower) fn descriptor_data(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        const OTHER: Why = "a descriptor's address or format read as data";
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar memory read as data");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u32,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar memory read as data"),
        };
        // How the program uses the descriptor tells the view's kind: an image or buffer
        // instruction before a scalar buffer load, which reads any buffer.
        let x = self.x;
        let uses = |of: Site| {
            x.res
                .descriptors
                .iter()
                .filter(|d| d.role == Role::Resource)
                .filter_map(move |d| {
                    d.source.leaves().into_iter().find_map(|l| match *l {
                        Source::Loaded { load, first } if load == of => Some((d.site, first)),
                        _ => None,
                    })
                })
        };
        let by_scalar_load = |u: &(Site, u8)| {
            matches!(x.p.blocks[u.0.0].insts[u.0.1].fields, Fields::Smem { .. })
        };
        let use_of = |of: Site| uses(of).find(|u| !by_scalar_load(u)).or_else(|| uses(of).next());
        // The console compiler loads a descriptor again wherever it needs it, and for a
        // size it reads the dwords of a load that nothing uses as a descriptor (a texture
        // picked from the heap by an index held per lane: one load for its width, one for
        // its base level, one for each read of the image). Another load of as many dwords
        // from the same address reads the same descriptor, so what uses that one tells
        // this one's kind. The handle is still made from this load's own operands, which
        // hold here.
        let address = self.scalar.get(&site).copied();
        let twin = || {
            let a = address?;
            let same = |s: Site| x.p.blocks[s.0].insts[s.1].name() == inst.name();
            x.res
                .scalar
                .iter()
                .filter(|(s, b)| *s != site && *b == a && same(*s))
                .find_map(|(s, _)| use_of(*s))
        };
        let use_at = use_of(site).or_else(twin);
        // A view whose kind no use tells, but what is read of it does.
        let sized = match use_at {
            Some(_) => None,
            None => self.sized_view(inst, site, code, count),
        };
        // A descriptor nothing uses as one, whose size the code reads all the same (a
        // texture picked from the heap by an index in a constant buffer, its width and
        // height turned into a texel size): the kind from its dwords and its used-resource
        // entry. Only an image whose layers or depth (dword 4) are not read: those are no 2D
        // texture's; and no buffer `sized_view` did not take (a stride of 4 or less may be a
        // typed view's component count, and the address is `view_address`'s below).
        let layers = inst.write(Reg::S(code + 4)).is_some_and(|v| self.read_as_number(v));
        let unused = if use_at.is_none() && sized.is_none() && count == 8 && !layers {
            self.unused_descriptor_kind(inst, site, code, count)
        } else {
            None
        };
        let (first, props, image, uav, used) = if let Some(kind) = unused {
            kind
        } else {
        let Some((at, first)) = use_at.or(sized.map(|_| (site, 0))) else {
            // A descriptor kept per lane in VGPRs (`Resources::lane_copied`): the code only
            // copies its dwords and compares them, lane against first lane, and every load
            // that reaches those lanes reads one address. Each dword stands in as its place
            // in the table, so the lanes compare equal as they do on the console.
            if self.x.res.lane_copied.contains(&site)
                && let Some(a) = self.scalar.get(&site).copied()
            {
                for k in 0..count {
                    let x = self.s.m.i32(a.offset / 4 + k);
                    self.write(inst, Reg::S(code + k as u8), x, None);
                }
                return Ok(());
            }
            // A sampler's descriptor: no program reads its dwords as numbers, so where one
            // meets numbers at a join the register is dead on that path (`dead`), as a
            // descriptor's address or format is.
            let sampler = x.res.descriptors.iter().any(|d| {
                d.role == Role::Sampler
                    && d.source.leaves().into_iter().any(
                        |l| matches!(*l, Source::Loaded { load, .. } if load == site),
                    )
            });
            if sampler {
                self.stale.extend(inst.writes.iter().map(|w| w.1));
                return Err("a sampler's descriptor read as data");
            }
            // A buffer view the code only takes the address of, to walk
            // the memory behind it (a ray-tracing structure): the address
            // from the host, the other dwords have no value.
            if count <= 4
                && let Some((lo, hi)) = self.view_address(site)
            {
                for k in 0..count {
                    let reg = Reg::S(code + k as u8);
                    match k {
                        0 => self.write(inst, reg, lo, None),
                        1 => self.write(inst, reg, hi, None),
                        _ => {
                            if let Some(v) = inst.write(reg) {
                                self.vals.insert(v, Val::Missing(OTHER));
                                self.stale.insert(v);
                            }
                        }
                    }
                }
                return Ok(());
            }
            if std::env::var_os("OATHKEEP_DESC_WHY").is_some() {
                eprintln!(
                    "DESC-WHY {:#x} {} scalar={:?} lane_copied={}",
                    inst.machine.offset,
                    inst.name(),
                    address,
                    self.x.res.lane_copied.contains(&site)
                );
            }
            return Err("a descriptor read only as data");
        };
        let src = Source::Loaded { load: site, first };
        let used = self.used_kind(&src);
        let ui: &Inst = &self.x.p.blocks[at.0].insts[at.1];
        let un = ui.name();
        let uav = un.contains("store")
            || un.contains("atomic")
            || used.is_some_and(|u| u.kind == ResourceType::Uav);
        let u = if uav { 0x1000 } else { 0 };
        let (props, image) = match &ui.fields {
            _ if sized.is_some_and(|image| image) => ((2 | u, 0x409), Some(2)),
            _ if sized.is_some() => ((11 | u, 0), None),
            // A ray-tracing structure in a buffer of the host: read raw.
            Fields::Image { .. } if un.starts_with("image_bvh") => ((11, 0), None),
            Fields::Image { dim, .. } => {
                let kind = match dim {
                    0 => 1,
                    1 => 2,
                    2 => 4,
                    4 => 6,
                    5 => 7,
                    _ => return Err("an image dimension (cube or MSAA)"),
                };
                ((kind | u, 0x409), Some(kind))
            }
            Fields::Buffer { idxen, format, .. } => {
                let stride = used.map_or(0, |u| u.stride);
                let counted = inst
                    .write(Reg::S(code + first + 2))
                    .is_some_and(|v| self.read_as_number(v));
                let props = if un.starts_with("buffer_load_format")
                    || un.starts_with("buffer_store_format")
                {
                    (10 | u, 0x409)
                } else if !counted
                    && *idxen
                    && format.is_none()
                    && stride >= 4
                    && stride.is_multiple_of(4)
                {
                    // Nothing reads the count: the query is cleaned up.
                    (12 | u, stride)
                } else {
                    // Structured ones too: the host's views of them are raw
                    // (see `buffer`), which count bytes, and a size asked
                    // for as a structured view's is that count on AMD.
                    (11 | u, 0)
                };
                if used.is_some_and(|u| u.kind == ResourceType::Cbv) {
                    self.stale.extend(inst.writes.iter().map(|w| w.1));
                    return Err("a constant buffer's descriptor read as data");
                }
                (props, None)
            }
            // Read by scalar buffer loads alone: the raw view `smem` reads it through.
            Fields::Smem { .. } if used.is_some_and(|u| u.kind == ResourceType::Srv) => {
                ((11, 0), None)
            }
            // A constant buffer: nothing of its descriptor is a number.
            Fields::Smem { .. } => {
                self.stale.extend(inst.writes.iter().map(|w| w.1));
                return Err("a constant buffer's descriptor read as data");
            }
            _ => return Err("a scalar memory read as data"),
        };
        (first, props, image, uav, used)
        };
        let src = Source::Loaded { load: site, first };
        let h = self.handle(site, &src, false, props)?;
        let c = self.cur;
        let i32t = self.s.t.i32;
        let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
        let mip = if image.is_some() && !uav {
            self.s.m.i32(0)
        } else {
            self.s.m.undef(i32t)
        };
        let d = self
            .s
            .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, mip]);
        let field = |l: &mut Self, k: u32| l.s.m.extract(c, d, k);
        let words: Vec<(u32, V)> = match image {
            Some(kind) => {
                let one = self.s.m.i32(1);
                let w = field(self, 0);
                let (height, depth) = match kind {
                    1 => (one, one),
                    2 => (field(self, 1), one),
                    6 => (one, field(self, 1)),
                    _ => (field(self, 1), field(self, 2)),
                };
                let levels = if uav { one } else { field(self, 3) };
                let less = |l: &mut Self, x: V| l.s.m.bin(c, Bin::Sub, x, one);
                let (w, height, depth, levels) = (
                    less(self, w),
                    less(self, height),
                    less(self, depth),
                    less(self, levels),
                );
                let k = |l: &mut Self, v: u32| l.s.m.i32(v);
                let (k30, k2, k14, k16, mask) = (
                    k(self, 30),
                    k(self, 2),
                    k(self, 14),
                    k(self, 16),
                    k(self, 0x3FFF),
                );
                let d1 = self.s.m.bin(c, Bin::Shl, w, k30);
                let hi = self.s.m.bin(c, Bin::LShr, w, k2);
                let hi = self.s.m.bin(c, Bin::And, hi, mask);
                let ht = self.s.m.bin(c, Bin::Shl, height, k14);
                let d2 = self.s.m.bin(c, Bin::Or, hi, ht);
                let ty = [0, 8, 9, 0, 10, 0, 12, 13][kind as usize];
                let base = k(self, 0xFAC | ty << 28);
                let last = self.s.m.bin(c, Bin::Shl, levels, k16);
                let d3 = self.s.m.bin(c, Bin::Or, base, last);
                vec![(1, d1), (2, d2), (3, d3), (4, depth)]
            }
            None => {
                // A raw view counts bytes; the descriptor's record count is in elements.
                let n = field(self, 0);
                let stride = used.map_or(0, |u| u.stride);
                let counted = inst
                    .write(Reg::S(code + first + 2))
                    .is_some_and(|v| self.read_as_number(v));
                let n = if props.0 & 0xFF == 11 && stride > 1 {
                    let k = self.s.m.i32(stride);
                    self.s.m.bin(c, Bin::UDiv, n, k)
                } else if props.0 & 0xFF == 11
                    && counted
                    && let Some(stride) = self.slot_stride(&src)?
                {
                    // Picked from the heap: the host's table has the stride.
                    let (i32t, one) = (self.s.t.i32, self.s.m.i32(1));
                    let k = self.s.op(c, "binary.i32", Attr::ReadNone, i32t, 39, &[stride, one]);
                    self.s.m.bin(c, Bin::UDiv, n, k)
                } else {
                    n
                };
                let mut words = vec![(2, n)];
                // Its address too, where the code computes with it and the
                // host can tell it.
                let computed = (0..2u8).any(|k| {
                    inst.write(Reg::S(code + first + k))
                        .is_some_and(|v| self.computed_with(v, 6))
                });
                // (A descriptor nothing uses as one gets it whenever the host can tell it, as
                // a buffer whose address alone is read does.)
                if first == 0
                    && (computed || unused.is_some())
                    && let Some((lo, hi)) = self.view_address(site)
                {
                    words.extend([(0, lo), (1, hi)]);
                }
                words
            }
        };
        for k in 0..count {
            let rel = k.wrapping_sub(u32::from(first));
            match words.iter().find(|w| w.0 == rel) {
                Some(&(_, x)) => self.write(inst, Reg::S(code + k as u8), x, None),
                None => {
                    if let Some(v) = inst.write(Reg::S(code + k as u8)) {
                        self.vals.insert(v, Val::Missing(OTHER));
                        self.stale.insert(v);
                    }
                }
            }
        }
        Ok(())
    }

    /// What [`Self::descriptor_data`] makes of a descriptor load that nothing uses as a
    /// descriptor but whose dwords the code reads: the first dword, the handle properties,
    /// the image kind (none for a buffer), whether it is an unordered view, and its
    /// used-resource entry. An image's eight dwords read as a 2D texture's, a buffer's four
    /// as a raw view's where the code reads its record count (dword 2); none for anything
    /// else (a constant buffer, a sampler, a buffer whose address alone is read).
    // shortcut: an image whose kind nothing tells is taken for a 2D texture (the title's
    // reads are texel sizes of 2D textures: puddle masks, shadow masks); a host view of
    // another kind would answer its own sizes.
    #[allow(clippy::type_complexity)]
    pub(in crate::emit::lower) fn unused_descriptor_kind(
        &self,
        inst: &Inst,
        site: Site,
        code: u8,
        count: u32,
    ) -> Option<(u8, (u32, u32), Option<u32>, bool, Option<&'a UsedResource>)> {
        // Kept per lane and compared (`Resources::lane_copied`), or a sampler's: the cases
        // `descriptor_data` handles on their own.
        if self.x.res.lane_copied.contains(&site)
            || self.x.res.descriptors.iter().any(|d| {
                d.role == Role::Sampler
                    && d.source.leaves().into_iter().any(
                        |l| matches!(*l, Source::Loaded { load, .. } if load == site),
                    )
            })
        {
            return None;
        }
        let used = self.used_kind(&Source::Loaded {
            load: site,
            first: 0,
        });
        if used.is_some_and(|u| !matches!(u.kind, ResourceType::Srv | ResourceType::Uav)) {
            return None;
        }
        let read = |k: u32| {
            inst.write(Reg::S(code + k as u8)).is_some_and(|v| {
                self.x
                    .p
                    .blocks
                    .iter()
                    .flat_map(|b| &b.insts)
                    .any(|i| i.reads.iter().any(|r| r.1 == v))
            })
        };
        let uav = used.is_some_and(|u| u.kind == ResourceType::Uav);
        let u = if uav { 0x1000 } else { 0 };
        match count {
            // width (dwords 1-2), height (2), levels (3), depth (4)
            8 if (1..5).any(read) && used.is_none_or(|u| u.dwords == 8) => {
                Some((0, (2 | u, 0x409), Some(2), uav, used))
            }
            // the record count
            4 if read(2) && used.is_none_or(|u| u.dwords == 4) => {
                Some((0, (11 | u, 0), None, uav, used))
            }
            _ => None,
        }
    }
}
