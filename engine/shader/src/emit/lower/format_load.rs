use super::*;

impl<'a> Lower<'a> {
    /// `channels` channels of the element at byte `off` of raw buffer `h`,
    /// converted from the GFX10 buffer format `format` as the hardware's
    /// typed loads do (channels past the format's read 0, 0, 0, 1).
    // shortcut: the descriptor's channel swizzle is taken as the identity
    // (it is for raw views and the descriptors code builds), elements of 4
    // bytes or more as dword-aligned.
    pub(in crate::emit::lower) fn format_load(&mut self, h: V, off: V, format: u8, channels: u32) -> Result<Vec<V>, Why> {
        let (bits, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let total: u32 = bits.iter().sum();
        let (u, align) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let load = |l: &mut Self, at: V, n: u32| -> Vec<V> {
            let mask = l.s.m.i8((1u8 << n) - 1);
            let r = l.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                l.s.t.res_ret,
                139,
                &[h, at, u, mask, align],
            );
            (0..n)
                .map(|k| {
                    let x = l.s.m.extract(c, r, k);
                    l.as_int(x)
                })
                .collect()
        };
        let words = if total < 32 {
            // A byte or half inside the dword that holds it.
            let (not3, three) = (self.s.m.i32(!3), self.s.m.i32(3));
            let at = self.s.m.bin(c, Bin::And, off, not3);
            let w = load(self, at, 1)[0];
            let byte = self.s.m.bin(c, Bin::And, off, three);
            let sh = self.s.m.bin(c, Bin::Shl, byte, three);
            vec![self.s.m.bin(c, Bin::LShr, w, sh)]
        } else {
            load(self, off, total / 32)
        };
        let float = |l: &mut Self, f: V| l.as_int(f);
        let mut out = Vec::new();
        let mut at = 0;
        for &w in bits {
            let word = words[(at / 32) as usize];
            let field = if w == 32 {
                word
            } else {
                let sh = self.s.m.i32(at % 32);
                let t = self.s.m.bin(c, Bin::LShr, word, sh);
                let m = self.s.m.i32((1 << w) - 1);
                self.s.m.bin(c, Bin::And, t, m)
            };
            at += w;
            // Sign-extended from the field's top bit.
            let signed = |l: &mut Self| {
                if w == 32 {
                    return field;
                }
                let k = l.s.m.i32(32 - w);
                let t = l.s.m.bin(c, Bin::Shl, field, k);
                l.s.m.bin(c, Bin::AShr, t, k)
            };
            let f32t = self.s.t.f32;
            // A normalized format's ends are exactly 1.0 and -1.0. The host
            // may divide through a reciprocal and land just below 1.0: RDNA
            // 4 loaded 16_16_UNORM's 65535 as the half 0x3BFF, not 0x3C00.
            let x = match numeric {
                Numeric::Unorm => {
                    let top = (1u64 << w) - 1;
                    let q = if w <= 16 {
                        let q = self.norm_quotient(field, w);
                        self.as_float(q)
                    } else {
                        let f = self.s.m.cast(c, Cast::UiToFp, field, f32t);
                        let max = self.s.m.f32((top as f32).to_bits());
                        self.s.m.bin(c, Bin::SDiv, f, max)
                    };
                    let (high, one) = (self.s.m.i32(top as u32), self.s.m.f32(1.0f32.to_bits()));
                    let at_top = self.s.m.cmp(c, pred::EQ, field, high);
                    let q = self.s.m.select(c, at_top, one, q);
                    float(self, q)
                }
                Numeric::Snorm => {
                    let s = signed(self);
                    let top = (1u32 << (w - 1)) - 1;
                    let q = if w <= 17 {
                        // The magnitude's quotient, signed (codes at and
                        // below -top are -1.0 below).
                        let (zero, sign) = (self.s.m.i32(0), self.s.m.i32(0x8000_0000));
                        let neg = self.s.m.cmp(c, pred::SLT, s, zero);
                        let minus = self.s.m.bin(c, Bin::Sub, zero, s);
                        let m = self.s.m.select(c, neg, minus, s);
                        let q = self.norm_quotient(m, w - 1);
                        let sign = self.s.m.select(c, neg, sign, zero);
                        let q = self.s.m.bin(c, Bin::Or, q, sign);
                        self.as_float(q)
                    } else {
                        let f = self.s.m.cast(c, Cast::SiToFp, s, f32t);
                        let max = self.s.m.f32((top as f32).to_bits());
                        self.s.m.bin(c, Bin::SDiv, f, max)
                    };
                    // The lowest value is -1.0 too, as is the one above it.
                    let (high, one) = (self.s.m.i32(top), self.s.m.f32(1.0f32.to_bits()));
                    let at_top = self.s.m.cmp(c, pred::SGE, s, high);
                    let q = self.s.m.select(c, at_top, one, q);
                    let low = self.s.m.i32(top.wrapping_neg());
                    let minus_one = self.s.m.f32((-1.0f32).to_bits());
                    let at_bottom = self.s.m.cmp(c, pred::SLE, s, low);
                    let q = self.s.m.select(c, at_bottom, minus_one, q);
                    float(self, q)
                }
                Numeric::Uscaled => {
                    let f = self.s.m.cast(c, Cast::UiToFp, field, f32t);
                    float(self, f)
                }
                Numeric::Sscaled => {
                    let s = signed(self);
                    let f = self.s.m.cast(c, Cast::SiToFp, s, f32t);
                    float(self, f)
                }
                Numeric::Uint => field,
                Numeric::Sint => signed(self),
                Numeric::Float if w == 32 => field,
                Numeric::Float => {
                    let h = match half_shift(w) {
                        0 => field,
                        k => {
                            let sh = self.s.m.i32(k);
                            self.s.m.bin(c, Bin::Shl, field, sh)
                        }
                    };
                    let f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
                    float(self, f)
                }
            };
            out.push(x);
        }
        let one = match numeric {
            Numeric::Uint | Numeric::Sint => 1,
            _ => 1.0f32.to_bits(),
        };
        for k in out.len() as u32..channels {
            out.push(self.s.m.i32(if k == 3 { one } else { 0 }));
        }
        out.truncate(channels as usize);
        Ok(out)
    }

    /// The bits of the float nearest to `a / d`, `d = 2^w - 1`, for codes
    /// `0 <= a < d` and `w <= 16`: what the console's buffer unit gives for
    /// a normalized code. The host's division is not exact (the RX 6700 XT
    /// returns 8-bit codes one ulp high and some 16-bit codes one ulp low),
    /// and titles truncate `x * d` back to the code (a material index in a
    /// vertex colour's alpha), which a quotient one ulp low turns into the
    /// code below.
    ///
    /// `Q = floor(a * 2^32 / d) = a * K + floor(a * r / d)` with `K =
    /// floor(2^32 / d)` and `r = 2^32 mod d` (no division when `r` is 1:
    /// widths dividing 32). `a / d` is never exact for `0 < a < d` (`d`
    /// odd, `r` coprime to it), so `Q` rounds to 24 bits up exactly when
    /// its highest dropped bit is set, and the result is that integer (at
    /// most 2^24, so its conversion is exact on any host) scaled by a power
    /// of two; `Q` itself when it fits, the quotient's further bits then
    /// lying below half an ulp. That holds for every code of every width but
    /// 11 and 12 (checked for all codes of widths 2 to 16), which take
    /// [`Self::exact_norm`]. The host's conversion of a larger integer is
    /// not relied on: WARP rounds those above 2^31 otherwise.
    pub(in crate::emit::lower) fn norm_quotient(&mut self, a: V, w: u32) -> V {
        if matches!(w, 11 | 12) {
            return self.exact_norm(a, w);
        }
        let c = self.cur;
        let d = (1u64 << w) - 1;
        let (kk, r) = ((1u64 << 32) / d, (1u64 << 32) % d);
        let k = |l: &mut Self, v: u64| l.s.m.i32(v as u32);
        let (one, kv) = (k(self, 1), k(self, kk));
        let mut q = self.s.m.bin(c, Bin::Mul, a, kv);
        if r != 1 {
            let (rv, dv) = (k(self, r), k(self, d));
            let t = self.s.m.bin(c, Bin::Mul, a, rv);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            q = self.s.m.bin(c, Bin::Add, q, more);
        }
        // The bits to drop (1 to 8) from the exponent of `q >> 8`, which
        // converts exactly (fewer than 25 bits); none when `q` fits.
        let k8 = k(self, 8);
        let top = self.s.m.bin(c, Bin::LShr, q, k8);
        let top = self.s.m.cast(c, Cast::UiToFp, top, self.s.t.f32);
        let top = self.as_int(top);
        let k23 = k(self, 23);
        let e = self.s.m.bin(c, Bin::LShr, top, k23);
        let bias = k(self, 127 + 15);
        let shift = self.s.m.bin(c, Bin::Sub, e, bias);
        let (zero, b24) = (k(self, 0), k(self, 1 << 24));
        let fits = self.s.m.cmp(c, pred::IULT, q, b24);
        let shift = self.s.m.select(c, fits, zero, shift);
        // Rounded up when the highest dropped bit is set: `a / d` is never
        // exact, so a dropped half is more than half, never a tie.
        let up = self.s.m.bin(c, Bin::Shl, q, one);
        let up = self.s.m.bin(c, Bin::LShr, up, shift);
        let up = self.s.m.bin(c, Bin::And, up, one);
        let m = self.s.m.bin(c, Bin::LShr, q, shift);
        let m = self.s.m.bin(c, Bin::Add, m, up);
        // At most 2^24: exact as a float, and so its scale by 2^(shift - 32).
        let f = self.s.m.cast(c, Cast::UiToFp, m, self.s.t.f32);
        let b95 = k(self, 127 - 32);
        let scale = self.s.m.bin(c, Bin::Add, shift, b95);
        let scale = self.s.m.bin(c, Bin::Shl, scale, k23);
        let scale = self.as_float(scale);
        let f = self.s.m.bin(c, Bin::Mul, f, scale);
        self.as_int(f)
    }

    /// [`Self::norm_quotient`] in integers only, for any width up to 16: `Q`
    /// as there, 8 more bits when that leaves fewer than 25 (codes of wide
    /// formats near 0), then rounded to 24 bits to nearest even with the
    /// remainder as sticky bit, and assembled as float bits.
    pub(in crate::emit::lower) fn exact_norm(&mut self, a: V, w: u32) -> V {
        let c = self.cur;
        let d = (1u64 << w) - 1;
        let (kk, r) = ((1u64 << 32) / d, (1u64 << 32) % d);
        let k = |l: &mut Self, v: u64| l.s.m.i32(v as u32);
        let (one, zero, dv, k8) = (k(self, 1), k(self, 0), k(self, d), k(self, 8));
        let kv = k(self, kk);
        let mut q = self.s.m.bin(c, Bin::Mul, a, kv);
        let mut rem = a;
        if r != 1 {
            let rv = k(self, r);
            let t = self.s.m.bin(c, Bin::Mul, a, rv);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            q = self.s.m.bin(c, Bin::Add, q, more);
            rem = self.s.m.bin(c, Bin::URem, t, dv);
        }
        // The exponent's bias less the 32 (or 40) fraction bits of `q`; the
        // shift below is added.
        let mut bias = k(self, 127 + 23 - 32);
        if kk < 1 << 24 {
            // Fewer than 25 bits for the smallest codes: 8 more.
            let t = self.s.m.bin(c, Bin::Shl, rem, k8);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            let rest = self.s.m.bin(c, Bin::URem, t, dv);
            let wide = self.s.m.bin(c, Bin::Shl, q, k8);
            let wide = self.s.m.bin(c, Bin::Or, wide, more);
            let limit = k(self, 1 << 24);
            let small = self.s.m.cmp(c, pred::IULT, q, limit);
            q = self.s.m.select(c, small, wide, q);
            rem = self.s.m.select(c, small, rest, rem);
            let b40 = k(self, 127 + 23 - 40);
            bias = self.s.m.select(c, small, b40, bias);
        }
        // Bits to drop: the bit length less 24, 1 to 8.
        let mut shift = one;
        for bit in 25..32 {
            let at = k(self, 1 << bit);
            let ge = self.s.m.cmp(c, pred::IUGE, q, at);
            let add = self.s.m.select(c, ge, one, zero);
            shift = self.s.m.bin(c, Bin::Add, shift, add);
        }
        let mant = self.s.m.bin(c, Bin::LShr, q, shift);
        let below = self.s.m.bin(c, Bin::Sub, shift, one);
        let half = self.s.m.bin(c, Bin::LShr, q, below);
        let half = self.s.m.bin(c, Bin::And, half, one);
        let low = self.s.m.bin(c, Bin::Shl, one, below);
        let low = self.s.m.bin(c, Bin::Sub, low, one);
        let low = self.s.m.bin(c, Bin::And, q, low);
        let rest = self.s.m.bin(c, Bin::Or, low, rem);
        let sticky = self.s.m.cmp(c, pred::NE, rest, zero);
        let odd = self.s.m.bin(c, Bin::And, mant, one);
        let tie = self.s.m.select(c, sticky, one, odd);
        let up = self.s.m.bin(c, Bin::And, half, tie);
        let mant = self.s.m.bin(c, Bin::Add, mant, up);
        // Rounding up to 2^24 moves to the next exponent.
        let k24 = k(self, 24);
        let carry = self.s.m.bin(c, Bin::LShr, mant, k24);
        let mant = self.s.m.bin(c, Bin::LShr, mant, carry);
        let e = self.s.m.bin(c, Bin::Add, bias, shift);
        let e = self.s.m.bin(c, Bin::Add, e, carry);
        let k23 = k(self, 23);
        let e = self.s.m.bin(c, Bin::Shl, e, k23);
        let fraction = k(self, 0x7F_FFFF);
        let m = self.s.m.bin(c, Bin::And, mant, fraction);
        let bits = self.s.m.bin(c, Bin::Or, e, m);
        let is_zero = self.s.m.cmp(c, pred::EQ, a, zero);
        self.s.m.select(c, is_zero, zero, bits)
    }

    /// Channels of buffer format `format`, as [`Self::format_load`] gives
    /// them, as d16 data: 16 bits each, two per dword, low half first (an
    /// odd last high half 0). Integer formats keep their low 16 bits; the
    /// others become f16.
    pub(in crate::emit::lower) fn d16_channels(&mut self, format: u8, channels: Vec<V>) -> Result<Vec<V>, Why> {
        let (_, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let mut halves = Vec::new();
        for x in channels {
            halves.push(if matches!(numeric, Numeric::Uint | Numeric::Sint) {
                let m = self.s.m.i32(0xFFFF);
                self.s.m.bin(c, Bin::And, x, m)
            } else {
                let (f, i32t) = (self.as_float(x), self.s.t.i32);
                self.s
                    .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
            });
        }
        let mut out = Vec::new();
        for pair in halves.chunks(2) {
            out.push(match *pair {
                [lo, hi] => {
                    let k16 = self.s.m.i32(16);
                    let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                    self.s.m.bin(c, Bin::Or, lo, hi)
                }
                [lo] => lo,
                _ => unreachable!("chunks of one or two"),
            });
        }
        Ok(out)
    }
}
