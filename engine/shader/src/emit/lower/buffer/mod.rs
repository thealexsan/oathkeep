mod narrow;
mod store_atomic;

use super::*;

impl<'a> Lower<'a> {
    pub(in crate::emit::lower) fn buffer(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        self.root_copy = None;
        let Fields::Buffer {
            data,
            addr,
            soffset,
            offset,
            offen,
            idxen,
            format,
            lds,
            tfe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let name = inst.name();
        if name == "buffer_gl0_inv" || name == "buffer_gl1_inv" {
            // The host has no console buffer cache to invalidate.
            return Ok(());
        }
        if *lds || *tfe {
            return Err("a buffer access to LDS or with tfe");
        }
        if format.is_some() && !name.starts_with("tbuffer_load_format_") {
            return Err(name);
        }
        let site = (b, i);
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let store = name.starts_with("buffer_store_");
        // DXIL atomic operations: add, and, or, xor, signed and unsigned
        // min and max, exchange; compare-exchange apart.
        let atomic = match name.strip_prefix("buffer_atomic_") {
            None => None,
            Some("add") => Some(0),
            Some("and") => Some(1),
            Some("or") => Some(2),
            Some("xor") => Some(3),
            Some("smin") => Some(4),
            Some("smax") => Some(5),
            Some("umin") => Some(6),
            Some("umax") => Some(7),
            Some("swap") => Some(8),
            Some("cmpswap") => Some(u32::MAX),
            Some(_) => return Err(name),
        };
        // Byte and 16-bit accesses: bits, and whether a load sign-extends.
        let narrow = match name {
            "buffer_load_ubyte" | "buffer_store_byte" => Some((8, false)),
            "buffer_load_sbyte" => Some((8, true)),
            "buffer_load_ushort" | "buffer_store_short" => Some((16, false)),
            "buffer_load_sshort" => Some((16, true)),
            _ => None,
        };
        // d16: two 16-bit channels per VGPR, low half first (buffer and
        // tbuffer formats alike).
        let d16 = name.contains("_format_d16_");
        let plain = name.replace("_format_d16_", "_format_");
        let (typed, count) = match plain
            .strip_prefix("buffer_load_")
            .or(plain.strip_prefix("buffer_store_"))
            .or(plain.strip_prefix("tbuffer_load_"))
        {
            _ if atomic.is_some() || narrow.is_some() => (false, 1),
            Some("dword") => (false, 1),
            Some("dwordx2") => (false, 2),
            Some("dwordx3") => (false, 3),
            Some("dwordx4") => (false, 4),
            Some("format_x") => (true, 1),
            Some("format_xy") => (true, 2),
            Some("format_xyz") => (true, 3),
            Some("format_xyzw") => (true, 4),
            _ => return Err(name),
        };
        let Operand::Vector { reg: areg, .. } = *addr else {
            return Err("a buffer address");
        };
        let vaddr = |l: &mut Self, k: u8| -> Result<V, Why> {
            let v = inst.read(Reg::V(areg + k)).ok_or("a buffer address")?;
            l.i32_of(v)
        };
        let index = if *idxen { Some(vaddr(self, 0)?) } else { None };
        let mut off = self.s.m.i32(u32::from(*offset));
        if *offen {
            let o = vaddr(self, u8::from(*idxen))?;
            off = self.s.m.bin(c, Bin::Add, off, o);
        }
        if !matches!(
            soffset,
            Operand::Scalar { code: NULL, .. } | Operand::Const(128)
        ) {
            let so = self.operand(inst, soffset)?;
            off = self.s.m.bin(c, Bin::Add, off, so);
        }
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked buffer")?;
        let used = self.used_kind(src);
        let (base, extra) = self.built_offset(src)?;
        if base != 0 {
            let b = self.s.m.i32(base);
            off = self.s.m.bin(c, Bin::Add, off, b);
        }
        if let Some(x) = extra {
            off = self.s.m.bin(c, Bin::Add, off, x);
        }
        // A buffer the program picks from the view heap by an index it
        // computes: no used-resource entry gives its stride, the host's
        // table by heap slot does; the access becomes a raw one at the
        // element's byte. (Also tells the host this shader reads such
        // buffers.)
        let mut index = index;
        if let Some(stride) = self.slot_stride(src)?
            && let Some(element) = index
            && !typed
            && used.is_none()
        {
            let at = self.s.m.bin(c, Bin::Mul, element, stride);
            off = self.s.m.bin(c, Bin::Add, at, off);
            index = None;
        }
        // A structured buffer by its used-resource entry's stride: raw
        // bytes too. The host makes its buffer views raw (a raw read
        // through a structured view gives zeros on NVIDIA), and one kind
        // of view can then serve every shader that reads the buffer.
        if let (Some(element), Some(u)) = (index, used)
            && !typed
            && u.kind != ResourceType::Cbv
            && u.stride >= 4
            && u.stride.is_multiple_of(4)
        {
            let k = self.s.m.i32(u.stride);
            let at = self.s.m.bin(c, Bin::Mul, element, k);
            off = self.s.m.bin(c, Bin::Add, at, off);
            index = None;
        }
        if used.is_some_and(|u| u.kind == ResourceType::Cbv) && !typed {
            // A constant buffer read per lane (a dynamically indexed array).
            // shortcut: the console's CBV descriptor is taken to have a 16-byte
            // stride, as the vector loads of whole rows suggest.
            let h = self.handle(site, src, false, (13, 0x1_0000))?;
            let byte = match index {
                Some(i) => {
                    let sixteen = self.s.m.i32(16);
                    let rows = self.s.m.bin(c, Bin::Mul, i, sixteen);
                    self.s.m.bin(c, Bin::Add, rows, off)
                }
                None => off,
            };
            let Some(Operand::Vector { reg, .. }) = *data else {
                return Err("a buffer result");
            };
            for k in 0..count {
                let four = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, byte, four);
                let x = self.cbuffer_dword(h, a);
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        if self.root_address(src).is_some() {
            // Title memory by address: the byte in the host copy from here on.
            if let Some(element) = index {
                if used.is_none_or(|u| u.stride != 1) {
                    return Err("an indexed access through a root view by address");
                }
                off = self.s.m.bin(c, Bin::Add, element, off);
                index = None;
            }
            if let Some((_, byte)) = self.root_memory(src, off, guard, store || atomic.is_some()) {
                off = byte;
            }
        }
        let c = self.cur;
        let uav = store || atomic.is_some() || used.is_some_and(|u| u.kind == ResourceType::Uav);
        let mut u = if uav { 0x1000 } else { 0 };
        if uav && !typed && self.entry(src).is_some_and(|e| self.coherent.contains(&e)) {
            u |= 0x4000;
        }
        let iu = self.s.m.undef(self.s.t.i32);
        if let Some((bits, signed)) = narrow {
            return self.buffer_narrow(
                inst, site, src, used, data, index, off, u, iu, guard, store, bits, signed,
            );
        }
        if let Some(op) = atomic {
            return self.buffer_atomic(inst, site, src, used, data, index, off, u, iu, guard, op);
        }
        if store {
            return self.buffer_store(
                inst, site, src, used, data, index, off, offset, offen, u, iu, guard, typed, d16,
                count,
            );
        }
        if let Some(f) = *format {
            if index.is_some() {
                return Err("a typed-format load by index");
            }
            let h = self.handle(site, src, false, (11 | u, 0))?;
            let Some(Operand::Vector { reg, .. }) = *data else {
                return Err("a buffer result");
            };
            let mut xs = self.format_load(h, off, f, count)?;
            if d16 {
                xs = self.d16_channels(f, xs)?;
            }
            for (k, x) in xs.into_iter().enumerate() {
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        let r = if typed {
            if *offen || matches!(off, V::Inst(_)) || *offset != 0 {
                return Err("a typed buffer load at an offset");
            }
            if uav {
                self.s.flags |= flags::TYPED_UAV_LOADS;
            }
            let h = self.handle(site, src, false, (10 | u, 0x409))?;
            let index = index.unwrap_or(self.s.m.i32(0));
            self.s.op(
                c,
                "bufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                68,
                &[h, index, iu],
            )
        } else {
            let mask = self.s.m.i8((1u8 << count) - 1);
            let align = self.s.m.i32(4);
            match index {
                // A raw buffer (stride 1) read by index: the index is the byte.
                Some(index) if used.is_some_and(|u| u.stride == 1) => {
                    let byte = self.s.m.bin(c, Bin::Add, index, off);
                    let h = self.handle(site, src, false, (11 | u, 0))?;
                    self.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, byte, iu, mask, align],
                    )
                }
                Some(index) => {
                    let stride = used.map_or(0, |u| u.stride);
                    if stride < 4 || !stride.is_multiple_of(4) {
                        return Err("a structured buffer of unknown stride");
                    }
                    let h = self.handle(site, src, false, (12 | u, stride))?;
                    self.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, index, off, mask, align],
                    )
                }
                None => {
                    let h = self.handle(site, src, false, (11 | u, 0))?;
                    self.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, off, iu, mask, align],
                    )
                }
            }
        };
        let Some(Operand::Vector { reg, .. }) = *data else {
            return Err("a buffer result");
        };
        if d16 {
            // Two f16 channels per VGPR (an odd last high half 0); the low
            // 16 bits of an integer view's (see the stores above).
            let integer = if typed { self.integer_view(src)? } else { None };
            let k16 = self.s.m.i32(16);
            let i32t = self.s.t.i32;
            for k in 0..count.div_ceil(2) {
                let mut x = None;
                for ch in (2 * k..(2 * k + 2).min(count)).rev() {
                    let f = self.s.m.extract(c, r, ch);
                    let mut h = self
                        .s
                        .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f]);
                    if let Some((int, _)) = integer {
                        let bits = self.as_int(f);
                        let low = self.s.m.i32(0xFFFF);
                        let low = self.s.m.bin(c, Bin::And, bits, low);
                        h = self.s.m.select(c, int, low, h);
                    }
                    x = Some(match x {
                        Some(hi) => {
                            let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                            self.s.m.bin(c, Bin::Or, h, hi)
                        }
                        None => h,
                    });
                }
                self.write(inst, Reg::V(reg + k as u8), x.expect("a channel"), guard);
            }
            return Ok(());
        }
        for k in 0..count {
            let x = self.s.m.extract(c, r, k);
            let x = self.as_int(x);
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }
}
