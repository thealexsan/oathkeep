use super::*;

impl<'a> Lower<'a> {
    /// Whether some instruction reads `v` as a number (or may, through a
    /// join), rather than in a descriptor operand.
    pub(in crate::emit::lower) fn read_as_number(&self, v: Value) -> bool {
        self.x.p.blocks.iter().any(|b| {
            b.phis.iter().any(|p| p.args.contains(&v))
                || b.insts.iter().any(|i| {
                    matches!(i.fields, Fields::Sop { .. } | Fields::Valu(_))
                        && i.reads.iter().any(|r| r.1 == v)
                })
        })
    }

    /// Whether some instruction computes with `v` (scalar or vector
    /// arithmetic), rather than only naming it in a descriptor operand,
    /// moving it (at most `depth` moves are followed) or comparing it: a
    /// waterfall loop keeps a descriptor's dwords per lane, reads the first
    /// lane's back and compares the lanes' with them.
    pub(in crate::emit::lower) fn computed_with(&self, v: Value, depth: u32) -> bool {
        self.x.p.blocks.iter().flat_map(|b| &b.insts).any(|i| {
            if !i.reads.iter().any(|r| r.1 == v) {
                return false;
            }
            let name = i.name();
            let moves = matches!(
                name,
                "s_mov_b32"
                    | "s_mov_b64"
                    | "v_mov_b32"
                    | "v_readfirstlane_b32"
                    | "v_readlane_b32"
                    | "v_writelane_b32"
                    | "v_cndmask_b32"
            );
            match &i.fields {
                Fields::Sop { .. } | Fields::Valu(_) if moves => {
                    depth > 0 && i.writes.iter().any(|w| self.computed_with(w.1, depth - 1))
                }
                Fields::Sop { .. } | Fields::Valu(_) => {
                    !(name.starts_with("v_cmp") || name.starts_with("s_cmp"))
                }
                _ => false,
            }
        })
    }

    /// Whether the image `src` names is one of 32-bit integers (one channel):
    /// the program has an integer atomic on the same table entry, which
    /// takes no other view.
    pub(in crate::emit::lower) fn is_counter(&self, src: &Source) -> bool {
        let Some(this) = self.entry(src) else {
            return false;
        };
        self.x.res.descriptors.iter().any(|d| {
            let i = &self.x.p.blocks[d.site.0].insts[d.site.1];
            d.role == Role::Resource
                && matches!(i.fields, Fields::Image { dmask: 1, .. })
                && matches!(
                    i.name(),
                    "image_atomic_add"
                        | "image_atomic_sub"
                        | "image_atomic_and"
                        | "image_atomic_or"
                        | "image_atomic_xor"
                        | "image_atomic_smin"
                        | "image_atomic_smax"
                        | "image_atomic_umin"
                        | "image_atomic_umax"
                )
                && self.entry(&d.source) == Some(this)
        })
    }

    /// Which table entry `src` is loaded from: its root parameter and byte.
    pub(in crate::emit::lower) fn entry(&self, src: &Source) -> Option<(usize, u32)> {
        let Source::Loaded { load, first } = src else {
            return None;
        };
        let a = self.scalar.get(load).filter(|a| a.dynamic.is_none())?;
        Some((a.param, a.offset + 4 * u32::from(*first)))
    }

    /// See [`Lower::coherent`].
    pub(in crate::emit::lower) fn find_coherent(&mut self) {
        let found: Vec<_> = self
            .x
            .res
            .descriptors
            .iter()
            .filter(|d| {
                let i = &self.x.p.blocks[d.site.0].insts[d.site.1];
                matches!(&i.fields, Fields::Buffer { cache, .. } if cache.glc)
                    && i.name().starts_with("buffer_load_dword")
                    && self.used_kind(&d.source).is_some_and(|u| u.kind == ResourceType::Uav)
            })
            .filter_map(|d| self.entry(&d.source))
            .collect();
        self.coherent.extend(found);
    }

    /// The kind of a view no instruction uses (in this program, or through
    /// another load of its address), by what the code reads of the
    /// descriptor the load `inst` at `site` writes to the SGPRs from `code`:
    /// `Some(true)` for a 2D image, read only for its size, `Some(false)`
    /// for a structured buffer, read only for its record count.
    ///
    /// - Eight dwords are an image's. `GetDimensions` of a 2D one reads the
    ///   width (dwords 1 and 2), the height (dword 2, bits 14-29, with one
    ///   `s_bfe_u32`) and the base level or the last (dword 3); of an array,
    ///   a cube or a volume also dword 4.
    /// - Four dwords at a table entry the used resources list as an SRV or
    ///   UAV are a buffer's. A stride above 4 bytes there is an element's
    ///   (a typed or raw view has at most 4): a test for a null view reads
    ///   the record count, nothing else.
    pub(in crate::emit::lower) fn sized_view(&self, inst: &Inst, site: Site, code: u8, count: u32) -> Option<bool> {
        let dword = |k: u8| inst.write(Reg::S(code + k));
        let read = |v: Value| {
            self.x.p.blocks.iter().any(|b| {
                b.insts.iter().any(|i| i.reads.iter().any(|r| r.1 == v))
                    || b.phis.iter().any(|p| p.args.contains(&v))
            })
        };
        let only = |allowed: &[u8]| {
            (0..count as u8).all(|k| allowed.contains(&k) || !dword(k).is_some_and(read))
        };
        match count {
            8 => {
                const HEIGHT: u32 = 16 << 16 | 14;
                let v = dword(2)?;
                let height = self.x.p.blocks.iter().flat_map(|b| &b.insts).any(|i| {
                    i.name() == "s_bfe_u32"
                        && i.reads.iter().any(|r| r.1 == v)
                        && matches!(&i.fields, Fields::Sop { src, .. }
                            if matches!(src.get(1), Some(Operand::Literal(HEIGHT))))
                });
                (height && only(&[1, 2, 3])).then_some(true)
            }
            4 => {
                let used = self.used_kind(&Source::Loaded { load: site, first: 0 })?;
                let a = self.scalar.get(&site)?;
                let buffer = used.dwords == 4
                    && a.dynamic.is_none()
                    && used.table_offset_dwords == a.offset / 4
                    && matches!(used.kind, ResourceType::Srv | ResourceType::Uav)
                    && used.stride > 4
                    && used.stride.is_multiple_of(4);
                (buffer && only(&[2])).then_some(false)
            }
            _ => None,
        }
    }

    /// The title address (low dword, bits from 32 up) of the buffer view
    /// whose descriptor the scalar load at `site` reads from the start of
    /// a table entry, as the host tells it when it binds (`shader.rs`
    /// `USER_VIEW_ADDRESS`); none for anything else. (The console's
    /// descriptor holds more in the second dword, the stride from bit 16
    /// up; code that takes the address masks those away.)
    pub(in crate::emit::lower) fn view_address(&mut self, site: Site) -> Option<(V, V)> {
        use crate::emit::shader::USER_VIEW_ADDRESS;
        let a = self.scalar.get(&site).copied()?;
        let rp = *self.x.native.layout.get(a.param)?;
        if rp.root != RootType::Table || a.dynamic.is_some() || a.offset % 32 != 0 {
            return None;
        }
        let dword = a.offset / 4;
        let used = self
            .x
            .native
            .used
            .iter()
            .filter(|u| {
                u.root == RootType::Table
                    && u.user_data == rp.user_data
                    && u.table_offset_dwords <= dword
            })
            .max_by_key(|u| u.table_offset_dwords)?;
        if used.dwords != 4 || !matches!(used.kind, ResourceType::Srv | ResourceType::Uav) {
            return None;
        }
        let lo = self.host_word(RootType::Other(USER_VIEW_ADDRESS), rp.slot, dword);
        let hi = self.host_word(RootType::Other(USER_VIEW_ADDRESS + 1), rp.slot, dword);
        Some((lo, hi))
    }

    /// `lo`, `hi` plus the instruction's byte offset `off`, with the carry.
    pub(in crate::emit::lower) fn address_plus(&mut self, lo: V, hi: V, off: i32) -> (V, V) {
        if off == 0 {
            return (lo, hi);
        }
        let c = self.cur;
        let i32t = self.s.t.i32;
        let k = self.s.m.i32(off as u32);
        let sum = self.s.m.bin(c, Bin::Add, lo, k);
        // Up: a carry when the sum wrapped below `lo`. Down: a borrow when
        // it wrapped above.
        let wrapped = self
            .s
            .m
            .cmp(c, if off > 0 { pred::IULT } else { pred::IUGT }, sum, lo);
        let one = self.s.m.cast(c, Cast::ZExt, wrapped, i32t);
        let hi = self.s.m.bin(c, if off > 0 { Bin::Add } else { Bin::Sub }, hi, one);
        (sum, hi)
    }

    /// Title memory at the 40-bit address `lo`, `hi` (`shader.rs`
    /// `SLOT_MEMORY`): the handle of the raw view of the host copy holding
    /// it and the byte in that view. A page without a copy reads zeros
    /// (and drops writes) through the null view at the first slot, and its
    /// number is left for the host, for lanes that run (`guard`). Goes on in
    /// another block.
    /// With `store`, a page that is not to be written ([`crate::emit::shader::MEMORY_READ_ONLY`])
    /// is the null view.
    pub(in crate::emit::lower) fn memory(&mut self, lo: V, hi: V, guard: Option<V>, store: bool) -> (V, V) {
        use crate::emit::shader::{MEMORY_FAULTS, MEMORY_PAGES, MEMORY_READ_ONLY, SLOT_FAULTS, SLOT_MEMORY, USER_MEMORY_BASE};
        let c = self.cur;
        let (i32t, res) = (self.s.t.i32, self.s.t.res_ret);
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let (k16, k8) = (k(self, 16), k(self, 0xFF));
        let low = self.s.m.bin(c, Bin::LShr, lo, k16);
        let high = self.s.m.bin(c, Bin::And, hi, k8);
        let high = self.s.m.bin(c, Bin::Shl, high, k16);
        let page = self.s.m.bin(c, Bin::Or, low, high);
        // Inside the directory whatever the address (lanes that do not run
        // compute one too); its last entry is never filled.
        let last = k(self, MEMORY_PAGES - 1);
        let page = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[page, last]);
        self.s.m.tag(page, Tag::Clamp);
        let k3 = k(self, 3);
        let at = self.s.m.bin(c, Bin::Shl, page, k3);
        let directory = self.s.root_srv_handle(SLOT_MEMORY);
        let (iu, both, align) = (self.s.m.undef(i32t), self.s.m.i8(3), k(self, 4));
        let entry = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            res,
            139,
            &[directory, at, iu, both, align],
        );
        let (copy, start) = (self.s.m.extract(c, entry, 0), self.s.m.extract(c, entry, 1));
        let (number, start) = (self.as_int(copy), self.as_int(start));
        let mask = k(self, !MEMORY_READ_ONLY);
        let copy = self.s.m.bin(c, Bin::And, number, mask);
        // A page without a copy: its number, at a place by the number. In a
        // branch: the notes lie in the PC's memory, and a store there with
        // every node of every ray would be most of what a trace does.
        let zero = k(self, 0);
        let mut missing = self.s.m.cmp(c, pred::EQ, copy, zero);
        if let Some(g) = guard {
            missing = self.s.m.bin(c, Bin::And, missing, g);
        }
        let places = k(self, MEMORY_FAULTS - 1);
        let faults = self.s.root_uav_handle(SLOT_FAULTS);
        let noting = self.begin_guarded(Some(missing));
        let c = self.cur;
        let place = self.s.m.bin(c, Bin::URem, page, places);
        let one = k(self, 1);
        let place = self.s.m.bin(c, Bin::Add, place, one);
        let k2 = k(self, 2);
        let place = self.s.m.bin(c, Bin::Shl, place, k2);
        let void = self.s.t.void;
        let mask = self.s.m.i8(1);
        self.s.op(
            c,
            "rawBufferStore.i32",
            Attr::NoUnwind,
            void,
            140,
            &[faults, place, iu, page, iu, iu, iu, mask, align],
        );
        self.end_guarded(noting, &[]);
        let c = self.cur;
        self.page_there = Some(self.s.m.cmp(c, pred::NE, copy, zero));
        let copy = if store {
            let closed = self.s.m.cmp(c, pred::SLT, number, zero);
            self.s.m.select(c, closed, zero, copy)
        } else {
            copy
        };
        let base = self.host_word(RootType::Other(USER_MEMORY_BASE), 0, 0);
        let slot = self.s.m.bin(c, Bin::Add, base, copy);
        // The builder's threads read what others of the dispatch wrote (a
        // refit climbs from children to parents): globally coherent.
        let coherent = if crate::spill::is_spilled(self.x.native) { 0x4000 } else { 0 };
        let h = self.s.heap_handle(c, slot, false, true, (11 | 0x1000 | coherent, 0));
        let k64k = k(self, 0xFFFF);
        let within = self.s.m.bin(c, Bin::And, lo, k64k);
        let byte = self.s.m.bin(c, Bin::Add, start, within);
        (h, byte)
    }

    /// The title address in the root view `src` is built around, in a program
    /// with a spilled user-data table (`shader.rs` `USER_ROOT_ADDRESS`).
    pub(in crate::emit::lower) fn root_address(&mut self, src: &Source) -> Option<(V, V)> {
        use crate::emit::shader::USER_ROOT_ADDRESS;
        let Source::Built { address, .. } = src else {
            return None;
        };
        let rp = *self.x.native.layout.get(address.param)?;
        if rp.root != RootType::Descriptor || !crate::spill::is_spilled(self.x.native) {
            return None;
        }
        let used = self.x.native.used.iter().find(|u| u.user_data == rp.user_data)?;
        if !matches!(used.kind, ResourceType::Srv | ResourceType::Uav) {
            return None;
        }
        let lo = self.host_word(RootType::Other(USER_ROOT_ADDRESS), rp.slot, 0);
        let hi = self.host_word(RootType::Other(USER_ROOT_ADDRESS + 1), rp.slot, 0);
        Some((lo, hi))
    }

    /// For an access through such a root view: the raw view of the host copy
    /// and the byte in it of the view's byte `off` (signed: an offset below
    /// the address stays below it). [`Lower::handle`] gives the same view for
    /// the rest of the instruction. None for any other source. Goes on in
    /// another block.
    /// shortcut: an access of several dwords is taken not to cross the end
    /// of a host copy (the builder's records lie on their size).
    pub(in crate::emit::lower) fn root_memory(&mut self, src: &Source, off: V, guard: Option<V>, store: bool) -> Option<(V, V)> {
        let (lo, hi) = self.root_address(src)?;
        let c = self.cur;
        let i32t = self.s.t.i32;
        let sum = self.s.m.bin(c, Bin::Add, lo, off);
        let wrapped = self.s.m.cmp(c, pred::IULT, sum, lo);
        let carry = self.s.m.cast(c, Cast::ZExt, wrapped, i32t);
        let k31 = self.s.m.i32(31);
        let sign = self.s.m.bin(c, Bin::AShr, off, k31);
        let hi = self.s.m.bin(c, Bin::Add, hi, carry);
        let mut hi = self.s.m.bin(c, Bin::Add, hi, sign);
        let mut sum = sum;
        if crate::emit::builder_bounds() {
            // No structure is near 64 MiB (the greatest seen has 14). One
            // whose memory was written over says it has gigabytes, and the
            // builder's copy of it went through all other structures' memory
            // after it and, past 2 GiB, before it. Nothing is there instead.
            let (below, all, zero) = (self.s.m.i32(16 << 20), self.s.m.i32(80 << 20), self.s.m.i32(0));
            let at = self.s.m.bin(c, Bin::Add, off, below);
            let near = self.s.m.cmp(c, pred::IULT, at, all);
            sum = self.s.m.select(c, near, sum, zero);
            hi = self.s.m.select(c, near, hi, zero);
        }
        let (h, byte) = self.memory(sum, hi, guard, store);
        self.root_copy = Some(h);
        Some((h, byte))
    }
}
