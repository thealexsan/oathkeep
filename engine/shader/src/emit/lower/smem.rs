use super::*;

impl<'a> Lower<'a> {
    /// Whether the dwords of one console load are read by one call for up
    /// to four of them (`bitcode::opt::WIDE` of `OATHKEEP_SHADER_OPT`), not
    /// by a call each.
    pub(in crate::emit::lower) fn wide_loads() -> bool {
        crate::emit::shader_opt() & crate::emit::bitcode::opt::WIDE != 0
    }

    /// `count` dwords from byte `addr` on of the raw view `h`, as integers,
    /// read in the current block by one call for every four of them
    /// ([`Self::wide_loads`]). They are the dwords of ONE console load,
    /// which the console reads as one (address plus 4, 8, ... without
    /// wrapping at 32 bits): the same bytes as a call a dword reads. Loads
    /// of different console instructions are never joined: the program
    /// computes their offsets in 32-bit registers, and `x - 4` and `x` at
    /// `x = 0` are one dword out of the view and its first one, where one
    /// call from `x - 4` on would read both out of it.
    pub(in crate::emit::lower) fn raw_dwords(&mut self, h: V, addr: V, count: u32) -> Vec<V> {
        let c = self.cur;
        let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let mut out = Vec::with_capacity(count as usize);
        for first in (0..count).step_by(4) {
            let n = (count - first).min(4);
            let off = self.s.m.i32(4 * first);
            let a = self.s.m.bin(c, Bin::Add, addr, off);
            let mask = self.s.m.i8((1u8 << n) - 1);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[h, a, u, mask, al],
            );
            for k in 0..n {
                let x = self.s.m.extract(c, r, k);
                out.push(self.as_int(x));
            }
        }
        out
    }

    pub(in crate::emit::lower) fn smem(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        let Fields::Smem {
            data,
            offset,
            soffset,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        self.root_copy = None;
        let name = inst.name();
        if let Some(a) = self.scalar.get(&site).copied().filter(|a| a.param == CODE) {
            return self.code_smem(inst, a);
        }
        if self.scalar.get(&site).is_some_and(|a| a.param == MEMORY) {
            return self.memory_smem(inst);
        }
        let Some(rest) = name.strip_prefix("s_buffer_load_dword") else {
            return self.descriptor_data(inst, site);
        };
        let count = match rest {
            "" => 1,
            "x2" => 2,
            "x4" => 4,
            "x8" => 8,
            "x16" => 16,
            _ => return Err("a scalar buffer load"),
        };
        let Some(Operand::Scalar { code, .. }) = *data else {
            return Err("a scalar buffer load");
        };
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked descriptor")?;
        let kind = self.used_kind(src);
        let c = self.cur;
        let (base, extra) = self.built_offset(src)?;
        let offset = &((*offset as u32).wrapping_add(base) as i32);
        let mut addr = self.s.m.i32(*offset as u32);
        if let Some(x) = extra {
            addr = self.s.m.bin(c, Bin::Add, addr, x);
        }
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            let so = self.operand(inst, soffset)?;
            addr = self.s.m.bin(c, Bin::Add, addr, so);
        }
        if matches!(src, Source::Built { address, .. } if address.param == CODE) {
            // A descriptor built around a table in the code.
            for k in 0..count {
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, addr, off);
                let x = self.code_dword(a)?;
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        // A buffer the program picks from the view heap by an index it
        // computes (`ResourceHeap`) has no used-resource entry, and the
        // heap holds a buffer view there, not a constant buffer: read as
        // raw bytes, as a table's SRV is below. Read as a constant buffer
        // (the host's validation: "Descriptor Type in Heap: SRV, Register
        // Type: CBV", heap index 17) the loads gave nothing:
        // the title's meshlet culling (BatchCullMeshlets,
        // BatchCullPreSkinnedMeshlets, BatchCullBatchCSMeshlets) reads its
        // batch records, mesh headers and instance transforms this way, so
        // it passed no meshlet and every indirect mesh draw ran empty.
        // The host's table of strides by heap slot tells a buffer view
        // (not 0) from anything else, which is still read as a constant
        // buffer; only the load that fits the slot runs.
        let picked = match kind {
            None => self.slot_stride(src)?,
            Some(_) => None,
        };
        if let Some(stride) = picked {
            let zero = self.s.m.i32(0);
            let is_buffer = self.s.m.cmp(self.cur, pred::NE, stride, zero);
            let at = self.begin_guarded(Some(is_buffer));
            let h = self.handle(site, src, false, (11, 0))?;
            let mut raw = Vec::new();
            let wide = Self::wide_loads();
            if wide {
                let dwords = self.raw_dwords(h, addr, count);
                raw.extend(dwords.into_iter().map(|x| (x, zero)));
            }
            for k in 0..if wide { 0 } else { count } {
                let b = self.cur;
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(b, Bin::Add, addr, off);
                let mask = self.s.m.i8(1);
                let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
                let r = self.s.op(
                    b,
                    "rawBufferLoad.f32",
                    Attr::ReadOnly,
                    self.s.t.res_ret,
                    139,
                    &[h, a, u, mask, al],
                );
                let x = self.s.m.extract(b, r, 0);
                let x = self.as_int(x);
                raw.push((x, zero));
            }
            let raw = self.end_guarded(at, &raw);
            let other = self.s.m.cmp(self.cur, pred::EQ, stride, zero);
            let at = self.begin_guarded(Some(other));
            let h = self.handle(site, src, false, (13, 0x1_0000))?;
            let mut both = Vec::new();
            for (k, &r) in raw.iter().enumerate() {
                let b = self.cur;
                let off = self.s.m.i32(4 * k as u32);
                let a = self.s.m.bin(b, Bin::Add, addr, off);
                let x = self.cbuffer_dword(h, a);
                both.push((x, r));
            }
            let both = self.end_guarded(at, &both);
            for (k, x) in both.into_iter().enumerate() {
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        if matches!(kind, Some(u) if u.kind == ResourceType::Srv) {
            // The size in a structure's header (its second dword), which a
            // copy of it loops over.
            let size = *offset == 4
                && count == 1
                && extra.is_none()
                && matches!(soffset, Operand::Scalar { code: NULL, .. })
                && crate::emit::builder_bounds();
            let mut root = false;
            let h = match self.root_memory(src, addr, None, false) {
                Some((h, byte)) => {
                    addr = byte;
                    root = true;
                    h
                }
                None => self.handle(site, src, false, (11, 0))?,
            };
            if Self::wide_loads() {
                for (k, x) in self.raw_dwords(h, addr, count).into_iter().enumerate() {
                    self.write(inst, Reg::S(code + k as u8), x, None);
                }
                return Ok(());
            }
            let c = self.cur;
            for k in 0..count {
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, addr, off);
                let mask = self.s.m.i8(1);
                let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
                let r = self.s.op(
                    c,
                    "rawBufferLoad.f32",
                    Attr::ReadOnly,
                    self.s.t.res_ret,
                    139,
                    &[h, a, u, mask, al],
                );
                let x = self.s.m.extract(c, r, 0);
                let mut x = self.as_int(x);
                if size && root {
                    let (most, zero) = (self.s.m.i32(1 << 20), self.s.m.i32(0));
                    let sane = self.s.m.cmp(c, pred::IULT, x, most);
                    x = self.s.m.select(c, sane, x, zero);
                }
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        let h = self.handle(site, src, false, (13, 0x1_0000))?;
        let constant = match addr {
            V::Const(_)
                if matches!(soffset, Operand::Scalar { code: NULL, .. }) && extra.is_none() =>
            {
                Some(*offset as u32)
            }
            _ => None,
        };
        let mut rows: HashMap<u32, V> = HashMap::new();
        for k in 0..count {
            let x = match constant {
                Some(base) => {
                    let byte = base + 4 * k;
                    let row = match rows.get(&(byte >> 4)) {
                        Some(&r) => r,
                        None => {
                            let ri = self.s.m.i32(byte >> 4);
                            let r = self.s.op(
                                c,
                                "cbufferLoadLegacy.i32",
                                Attr::ReadOnly,
                                self.s.t.cbuf_ret,
                                59,
                                &[h, ri],
                            );
                            rows.insert(byte >> 4, r);
                            r
                        }
                    };
                    self.s.m.extract(c, row, (byte >> 2) & 3)
                }
                None => {
                    let off = self.s.m.i32(4 * k);
                    let a = self.s.m.bin(c, Bin::Add, addr, off);
                    self.cbuffer_dword(h, a)
                }
            };
            self.write(inst, Reg::S(code + k as u8), x, None);
        }
        Ok(())
    }

    /// A descriptor load (`s_load_dword*` from a table or heap) whose dwords
    /// the code also reads as numbers: the console compiler computes sizes
    /// from descriptor bits (an image's width, height, depth, levels; a
    /// buffer's record count) instead of querying them. Those fields are
    /// rebuilt from the host view's dimensions in the GFX10 layout (image
    /// dword 1 bits 30-31 and dword 2 bits 0-13 width - 1, dword 2 bits
    /// 14-29 height - 1, dword 3 identity swizzle, last level at 16, type at

    /// `s_load_dword*` from the program's constant table.
    pub(in crate::emit::lower) fn code_smem(&mut self, inst: &Inst, a: Address) -> Result<(), Why> {
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar load");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u32,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar load from the code"),
        };
        let c = self.cur;
        let mut addr = self.s.m.i32(a.offset);
        if let Some(value) = a.dynamic {
            let mut d = self.i32_of(value)?;
            // A term held per lane reached the address through
            // `v_readfirstlane_b32`: the first lane's.
            let per_lane = self.def_block.get(&value).is_some_and(|&b| {
                let writes = |i: &Inst| i.writes.iter().any(|w| w.1 == value && matches!(w.0, Reg::V(_)));
                self.x.p.blocks[b].insts.iter().any(writes)
            });
            if per_lane {
                self.s.flags |= flags::WAVE_OPS;
                let i32t = self.s.t.i32;
                let first = self.s.op(c, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[d]);
                d = if self.s.pixel && crate::emit::helper_own() {
                    let helper = self.helper_lane();
                    self.s.m.select(c, helper, d, first)
                } else {
                    first
                };
            }
            addr = self.s.m.bin(c, Bin::Add, addr, d);
        }
        for k in 0..count {
            let off = self.s.m.i32(4 * k);
            let at = self.s.m.bin(c, Bin::Add, addr, off);
            let x = self.code_dword(at)?;
            self.write(inst, Reg::S(code + k as u8), x, None);
        }
        Ok(())
    }

    /// The dword at a computed byte address of a constant buffer.
    pub(in crate::emit::lower) fn cbuffer_dword(&mut self, h: V, a: V) -> V {
        let c = self.cur;
        let four = self.s.m.i32(4);
        let ri = self.s.m.bin(c, Bin::LShr, a, four);
        let r = self.s.op(
            c,
            "cbufferLoadLegacy.i32",
            Attr::ReadOnly,
            self.s.t.cbuf_ret,
            59,
            &[h, ri],
        );
        let two = self.s.m.i32(2);
        let three = self.s.m.i32(3);
        let comp = self.s.m.bin(c, Bin::LShr, a, two);
        let comp = self.s.m.bin(c, Bin::And, comp, three);
        let mut x = self.s.m.extract(c, r, 0);
        for j in 1..4 {
            let e = self.s.m.extract(c, r, j);
            let jc = self.s.m.i32(j);
            let is = self.s.m.cmp(c, pred::EQ, comp, jc);
            x = self.s.m.select(c, is, e, x);
        }
        x
    }
}
