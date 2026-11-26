use super::*;

impl<'a> Lower<'a> {
    pub(in crate::emit::lower) fn integer(&mut self, inst: &Inst, name: &'static str) -> Result<V, Why> {
        let c = self.cur;
        let s = |l: &mut Self, k| l.src(inst, k);
        let k = |l: &mut Self, x: u32| l.s.m.i32(x);
        let m31 = |l: &mut Self, x: V| {
            let k31 = l.s.m.i32(31);
            l.s.m.bin(c, Bin::And, x, k31)
        };
        Ok(match name {
            "v_and_b32" | "v_or_b32" | "v_xor_b32" | "v_add_nc_u32" | "v_sub_nc_u32"
            | "v_subrev_nc_u32" | "v_mul_lo_u32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let (op, a, b) = match name {
                    "v_and_b32" => (Bin::And, a, b),
                    "v_or_b32" => (Bin::Or, a, b),
                    "v_xor_b32" => (Bin::Xor, a, b),
                    "v_add_nc_u32" => (Bin::Add, a, b),
                    "v_sub_nc_u32" => (Bin::Sub, a, b),
                    "v_subrev_nc_u32" => (Bin::Sub, b, a),
                    _ => (Bin::Mul, a, b),
                };
                self.s.m.bin(c, op, a, b)
            }
            "v_xnor_b32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let x = self.s.m.bin(c, Bin::Xor, a, b);
                let ones = k(self, u32::MAX);
                self.s.m.bin(c, Bin::Xor, x, ones)
            }
            "v_not_b32" => {
                let a = s(self, 0)?;
                let ones = k(self, u32::MAX);
                self.s.m.bin(c, Bin::Xor, a, ones)
            }
            "v_lshlrev_b32" | "v_lshrrev_b32" | "v_ashrrev_i32" => {
                let (sh, a) = (s(self, 0)?, s(self, 1)?);
                let sh = m31(self, sh);
                let op = match name {
                    "v_lshlrev_b32" => Bin::Shl,
                    "v_lshrrev_b32" => Bin::LShr,
                    _ => Bin::AShr,
                };
                self.s.m.bin(c, op, a, sh)
            }
            "v_lshl_add_u32" | "v_lshl_or_b32" | "v_add_lshl_u32" | "v_and_or_b32"
            | "v_or3_b32" | "v_xad_u32" | "v_add3_u32" | "v_xor3_b32" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                match name {
                    "v_lshl_add_u32" | "v_lshl_or_b32" => {
                        let b = m31(self, b);
                        let t = self.s.m.bin(c, Bin::Shl, a, b);
                        let op = if name == "v_lshl_add_u32" {
                            Bin::Add
                        } else {
                            Bin::Or
                        };
                        self.s.m.bin(c, op, t, d)
                    }
                    "v_add_lshl_u32" => {
                        let t = self.s.m.bin(c, Bin::Add, a, b);
                        let d = m31(self, d);
                        self.s.m.bin(c, Bin::Shl, t, d)
                    }
                    _ => {
                        let (o1, o2) = match name {
                            "v_and_or_b32" => (Bin::And, Bin::Or),
                            "v_or3_b32" => (Bin::Or, Bin::Or),
                            "v_xad_u32" => (Bin::Xor, Bin::Add),
                            "v_add3_u32" => (Bin::Add, Bin::Add),
                            _ => (Bin::Xor, Bin::Xor),
                        };
                        let t = self.s.m.bin(c, o1, a, b);
                        self.s.m.bin(c, o2, t, d)
                    }
                }
            }
            "v_mul_u32_u24" | "v_mul_i32_i24" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let (sh, op) = (
                    k(self, 8),
                    if name == "v_mul_i32_i24" {
                        Bin::AShr
                    } else {
                        Bin::LShr
                    },
                );
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                self.s.m.bin(c, Bin::Mul, a, b)
            }
            "v_mul_hi_u32_u24" | "v_mul_hi_i32_i24" => {
                // The high dword of the same 48-bit product: the low 24 bits
                // of each operand, zero- or sign-extended, multiplied.
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let signed = name == "v_mul_hi_i32_i24";
                let (sh, op) = (k(self, 8), if signed { Bin::AShr } else { Bin::LShr });
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                self.mul_hi(a, b, signed)
            }
            "v_bfe_u32" | "v_bfe_i32" => {
                let (a, off, w) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let w = m31(self, w);
                self.bfe(a, off, w, name == "v_bfe_i32")
            }
            "v_bfi_b32" => {
                let (m, a, b) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let ones = k(self, u32::MAX);
                let nm = self.s.m.bin(c, Bin::Xor, m, ones);
                let x = self.s.m.bin(c, Bin::And, m, a);
                let y = self.s.m.bin(c, Bin::And, nm, b);
                self.s.m.bin(c, Bin::Or, x, y)
            }
            "v_max_i32" | "v_min_i32" | "v_max_u32" | "v_min_u32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let p = match name {
                    "v_max_i32" => pred::SGT,
                    "v_min_i32" => pred::SLT,
                    "v_max_u32" => pred::IUGT,
                    _ => pred::IULT,
                };
                let t = self.s.m.cmp(c, p, a, b);
                self.s.m.select(c, t, a, b)
            }
            "v_mul_hi_u32" | "v_mul_hi_i32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                self.mul_hi(a, b, name == "v_mul_hi_i32")
            }
            "v_alignbit_b32" | "v_alignbyte_b32" => {
                // The low dword of {a, b} >> shift.
                let (a, b, sh) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let sh = if name == "v_alignbyte_b32" {
                    let three = k(self, 3);
                    let t = self.s.m.bin(c, Bin::And, sh, three);
                    let k3 = k(self, 3);
                    self.s.m.bin(c, Bin::Shl, t, k3)
                } else {
                    m31(self, sh)
                };
                let k32 = k(self, 32);
                let lo = self.s.m.bin(c, Bin::LShr, b, sh);
                let left = self.s.m.bin(c, Bin::Sub, k32, sh);
                let left = m31(self, left);
                let hi = self.s.m.bin(c, Bin::Shl, a, left);
                let zero = k(self, 0);
                let none = self.s.m.cmp(c, pred::EQ, sh, zero);
                let hi = self.s.m.select(c, none, zero, hi);
                self.s.m.bin(c, Bin::Or, lo, hi)
            }
            "v_bfrev_b32" => {
                let a = s(self, 0)?;
                self.s
                    .op(c, "unary.i32", Attr::ReadNone, self.s.t.i32, 30, &[a])
            }
            "v_bcnt_u32_b32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let n = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 31, &[a]);
                self.s.m.bin(c, Bin::Add, n, b)
            }
            "v_mad_u32_u24" | "v_mad_i32_i24" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let (sh, op) = (
                    k(self, 8),
                    if name == "v_mad_i32_i24" {
                        Bin::AShr
                    } else {
                        Bin::LShr
                    },
                );
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                let m = self.s.m.bin(c, Bin::Mul, a, b);
                self.s.m.bin(c, Bin::Add, m, d)
            }
            "v_sad_u8" | "v_sad_hi_u8" | "v_sad_u16" | "v_sad_u32" | "v_msad_u8" => {
                // src2 plus the sum of the unsigned differences |a - b| of
                // the operands' bytes, halves or whole dwords, wrapping at 32
                // bits. v_sad_hi_u8 adds the sum shifted to the high half;
                // v_msad_u8 leaves out a byte whose mask byte (src1) is 0.
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let (width, parts) = match name {
                    "v_sad_u16" => (16, 2),
                    "v_sad_u32" => (32, 1),
                    _ => (8, 4),
                };
                let zero = k(self, 0);
                let mut sum = zero;
                for i in 0..parts {
                    let (mut x, mut y) = (a, b);
                    if width < 32 {
                        let (sh, mask) = (k(self, width * i), k(self, (1 << width) - 1));
                        x = self.s.m.bin(c, Bin::LShr, x, sh);
                        x = self.s.m.bin(c, Bin::And, x, mask);
                        y = self.s.m.bin(c, Bin::LShr, y, sh);
                        y = self.s.m.bin(c, Bin::And, y, mask);
                    }
                    let above = self.s.m.cmp(c, pred::IUGT, x, y);
                    let hi = self.s.m.select(c, above, x, y);
                    let lo = self.s.m.select(c, above, y, x);
                    let mut diff = self.s.m.bin(c, Bin::Sub, hi, lo);
                    if name == "v_msad_u8" {
                        let masked = self.s.m.cmp(c, pred::NE, y, zero);
                        diff = self.s.m.select(c, masked, diff, zero);
                    }
                    sum = self.s.m.bin(c, Bin::Add, sum, diff);
                }
                if name == "v_sad_hi_u8" {
                    let k16 = k(self, 16);
                    sum = self.s.m.bin(c, Bin::Shl, sum, k16);
                }
                self.s.m.bin(c, Bin::Add, sum, d)
            }
            "v_min3_i32" | "v_max3_i32" | "v_min3_u32" | "v_max3_u32" | "v_med3_i32"
            | "v_med3_u32" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let signed = name.ends_with("i32");
                let (lt, gt) = if signed {
                    (pred::SLT, pred::SGT)
                } else {
                    (pred::IULT, pred::IUGT)
                };
                let pick = |l: &mut Self, p, x, y| {
                    let t = l.s.m.cmp(c, p, x, y);
                    l.s.m.select(c, t, x, y)
                };
                match &name[2..6] {
                    "min3" => {
                        let t = pick(self, lt, a, b);
                        pick(self, lt, t, d)
                    }
                    "max3" => {
                        let t = pick(self, gt, a, b);
                        pick(self, gt, t, d)
                    }
                    _ => {
                        let lo = pick(self, lt, a, b);
                        let hi = pick(self, gt, a, b);
                        let t = pick(self, lt, hi, d);
                        pick(self, gt, lo, t)
                    }
                }
            }
            "v_perm_b32" => {
                // Byte i of the result is byte S of {src0, src1} (src1 the
                // low dword) for selector byte S below 8; 8-11 repeat the
                // sign of bytes 1 and 3 of src1, then of src0; 12 is 0x00,
                // higher 0xFF.
                let Fields::Valu(v) = &inst.fields else {
                    unreachable!()
                };
                let sel = match v.src.get(2) {
                    Some(&Operand::Literal(x)) => x,
                    Some(&Operand::Const(k)) => crate::resource::inline(k).ok_or(name)?,
                    _ => {
                        // A computed selector: each byte picked at run time.
                        let (a, b, sv) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                        let mut r = k(self, 0);
                        for i in 0..4u32 {
                            let (sh, ff) = (k(self, 8 * i), k(self, 0xFF));
                            let sel = self.s.m.bin(c, Bin::LShr, sv, sh);
                            let sel = self.s.m.bin(c, Bin::And, sel, ff);
                            // Select a dword and a byte within it.
                            // Later selects handle codes 8 and above.
                            let four = k(self, 4);
                            let zero = k(self, 0);
                            let high = self.s.m.bin(c, Bin::And, sel, four);
                            let high = self.s.m.cmp(c, pred::NE, high, zero);
                            let word = self.s.m.select(c, high, a, b);
                            let three = k(self, 3);
                            let index = self.s.m.bin(c, Bin::And, sel, three);
                            let bits = self.s.m.bin(c, Bin::Shl, index, three);
                            let x = self.s.m.bin(c, Bin::LShr, word, bits);
                            let mut byte = self.s.m.bin(c, Bin::And, x, ff);
                            // 8-11: the sign of bytes 1, 3 of src1, then of src0.
                            for (n, src, bit) in [(8, b, 15), (9, b, 31), (10, a, 15), (11, a, 31)]
                            {
                                let left = k(self, 31 - bit);
                                let t = self.s.m.bin(c, Bin::Shl, src, left);
                                let k31 = k(self, 31);
                                let t = self.s.m.bin(c, Bin::AShr, t, k31);
                                let t = self.s.m.bin(c, Bin::And, t, ff);
                                let kn = k(self, n);
                                let is = self.s.m.cmp(c, pred::EQ, sel, kn);
                                byte = self.s.m.select(c, is, t, byte);
                            }
                            let (k12, z) = (k(self, 12), k(self, 0));
                            let is12 = self.s.m.cmp(c, pred::EQ, sel, k12);
                            byte = self.s.m.select(c, is12, z, byte);
                            let above = self.s.m.cmp(c, pred::IUGT, sel, k12);
                            byte = self.s.m.select(c, above, ff, byte);
                            let at = k(self, 8 * i);
                            let byte = self.s.m.bin(c, Bin::Shl, byte, at);
                            r = self.s.m.bin(c, Bin::Or, r, byte);
                        }
                        return Ok(r);
                    }
                };
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let mut r = k(self, 0);
                for i in 0..4 {
                    let pick = sel >> (8 * i) & 0xFF;
                    let byte = match pick {
                        0..=7 => {
                            let (src, n) = if pick < 4 { (b, pick) } else { (a, pick - 4) };
                            let sh = k(self, 8 * n);
                            let t = self.s.m.bin(c, Bin::LShr, src, sh);
                            let ff = k(self, 0xFF);
                            self.s.m.bin(c, Bin::And, t, ff)
                        }
                        8..=11 => {
                            let src = if pick < 10 { b } else { a };
                            let bit = if pick % 2 == 0 { 15 } else { 31 };
                            let left = k(self, 31 - bit);
                            let t = self.s.m.bin(c, Bin::Shl, src, left);
                            let k31 = k(self, 31);
                            let t = self.s.m.bin(c, Bin::AShr, t, k31);
                            let ff = k(self, 0xFF);
                            self.s.m.bin(c, Bin::And, t, ff)
                        }
                        12 => continue,
                        _ => k(self, 0xFF),
                    };
                    let at = k(self, 8 * i);
                    let byte = self.s.m.bin(c, Bin::Shl, byte, at);
                    r = self.s.m.bin(c, Bin::Or, r, byte);
                }
                r
            }
            "v_bfm_b32" => {
                // ((1 << src0[4:0]) - 1) << src1[4:0]
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let one = k(self, 1);
                let (a, b) = (m31(self, a), m31(self, b));
                let m = self.s.m.bin(c, Bin::Shl, one, a);
                let m = self.s.m.bin(c, Bin::Sub, m, one);
                self.s.m.bin(c, Bin::Shl, m, b)
            }
            "v_cvt_pk_u8_f32" => {
                // src0 rounded to nearest even and saturated to a byte, into
                // byte src1[1:0] of src2.
                let f = self.fsrc(inst, 0)?;
                let (b, d) = (s(self, 1)?, s(self, 2)?);
                let f = self.unary(26, f);
                let (zero, max) = (self.s.m.f32(0), self.s.m.f32(255.0f32.to_bits()));
                let f = self.binary(35, f, zero);
                let f = self.binary(36, f, max);
                let x = self.s.m.cast(c, Cast::FpToUi, f, self.s.t.i32);
                let (three, k3) = (k(self, 3), k(self, 3));
                let sh = self.s.m.bin(c, Bin::And, b, three);
                let sh = self.s.m.bin(c, Bin::Shl, sh, k3);
                let x = self.s.m.bin(c, Bin::Shl, x, sh);
                let ff = k(self, 0xFF);
                let hole = self.s.m.bin(c, Bin::Shl, ff, sh);
                let ones = k(self, u32::MAX);
                let hole = self.s.m.bin(c, Bin::Xor, hole, ones);
                let d = self.s.m.bin(c, Bin::And, d, hole);
                self.s.m.bin(c, Bin::Or, d, x)
            }
            "v_ffbl_b32" => {
                // -1 for zero, as DXIL's first-bit-low.
                let a = s(self, 0)?;
                self.s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 32, &[a])
            }
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => {
                // Bits of the mask's low (high) half below this lane, plus src1:
                // with an all-ones mask, the lane index.
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                // Mesh lanes are the console's by construction; pixel and
                // vertex lanes are the host's, as their masks are.
                if self.mask_data == MaskData::Host {
                    // Host lanes: the mask must be one (a ballot) or all lanes.
                    let Fields::Valu(v) = &inst.fields else {
                        unreachable!()
                    };
                    let host = match v.src.first() {
                        Some(&Operand::Const(193)) => true,
                        Some(&Operand::Scalar { code, .. }) => {
                            inst.read(Reg::S(code)).is_some_and(|m| {
                                self.x.st.masks.contains_key(&m) || self.x.st.highs.contains_key(&m)
                            })
                        }
                        _ => false,
                    };
                    if !host {
                        return Err("v_mbcnt of a mask that is no ballot (console lane layout)");
                    }
                    self.s.flags |= flags::WAVE_OPS;
                } else if self.s.mesh.is_none() {
                    self.cross_lane(false)?;
                }
                let lane = self.console_lane();
                let below = if name == "v_mbcnt_lo_u32_b32" {
                    lane
                } else {
                    let (k32, z) = (k(self, 32), k(self, 0));
                    let t = self.s.m.bin(c, Bin::Sub, lane, k32);
                    let low = self.s.m.cmp(c, pred::IULT, lane, k32);
                    self.s.m.select(c, low, z, t)
                };
                // (1 << below) - 1, all ones from 32 on.
                let (one, k32, ones) = (k(self, 1), k(self, 32), k(self, u32::MAX));
                let sh = m31(self, below);
                let m = self.s.m.bin(c, Bin::Shl, one, sh);
                let m = self.s.m.bin(c, Bin::Sub, m, one);
                let full = self.s.m.cmp(c, pred::IUGE, below, k32);
                let m = self.s.m.select(c, full, ones, m);
                let x = self.s.m.bin(c, Bin::And, a, m);
                let n = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 31, &[x]);
                self.s.m.bin(c, Bin::Add, n, b)
            }
            _ => return Err(name),
        })
    }
}
