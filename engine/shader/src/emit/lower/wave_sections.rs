use super::*;

impl<'a> Lower<'a> {
    /// Finds the whole-wave sections and the values they leave.
    pub(in crate::emit::lower) fn find_sections(&mut self) {
        let p = self.x.p;
        let mut inside: HashMap<Value, (usize, usize, usize)> = HashMap::new();
        for b in 0..p.blocks.len() {
            let found = crate::emit::wave::sections(p, b);
            for (&start, sec) in &found {
                for i in start + 1..sec.end {
                    for &(_, w) in &p.blocks[b].insts[i].writes {
                        inside.insert(w, (b, start, sec.end));
                    }
                }
            }
            if !found.is_empty() {
                self.sections.insert(b, found);
            }
        }
        if inside.is_empty() {
            return;
        }
        for (b, block) in p.blocks.iter().enumerate() {
            for phi in &block.phis {
                self.section_out
                    .extend(phi.args.iter().filter(|a| inside.contains_key(a)));
            }
            for (i, inst) in block.insts.iter().enumerate() {
                for &(_, r) in &inst.reads {
                    if let Some(&(sb, start, end)) = inside.get(&r)
                        && !(sb == b && i > start && i < end)
                    {
                        self.section_out.insert(r);
                    }
                }
            }
        }
    }

    /// An instruction inside a whole-wave section: the values that leave
    /// the section are made from what the simulation found them to be.
    pub(in crate::emit::lower) fn section_inst(&mut self, b: usize, i: usize, start: usize) {
        const UNKNOWN: Why = "a whole-wave section step the simulation does not follow";
        let inst = &self.x.p.blocks[b].insts[i];
        for &(_, w) in &inst.writes {
            let val = if self.section_out.contains(&w) {
                let shape = self.sections[&b][&start].shapes.get(&w).copied();
                let own = self.own_bits.totals.contains(&w);
                match shape.ok_or(UNKNOWN).and_then(|sh| self.shape(sh, own)) {
                    Ok(x) => Val::I32(x),
                    Err(why) => Val::Missing(why),
                }
            } else {
                Val::Missing(UNKNOWN)
            };
            self.vals.insert(w, val);
        }
    }

    /// A value a whole-wave section leaves, as DXIL wave operations over the
    /// host's active lanes: the console's result with console lanes; on
    /// another wave size, what the HLSL wave operation the console compiler
    /// expanded gives on that wave.
    ///
    /// `own`: a wave OR that only steers a loop over each lane's own bits
    /// ([`super::own_bits`]); where two host waves cannot exchange their halves' ORs it is
    /// this host wave's.
    pub(in crate::emit::lower) fn shape(&mut self, shape: crate::emit::wave::Shape, own: bool) -> Result<V, Why> {
        use crate::emit::wave::{Op, Shape};
        let (op, src, mask, ident, prefix) = match shape {
            Shape::K(k) => return Ok(self.s.m.i32(k)),
            Shape::Value(v) => return self.i32_of(v),
            Shape::Active(op, src, mask, ident) => (op, src, mask, ident, None),
            Shape::ActiveF64(lo, hi, mask, high) => {
                return self.active_f64_sum(lo, hi, mask, high);
            }
            Shape::Masked(src, mask, ident) => {
                let c = self.cur;
                let &m = self
                    .x
                    .st
                    .masks
                    .get(&mask)
                    .ok_or("a wave reduction's untracked mask")?;
                let on = self.boolean(m, c)?;
                let x = self.i32_of(src)?;
                let id = self.s.m.i32(ident);
                return Ok(self.s.m.select(c, on, x, id));
            }
            Shape::Prefix(op, src, mask, ident, inclusive) => {
                (op, src, mask, ident, Some(inclusive))
            }
        };
        // Mesh: no host wave size can be declared, so a reduction is over this host wave
        // only. A minimum or maximum is taken that way: the compiler's "next distinct
        // value" loop (the wave's minimum, then the lanes that hold it) serves every
        // lane however the lanes are divided into waves. A sum, a bit reduction or a
        // prefix would be of part of a console wave: still refused.
        if self.mask_data == MaskData::None
            && (prefix.is_some()
                || !matches!(
                    op,
                    Op::MinU | Op::MaxU | Op::MinI | Op::MaxI | Op::MinF | Op::MaxF
                ))
        {
            return Err("a wave reduction in a mesh shader");
        }
        let c = self.cur;
        let &m = self
            .x
            .st
            .masks
            .get(&mask)
            .ok_or("a wave reduction's untracked mask")?;
        let on = self.boolean(m, c)?;
        let x = self.i32_of(src)?;
        let id = self.s.m.i32(ident);
        let x = self.s.m.select(c, on, x, id);
        self.s.flags |= flags::WAVE_OPS;
        if op == Op::AddH {
            return match prefix {
                Some(_) => Err("a wave prefix of an operation other than a sum"),
                None => self.half_sum(x),
            };
        }
        let float = matches!(op, Op::AddF | Op::MinF | Op::MaxF);
        let (i32t, f32t) = (self.s.t.i32, self.s.t.f32);
        let arg = if float { self.as_float(x) } else { x };
        let ty = if float { f32t } else { i32t };
        let sfx = if float { "f32" } else { "i32" };
        let r = match (op, prefix) {
            (Op::AddU | Op::AddF, Some(inclusive)) => {
                let (k0, k1) = (self.s.m.i8(0), self.s.m.i8(1));
                let r = self.s.op(
                    c,
                    &format!("wavePrefixOp.{sfx}"),
                    Attr::NoUnwind,
                    ty,
                    121,
                    &[arg, k0, k1],
                );
                if inclusive {
                    self.s.m.bin(c, Bin::Add, r, arg)
                } else {
                    r
                }
            }
            (_, Some(_)) => return Err("a wave prefix of an operation other than a sum"),
            (Op::Or | Op::And | Op::Xor, None) => {
                let k = self.s.m.i8(match op {
                    Op::And => 0,
                    Op::Or => 1,
                    _ => 2,
                });
                self.s
                    .op(c, "waveActiveBit.i32", Attr::NoUnwind, i32t, 120, &[arg, k])
            }
            (_, None) => {
                let (code, unsigned) = match op {
                    Op::AddU => (0, 1),
                    Op::AddF => (0, 0),
                    Op::MinU => (2, 1),
                    Op::MaxU => (3, 1),
                    Op::MinI | Op::MinF => (2, 0),
                    _ => (3, 0),
                };
                let (k, u) = (self.s.m.i8(code), self.s.m.i8(unsigned));
                self.s.op(
                    c,
                    &format!("waveActiveOp.{sfx}"),
                    Attr::NoUnwind,
                    ty,
                    119,
                    &[arg, k, u],
                )
            }
        };
        // two host waves of 32 hold the console's 64 lanes: their results combine
        let r = if self.local || self.exchange.as_ref().is_some_and(|e| e.1) {
            // this host wave's own result (and prefix): each wave on its own, or a group of
            // 32 threads or fewer, which one host wave holds whole (combining with the 0 a
            // missing half "gives" broke AND, MIN and signed MAX)
            r
        } else if own && prefix.is_none() && op == Op::Or && !self.all_lanes(self.cur_ir) {
            // each thread walks the bits of this value and works only at its own, which the
            // host wave's OR holds: the same run as with the console wave's OR
            r
        } else if self.exchange.is_some() {
            if !self.all_lanes(self.cur_ir) {
                need(self.cur_ir);
                return Err("a wave reduction where not every lane runs");
            }
            let bits = |l: &mut Self, v: V| if float { l.as_int(v) } else { v };
            let back = |l: &mut Self, v: V| if float { l.as_float(v) } else { v };
            match prefix {
                Some(_) => {
                    // the upper half adds the lower half's total to its prefix
                    let (k0, ku) = (self.s.m.i8(0), self.s.m.i8(u8::from(!float)));
                    let total = self.s.op(c, &format!("waveActiveOp.{sfx}"), Attr::NoUnwind, ty, 119, &[arg, k0, ku]);
                    let tb = bits(self, total);
                    let c = self.cur;
                    let (lo, _) = self.exchange_halves(c, tb);
                    let lo = back(self, lo);
                    let cl = self.console_lane();
                    let k32 = self.s.m.i32(32);
                    let upper = self.s.m.cmp(c, pred::IUGE, cl, k32);
                    let zero = if float { self.s.m.f32(0) } else { self.s.m.i32(0) };
                    let add = self.s.m.select(c, upper, lo, zero);
                    self.s.m.bin(c, Bin::Add, r, add)
                }
                None => {
                    let rb = bits(self, r);
                    let c = self.cur;
                    let (lo, hi) = self.exchange_halves(c, rb);
                    let (lo, hi) = (back(self, lo), back(self, hi));
                    match op {
                        Op::AddU | Op::AddF => self.s.m.bin(c, Bin::Add, lo, hi),
                        Op::Or => self.s.m.bin(c, Bin::Or, lo, hi),
                        Op::And => self.s.m.bin(c, Bin::And, lo, hi),
                        Op::Xor => self.s.m.bin(c, Bin::Xor, lo, hi),
                        _ => {
                            // FMax 35, FMin 36, IMax 37, IMin 38, UMax 39, UMin 40
                            let code = match op {
                                Op::MaxF => 35,
                                Op::MinF => 36,
                                Op::MinU => 40,
                                Op::MaxU => 39,
                                Op::MinI => 38,
                                _ => 37,
                            };
                            self.s.op(c, &format!("binary.{sfx}"), Attr::ReadNone, ty, code, &[lo, hi])
                        }
                    }
                }
            }
        } else {
            r
        };
        Ok(if float { self.as_int(r) } else { r })
    }

    /// `WaveActiveSum` of 64-bit floats (`f64(lo, hi)` in active lanes, `+0.0` elsewhere),
    /// returning the low (`!high`) or high (`high`) 32-bit word.
    pub(in crate::emit::lower) fn active_f64_sum(
        &mut self,
        lo: Value,
        hi: Value,
        mask: Value,
        high: bool,
    ) -> Result<V, Why> {
        if self.mask_data == MaskData::None {
            return Err("a wave reduction in a mesh shader");
        }
        let c = self.cur;
        let &m = self
            .x
            .st
            .masks
            .get(&mask)
            .ok_or("a wave reduction's untracked mask")?;
        let on = self.boolean(m, c)?;
        let lo = self.i32_of(lo)?;
        let hi = self.i32_of(hi)?;
        let z = self.s.m.i32(0);
        let lo = self.s.m.select(c, on, lo, z);
        let hi = self.s.m.select(c, on, hi, z);
        self.s.flags |= flags::WAVE_OPS | flags::DOUBLES;
        let f64t = self.s.m.double();
        let i32t = self.s.t.i32;
        let split_ty = self
            .s
            .m
            .named_struct("dx.types.splitdouble", vec![i32t, i32t]);
        let arg = self
            .s
            .op(c, "makeDouble.f64", Attr::ReadNone, f64t, 101, &[lo, hi]);
        let (k0, k0b) = (self.s.m.i8(0), self.s.m.i8(0));
        let sum = self
            .s
            .op(c, "waveActiveOp.f64", Attr::NoUnwind, f64t, 119, &[arg, k0, k0b]);
        let split = self
            .s
            .op(c, "splitDouble.f64", Attr::ReadNone, split_ty, 102, &[sum]);
        let mut r_lo = self.s.m.extract(c, split, 0);
        let mut r_hi = self.s.m.extract(c, split, 1);
        if self.exchange.is_some() {
            if !self.all_lanes(self.cur_ir) {
                need(self.cur_ir);
                return Err("a wave reduction where not every lane runs");
            }
            let (lo0, lo1) = self.exchange_halves(self.cur, r_lo);
            let (hi0, hi1) = self.exchange_halves(self.cur, r_hi);
            let c = self.cur;
            let d0 = self
                .s
                .op(c, "makeDouble.f64", Attr::ReadNone, f64t, 101, &[lo0, hi0]);
            let d1 = self
                .s
                .op(c, "makeDouble.f64", Attr::ReadNone, f64t, 101, &[lo1, hi1]);
            let total = self.s.m.bin(c, Bin::Add, d0, d1);
            let split = self
                .s
                .op(c, "splitDouble.f64", Attr::ReadNone, split_ty, 102, &[total]);
            r_lo = self.s.m.extract(c, split, 0);
            r_hi = self.s.m.extract(c, split, 1);
        }
        Ok(if high { r_hi } else { r_lo })
    }

    /// The sum of two 16-bit floats (the low halves of `a` and `b`) as the console's
    /// `v_add_f16` gives it: added in f32 and rounded to 16 bits, to nearest
    /// ([`Self::f16_nearest`]).
    pub(in crate::emit::lower) fn add_halves(&mut self, a: V, b: V) -> V {
        let c = self.cur;
        let f32t = self.s.t.f32;
        let low = self.s.m.i32(0xFFFF);
        let wide = |l: &mut Self, x: V| {
            let h = l.s.m.bin(c, Bin::And, x, low);
            l.s.op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h])
        };
        let (fa, fb) = (wide(self, a), wide(self, b));
        let f = self.s.m.bin(c, Bin::Add, fa, fb);
        self.f16_nearest(f)
    }

    /// `WaveActiveSum` of 16-bit floats (`x`: each lane's term in its low half, +0 in the
    /// lanes outside the mask), bit for bit as the console adds it: every addition rounds to
    /// 16 bits, so the order counts, and the compiler's is the balanced tree over the lanes
    /// in their order (lanes 2k and 2k+1, then pairs, ... then the wave's two halves;
    /// [`crate::emit::wave::buddies`]). Each lane adds its partner's partial sum, read from the
    /// lane whose number differs in one bit, step by step; two host waves of 32 then exchange
    /// their halves' sums for the last addition. The high half of the result is lane 0's,
    /// which no `v_add_f16` writes.
    /// On a host wave of 64 lanes where not every lane runs, the lanes that are elsewhere
    /// count as the console's do (it masks them to +0): a partner's partial sum is read from
    /// the first lane there is of the partner's run of lanes, and is +0 without one. Two host
    /// waves of 32 can only exchange where every lane runs: refused elsewhere, in a thread
    /// group that does not fill its waves, and for each half-wave on its own (half a sum is
    /// no sum).
    pub(in crate::emit::lower) fn half_sum(&mut self, x: V) -> Result<V, Why> {
        const WHERE: Why = "a wave sum of 16-bit floats where not every lane runs";
        let threads = self.s.threads.ok_or("a wave sum of 16-bit floats outside compute")?;
        if threads.iter().product::<u32>() % 64 != 0 {
            return Err("a wave sum of 16-bit floats in a wave that is not full");
        }
        if self.local {
            return Err("a wave sum of 16-bit floats in a half-wave");
        }
        // two host waves of 32 that exchange, or one host wave of 64 lanes
        let halves = match (&self.exchange, self.s.wave_size) {
            (Some((_, false, _)), _) => true,
            (None, Some(64)) if self.x.p.wave64 => false,
            _ => return Err("a wave sum of 16-bit floats on these waves"),
        };
        let every = if halves {
            self.all_lanes(self.cur_ir)
        } else {
            all_lane_blocks(self.x.st).is_some_and(|all| all.get(self.cur_ir).copied().unwrap_or(false))
        };
        if !every && halves {
            need(self.cur_ir);
            return Err(WHERE);
        }
        let c = self.cur;
        let i32t = self.s.t.i32;
        let lane = self.s.op(c, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let (high, low, zero) = (self.s.m.i32(0xFFFF_0000), self.s.m.i32(0xFFFF), self.s.m.i32(0));
        // the lanes that are here, when not all are
        let here = (!every).then(|| {
            self.s.flags |= flags::WAVE_OPS;
            let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
            let yes = self.s.m.bool(true);
            let r = self.s.op(c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[yes]);
            (self.s.m.extract(c, r, 0), self.s.m.extract(c, r, 1))
        });
        // `v` of the first lane here of the `n` (a power of two) lanes from `base`: +0 of none
        let first_of = |l: &mut Self, v: V, base: V, n: u32| {
            let Some((lo, hi)) = here else {
                return l.read_lane(v, base);
            };
            let (k31, k32) = (l.s.m.i32(31), l.s.m.i32(32));
            let lower = l.s.m.cmp(c, pred::IULT, base, k32);
            let word = l.s.m.select(c, lower, lo, hi);
            let shift = l.s.m.bin(c, Bin::And, base, k31);
            let word = l.s.m.bin(c, Bin::LShr, word, shift);
            let run = l.s.m.i32(u32::MAX >> (32 - n));
            let word = l.s.m.bin(c, Bin::And, word, run);
            let any = l.s.m.cmp(c, pred::NE, word, zero);
            let nth = l.s.op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[word]);
            let from = l.s.m.bin(c, Bin::Add, base, nth);
            // (no lane there: this one, which is here; the read is not used)
            let from = l.s.m.select(c, any, from, lane);
            let got = l.read_lane(v, from);
            l.s.m.select(c, any, got, zero)
        };
        let mut sum = x;
        for step in 0..if halves { 5 } else { 6 } {
            let bit = self.s.m.i32(1 << step);
            let mut partner = self.s.m.bin(c, Bin::Xor, lane, bit);
            if !every {
                let run = self.s.m.i32(!((1 << step) - 1));
                partner = self.s.m.bin(c, Bin::And, partner, run);
            }
            let theirs = first_of(self, sum, partner, 1 << step);
            sum = self.add_halves(sum, theirs);
        }
        let first = first_of(self, x, zero, 1);
        let first = self.s.m.bin(c, Bin::And, first, high);
        let sum = self.s.m.bin(c, Bin::And, sum, low);
        let mine = self.s.m.bin(c, Bin::Or, first, sum);
        if !halves {
            return Ok(mine);
        }
        let (lo, hi) = self.exchange_halves(c, mine);
        let c = self.cur;
        let total = self.add_halves(lo, hi);
        let total = self.s.m.bin(c, Bin::And, total, low);
        let first = self.s.m.bin(c, Bin::And, lo, high);
        Ok(self.s.m.bin(c, Bin::Or, first, total))
    }
}
