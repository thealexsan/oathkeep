mod convert;
mod float;
mod lane;
mod shuffle;

use super::*;

impl<'a> Lower<'a> {
    pub(in crate::emit::lower) fn valu(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let name = inst.name();
        if let Some(&source) = self.x.res.restored_lanes.get(&(b, i)) {
            // This lane was overwritten by a uniform scalar. The previous
            // contents of other lanes can be unknown (a reduction scratch).
            let word = match source {
                crate::resource::Moved::Value(v) => self.i32_of(v)?,
                crate::resource::Moved::Const(k) => self.s.m.i32(k),
            };
            return self.write_i(inst, word, None);
        }
        if self.x.res.waterfall.compares.contains(&(b, i)) {
            // The first lane's dwords of a waterfall loop are each lane's
            // own (see `resource::Waterfall`).
            self.lane(inst, Some(true));
            return Ok(());
        }
        // One lane of a VGPR a whole-wave section left.
        if name == "v_readlane_b32"
            && let Some(&(_, w)) = inst.writes.first()
            && let Some(sh) = self.sections.get(&b).and_then(|s| {
                s.iter().filter(|s| s.1.end < i).find_map(|s| s.1.reads.get(&w).copied())
            })
        {
            let x = self.shape(sh, false)?;
            self.vals.insert(w, Val::I32(x));
            return Ok(());
        }
        if let Some(rest) = name.strip_prefix("v_cmp_").or(name.strip_prefix("v_cmpx_")) {
            let r = if v.dpp.is_some() {
                Err("a compare with DPP")
            } else {
                self.compare(inst, rest)
            };
            if let Err(why) = r {
                for &(_, w) in &inst.writes {
                    self.lane_fail.insert(w, why);
                }
            }
            return r;
        }
        let mut guard = self.guard(b, i)?;
        if let Some(d) = v.dpp {
            let (x, enable) = self.dpp(inst, d)?;
            guard = match (guard, enable) {
                (Some(g), Some(e)) => Some(self.s.m.bin(self.cur, Bin::And, g, e)),
                (g, e) => g.or(e),
            };
            self.src0 = Some(x);
        }
        let r = self.valu_op(inst, guard);
        self.src0 = None;
        r
    }

    /// DPP: src0 as read from another lane, and which lanes write their
    /// result (none: all).
    pub(in crate::emit::lower) fn dpp(&mut self, inst: &Inst, dpp: Dpp) -> Result<(V, Option<V>), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let Some(&Operand::Vector { reg, .. }) = v.src.first() else {
            return Err("a DPP operand");
        };
        let x = self.i32_of(inst.read(Reg::V(reg)).ok_or("a DPP operand")?)?;
        // Quad permutations and DPP8 stay in groups the pixel layout shares.
        let quad = matches!(dpp, Dpp::Row { ctrl: 0..=0xFF, .. });
        let mesh_quad = self.s.mesh.is_some() && quad;
        if !mesh_quad {
            self.cross_lane(quad)?;
        }
        let c = self.cur;
        let lane = self.lane_index();
        let k = |l: &mut Self, n: u32| l.s.m.i32(n);
        let (sixteen_mask, low) = (k(self, !15), k(self, 15));
        let row = self.s.m.bin(c, Bin::And, lane, sixteen_mask);
        let pos = self.s.m.bin(c, Bin::And, lane, low);
        // Source lane, and whether it lies in the row.
        let (src, inside, masks, zero) = match dpp {
            Dpp::Lanes { sel, .. } => {
                let (seven, not7, three) = (k(self, 7), k(self, !7), k(self, 3));
                let p = self.s.m.bin(c, Bin::And, lane, seven);
                let sh = self.s.m.bin(c, Bin::Mul, p, three);
                let sel = k(self, sel);
                let pick = self.s.m.bin(c, Bin::LShr, sel, sh);
                let pick = self.s.m.bin(c, Bin::And, pick, seven);
                let base = self.s.m.bin(c, Bin::And, lane, not7);
                (self.s.m.bin(c, Bin::Or, base, pick), None, None, false)
            }
            Dpp::Row {
                ctrl,
                row_mask,
                bank_mask,
                bound_ctrl,
                ..
            } => {
                let n = u32::from(ctrl & 15);
                let (src, inside) = match ctrl {
                    0..=0xFF => {
                        let (three, one, not3) = (k(self, 3), k(self, 1), k(self, !3));
                        let p = self.s.m.bin(c, Bin::And, lane, three);
                        let sh = self.s.m.bin(c, Bin::Shl, p, one);
                        let perm = k(self, u32::from(ctrl));
                        let pick = self.s.m.bin(c, Bin::LShr, perm, sh);
                        let pick = self.s.m.bin(c, Bin::And, pick, three);
                        let base = self.s.m.bin(c, Bin::And, lane, not3);
                        (self.s.m.bin(c, Bin::Or, base, pick), None)
                    }
                    0x101..=0x10F | 0x111..=0x11F => {
                        let nn = k(self, n);
                        let left = ctrl < 0x110;
                        let (op, p) = if left {
                            (Bin::Add, pred::IULT)
                        } else {
                            (Bin::Sub, pred::IUGE)
                        };
                        let s = self.s.m.bin(c, op, lane, nn);
                        // Left: pos + n < 16; right: pos >= n.
                        let bound = if left { k(self, 16 - n) } else { nn };
                        (s, Some(self.s.m.cmp(c, p, pos, bound)))
                    }
                    0x121..=0x12F => {
                        let nn = k(self, n);
                        let t = self.s.m.bin(c, Bin::Sub, pos, nn);
                        let t = self.s.m.bin(c, Bin::And, t, low);
                        (self.s.m.bin(c, Bin::Or, row, t), None)
                    }
                    0x140 | 0x141 => {
                        let m = k(self, if ctrl == 0x140 { 15 } else { 7 });
                        let t = self.s.m.bin(c, Bin::Xor, lane, m);
                        (t, None)
                    }
                    0x150..=0x15F => {
                        let nn = k(self, n);
                        (self.s.m.bin(c, Bin::Or, row, nn), None)
                    }
                    0x160..=0x16F => {
                        let nn = k(self, n);
                        (self.s.m.bin(c, Bin::Xor, lane, nn), None)
                    }
                    _ => return Err("a DPP lane pattern"),
                };
                let masks = (row_mask != 0xF || bank_mask != 0xF).then_some((row_mask, bank_mask));
                (src, inside, masks, bound_ctrl)
            }
        };
        let mut r = if mesh_quad {
            self.quad_pick(x, src)
        } else if let Dpp::Row { ctrl, .. } = dpp
            && self.helpers_read(quad)
        {
            self.quad_read(x, [0, 1, 2, 3].map(|i| (ctrl >> (2 * i) & 3) as u8))
        } else {
            self.read_lane(x, src)
        };
        let mut enable = None;
        if let Some(inside) = inside {
            if zero {
                let z = k(self, 0);
                r = self.s.m.select(c, inside, r, z);
            } else {
                enable = Some(inside);
            }
        }
        if let Some((rows, banks)) = masks {
            // Row (16 lanes) and bank (4 lanes of a row) write masks.
            let (four, two, three) = (k(self, 4), k(self, 2), k(self, 3));
            let ri = self.s.m.bin(c, Bin::LShr, lane, four);
            let ri = self.s.m.bin(c, Bin::And, ri, three);
            let bi = self.s.m.bin(c, Bin::LShr, lane, two);
            let bi = self.s.m.bin(c, Bin::And, bi, three);
            let (rm, bm) = (k(self, u32::from(rows)), k(self, u32::from(banks)));
            let rb = self.s.m.bin(c, Bin::LShr, rm, ri);
            let bb = self.s.m.bin(c, Bin::LShr, bm, bi);
            let both = self.s.m.bin(c, Bin::And, rb, bb);
            let (one, z) = (k(self, 1), k(self, 0));
            let both = self.s.m.bin(c, Bin::And, both, one);
            let on = self.s.m.cmp(c, pred::NE, both, z);
            enable = Some(match enable {
                Some(e) => self.s.m.bin(c, Bin::And, e, on),
                None => on,
            });
        }
        Ok((r, enable))
    }

    pub(in crate::emit::lower) fn valu_op(&mut self, inst: &Inst, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let name = inst.name();
        let c = self.cur;
        let f = |l: &mut Self, k| l.fsrc(inst, k);
        let s = |l: &mut Self, k| l.src(inst, k);
        let float = |l: &mut Self, r: V| l.write_f(inst, r, guard);
        let int = |l: &mut Self, r: V| l.write_i(inst, r, guard);
        if let Some(r) = self.valu16(inst, name, guard) {
            return r;
        }
        if let Some(op) = name.strip_prefix("v_pk_") {
            return self.packed16(inst, op, guard);
        }
        let _ = (c, f, s, float);
        if let Some(r) = self.valu_float(inst, v, name, guard) {
            return r;
        }
        if let Some(r) = self.valu_convert(inst, v, name, guard) {
            return r;
        }
        if let Some(r) = self.valu_lane(inst, v, name, guard) {
            return r;
        }
        if let Some(r) = self.valu_shuffle(inst, v, name, guard) {
            return r;
        }
        let r = self.integer(inst, name)?;
        int(self, r)
    }
}
