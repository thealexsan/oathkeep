use super::super::*;

impl<'a> Lower<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn buffer_narrow(
        &mut self,
        inst: &Inst,
        site: Site,
        src: &'a Source,
        used: Option<&'a UsedResource>,
        data: &Option<Operand>,
        index: Option<V>,
        off: V,
        u: u32,
        iu: V,
        guard: Option<V>,
        store: bool,
        bits: u32,
        signed: bool,
    ) -> Result<(), Why> {
        let c = self.cur;
            // Below a dword: raw buffers are read and written in dwords on
            // 4 bytes, so the dword that holds the value is read and
            // shifted, or changed with two atomic operations that touch
            // only the value's own bits (other lanes may write the rest of
            // the dword at the same time). A 16-bit value at a dword's
            // last byte continues in the next dword.
            let byte = match index {
                None => off,
                Some(index) if used.is_some_and(|u| u.stride == 1) => {
                    self.s.m.bin(c, Bin::Add, index, off)
                }
                Some(_) => return Err("a byte or 16-bit buffer access by index"),
            };
            let h = self.handle(site, src, false, (11 | u, 0))?;
            let k3 = self.s.m.i32(3);
            let within = self.s.m.bin(c, Bin::And, byte, k3);
            let aligned = self.s.m.bin(c, Bin::Sub, byte, within);
            let shift = self.s.m.bin(c, Bin::Shl, within, k3);
            let four = self.s.m.i32(4);
            let next = self.s.m.bin(c, Bin::Add, aligned, four);
            let ones = self.s.m.i32(if bits == 8 { 0xff } else { 0xffff });
            let zero = self.s.m.i32(0);
            let k8 = self.s.m.i32(8);
            let straddles = self.s.m.cmp(c, pred::EQ, within, k3);
            if !store {
                let mask = self.s.m.i8(1);
                let align = self.s.m.i32(4);
                let dword = |l: &mut Self, at: V| {
                    let r = l.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        l.s.t.res_ret,
                        139,
                        &[h, at, iu, mask, align],
                    );
                    let f = l.s.m.extract(c, r, 0);
                    l.as_int(f)
                };
                let low = dword(self, aligned);
                let mut x = self.s.m.bin(c, Bin::LShr, low, shift);
                if bits == 16 {
                    let high = dword(self, next);
                    let high = self.s.m.bin(c, Bin::Shl, high, k8);
                    let high = self.s.m.select(c, straddles, high, zero);
                    x = self.s.m.bin(c, Bin::Or, x, high);
                }
                x = self.s.m.bin(c, Bin::And, x, ones);
                if signed {
                    let up = self.s.m.i32(32 - bits);
                    x = self.s.m.bin(c, Bin::Shl, x, up);
                    x = self.s.m.bin(c, Bin::AShr, x, up);
                }
                let Some(Operand::Vector { reg, .. }) = *data else {
                    return Err("a buffer result");
                };
                self.write(inst, Reg::V(reg), x, guard);
                return Ok(());
            }
            let d = data.as_ref().ok_or("buffer store data")?;
            let v = self.vgprs(inst, d, 1)?[0];
            let v = self.s.m.bin(c, Bin::And, v, ones);
            let all = self.s.m.i32(u32::MAX);
            let clear = self.s.m.bin(c, Bin::Shl, ones, shift);
            let keep = self.s.m.bin(c, Bin::Xor, clear, all);
            let set = self.s.m.bin(c, Bin::Shl, v, shift);
            let keep_next = self.s.m.i32(0xffff_ff00);
            let set_next = self.s.m.bin(c, Bin::LShr, v, k8);
            let (and, or) = (self.s.m.i32(1), self.s.m.i32(2));
            let i32t = self.s.t.i32;
            let at = self.begin_guarded(guard);
            for (op, x) in [(and, keep), (or, set)] {
                let args = [h, op, aligned, iu, iu, x];
                self.s
                    .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args);
            }
            if bits == 16 {
                let second = self.begin_guarded(Some(straddles));
                for (op, x) in [(and, keep_next), (or, set_next)] {
                    let args = [h, op, next, iu, iu, x];
                    self.s
                        .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args);
                }
                self.end_guarded(second, &[]);
            }
            self.end_guarded(at, &[]);
            return Ok(());
    }
}
