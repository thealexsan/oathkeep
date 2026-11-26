use super::*;

impl<'a> Lower<'a> {
    /// A dword of a lane mask as a number: the ballot of the lanes' bits,
    /// taken in the block that writes the mask (see [`MaskData`]).
    pub(in crate::emit::lower) fn mask_dword(&mut self, v: Value) -> Result<V, Why> {
        let (lo, high) = match self.x.st.highs.get(&v) {
            Some(&lo) => (lo, true),
            None => (v, false),
        };
        let &m = self.x.st.masks.get(&lo).ok_or(LANE_MASK)?;
        // A mask that holds lanes outside EXEC where it is written (the complement of one,
        // say) is the ballot of the lanes that are there: all of them, where two exchanging
        // host waves know every lane runs that block.
        let made = self.def_block.get(&lo).copied();
        let whole = self.exchange.is_some() && !self.local && made.is_none_or(|b| self.all_lanes(b));
        if !self.x.st.active.contains(&lo) && !whole {
            if self.exchange.is_some() && !self.local {
                need(made.unwrap_or(0));
            }
            return Err(MASK_ELSEWHERE);
        }
        let (x, y) = match self.ballots.get(&lo) {
            Some(&xy) => xy,
            None => {
                let at = match self.def_block.get(&lo) {
                    // Program inputs (EXEC on entry).
                    None => self.s.entry,
                    Some(&b) if b == self.cur_ir => self.cur,
                    Some(&b) => self.tail[b].ok_or("a lane mask written after its use")?,
                };
                if !self.balloting.insert(lo) {
                    return Err("a lane mask whose lanes depend on its own value");
                }
                let bit = self.boolean(m, at);
                self.balloting.remove(&lo);
                let bit = bit?;
                let i32t = self.s.t.i32;
                let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
                let r = self
                    .s
                    .op(at, "waveActiveBallot", Attr::NoUnwind, four, 116, &[bit]);
                self.s.flags |= flags::WAVE_OPS;
                let mut xy = (self.s.m.extract(at, r, 0), self.s.m.extract(at, r, 1));
                if self.exchange.is_some() {
                    // a host wave's ballot is its half's 32 bits: the halves exchange them
                    let b = self.def_block.get(&lo).copied();
                    if self.local {
                        if !self.local_ok && !self.local_any {
                            return Err("a lane mask read as data in a half-wave");
                        }
                    } else if !self.exchange.as_ref().is_some_and(|e| e.1) && !b.is_none_or(|b| self.all_lanes(b)) {
                        // (a group of 32 or fewer has no other half to exchange with)
                        need(b.unwrap_or(0));
                        return Err(DIVERGENT_MASK_EXCHANGE);
                    }
                    xy = self.exchange_halves(at, xy.0);
                }
                self.ballots.insert(lo, xy);
                xy
            }
        };
        Ok(if high { y } else { x })
    }

    // ---- operands ----

    pub(in crate::emit::lower) fn operand(&mut self, inst: &Inst, op: &Operand) -> Result<V, Why> {
        match *op {
            Operand::Scalar { code: NULL, .. } => Ok(self.s.m.i32(0)),
            Operand::Scalar { code, .. } => {
                let v = inst.read(Reg::S(code)).ok_or("an unread operand")?;
                self.i32_of(v)
            }
            Operand::Vector { reg, .. } => {
                let v = inst.read(Reg::V(reg)).ok_or("an unread operand")?;
                self.i32_of(v)
            }
            // src_scc: SCC as 0 or 1 (Oathkeep upstream 0b35b7f, as the dot
            // products and packed multiply-adds below).
            Operand::Const(253) => {
                let bit = self.scc_in(inst)?;
                Ok(self.s.m.cast(self.cur, Cast::ZExt, bit, self.s.t.i32))
            }
            // Nothing to wait for (see `ordered_section`).
            Operand::Const(POPS_EXITING) if !self.ordered.is_empty() => Ok(self.s.m.i32(0)),
            Operand::Const(c) => crate::resource::inline(c)
                .map(|x| self.s.m.i32(x))
                .ok_or("a special operand (vccz, execz, scc, lds_direct)"),
            Operand::Literal(x) => Ok(self.s.m.i32(x)),
            Operand::Attr { .. } => Err("an attribute operand"),
        }
    }

    /// An operand read as f16 data: GFX10 puts an inline f16 constant in
    /// the low half of the operand (upstream PR #637, checked there against
    /// the instruction encodings; the half-and-half reading taken from PR
    /// #458 gave the high half the constant too).
    pub(in crate::emit::lower) fn f16_operand(&mut self, inst: &Inst, op: &Operand) -> Result<V, Why> {
        match *op {
            Operand::Const(c @ 240..=248) => Ok(self.s.m.i32(inline_f16(c))),
            _ => self.operand(inst, op),
        }
    }

    pub(in crate::emit::lower) fn as_float(&mut self, x: V) -> V {
        self.s.m.cast(self.cur, Cast::Bitcast, x, self.s.t.f32)
    }

    pub(in crate::emit::lower) fn as_int(&mut self, f: V) -> V {
        self.s.m.cast(self.cur, Cast::Bitcast, f, self.s.t.i32)
    }

    /// The SDWA operand selection of source `k`: a byte or word of the
    /// dword, zero- or sign-extended (none: the whole dword).
    pub(in crate::emit::lower) fn sdwa_src(inst: &Inst, k: usize) -> Option<(u8, bool)> {
        let Fields::Valu(v) = &inst.fields else {
            return None;
        };
        let s = v.sdwa?;
        let sel = *s.src_sel.get(k)?;
        let sext = s.sext >> k & 1 != 0;
        (sel < 6).then_some((sel, sext))
    }

    /// Bits `sel` (0-3 a byte, 4-5 a word) of `x`, extended.
    pub(in crate::emit::lower) fn select_bits(&mut self, x: V, sel: u8, sext: bool) -> V {
        let c = self.cur;
        let (width, at) = if sel < 4 {
            (8, 8 * sel)
        } else {
            (16, 16 * (sel - 4))
        };
        let left = self.s.m.i32(u32::from(32 - width - at));
        let t = self.s.m.bin(c, Bin::Shl, x, left);
        let right = self.s.m.i32(32 - u32::from(width));
        let op = if sext { Bin::AShr } else { Bin::LShr };
        self.s.m.bin(c, op, t, right)
    }

    /// A float source with its abs/neg modifiers.
    pub(in crate::emit::lower) fn fsrc(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let selected = Self::sdwa_src(inst, k);
        let mut f = match op {
            _ if selected.is_some() => {
                let (sel, sext) = selected.expect("checked");
                let x = self.operand(inst, &op)?;
                let x = self.select_bits(x, sel, sext);
                self.as_float(x)
            }
            _ if k == 0 && self.src0.is_some() => {
                let x = self.src0.expect("checked");
                self.as_float(x)
            }
            Operand::Vector { reg, .. } | Operand::Scalar { code: reg, .. }
                if !matches!(op, Operand::Scalar { code: NULL, .. }) =>
            {
                let r = if matches!(op, Operand::Vector { .. }) {
                    Reg::V(reg)
                } else {
                    Reg::S(reg)
                };
                let val = inst.read(r).ok_or("an unread operand")?;
                match self.floats.get(&val) {
                    Some(&f) => f,
                    None => {
                        let x = self.i32_of(val)?;
                        self.as_float(x)
                    }
                }
            }
            _ => {
                let x = self.operand(inst, &op)?;
                self.as_float(x)
            }
        };
        if v.abs >> k & 1 != 0 {
            f = self.unary(6, f);
        }
        if v.neg >> k & 1 != 0 {
            let z = self.s.m.f32(0x8000_0000);
            f = self.s.m.bin(self.cur, Bin::Sub, z, f);
        }
        Ok(f)
    }

    pub(in crate::emit::lower) fn src(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        if v.abs >> k & 1 != 0 || v.neg >> k & 1 != 0 {
            return Err("integer operand modifiers");
        }
        match self.src0 {
            Some(x) if k == 0 => Ok(x),
            _ => {
                let x = self.operand(inst, &op)?;
                Ok(match Self::sdwa_src(inst, k) {
                    Some((sel, sext)) => self.select_bits(x, sel, sext),
                    None => x,
                })
            }
        }
    }
}
