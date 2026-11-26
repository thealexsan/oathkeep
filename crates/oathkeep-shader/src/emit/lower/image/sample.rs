use super::super::*;

impl<'a> Lower<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn image_sample(
        &mut self,
        inst: &Inst,
        name: &'static str,
        op: u32,
        site: Site,
        h: V,
        data: &Operand,
        regs: &[u8],
        dmask: &u8,
        unorm: &bool,
        a16: &bool,
        kind: u32,
        coords: usize,
        has_cl: bool,
        guard: Option<V>,
        fu: V,
        iu: V,
        gathered: &mut Option<[V; 4]>,
    ) -> Result<Option<V>, Why> {
        let c = self.cur;
        let addr_u32 = |l: &mut Self, reg_idx: usize| -> Result<V, Why> {
            let r = *regs.get(reg_idx).ok_or("an image address")?;
            let v = inst.read(Reg::V(r)).ok_or("an image address")?;
            l.i32_of(v)
        };
        let addr_packed = |l: &mut Self, base_reg: usize, k: usize| -> Result<V, Why> {
            let x = addr_u32(l, base_reg + if *a16 { k / 2 } else { k })?;
            if !*a16 {
                return Ok(x);
            }
            let c = l.cur;
            let (sh, mask) = (l.s.m.i32(16 * (k as u32 % 2)), l.s.m.i32(0xFFFF));
            let x = l.s.m.bin(c, Bin::LShr, x, sh);
            Ok(l.s.m.bin(c, Bin::And, x, mask))
        };
        let addr_val = |l: &mut Self, k: usize| -> Result<V, Why> { addr_packed(l, 0, k) };
        let res = {
            let ss = self
                .descriptors
                .get(&(site, true))
                .copied()
                .ok_or("an untracked sampler")?;
            // Comparison: the reference value is the first address after
            // the bias, and the sampler is a comparison sampler.
            let cmp = matches!(op, 64 | 65);
            if cmp && (*a16 || dmask.count_ones() != 1) {
                return Err("a comparison sample of 16-bit coordinates or several channels");
            }
            let mut sampler_choice: Option<(V, Vec<u32>)> = None;
            let sh = match ss {
                Source::Phi { block, args } => {
                    match self.sampler_phi_choice(*block, args, cmp, 8) {
                        Ok((None, regs)) => self.s.sampler_handle(self.cur, regs[0]),
                        Ok((Some(idx), regs)) => {
                            let first = self.s.sampler_handle(self.cur, regs[0]);
                            sampler_choice = Some((idx, regs));
                            first
                        }
                        Err(_) => {
                            self.handle(site, ss, true, (14 | if cmp { 0x8000 } else { 0 }, 0))?
                        }
                    }
                }
                _ => self.handle(site, ss, true, (14 | if cmp { 0x8000 } else { 0 }, 0))?,
            };
            // Texel offsets (`_o`) come first: x in bits 0-5, y in 8-13,
            // signed (programmable offsets: gathers only).
            let offset_word = name.ends_with("_o");
            let first = usize::from(offset_word)
                + usize::from(name.starts_with("image_sample_b"))
                + usize::from(cmp);
            // Coordinates the size divides (not the array slice); explicit
            // gradients (x then y) come before the coordinates.
            let spatial = coords - usize::from(kind == 6 || kind == 7);
            // A cube's gradients are the two of its face coordinates.
            let grads = if kind == 5 { 2 } else { spatial };
            let deriv_regs = if op == 63 {
                if *a16 {
                    grads.div_ceil(2)
                } else {
                    grads
                }
            } else {
                0
            };
            let coord_base = first + 2 * deriv_regs;
            let mut cs = vec![fu; 4];
            // With a16, sampling coordinates, gradients, LOD and clamp are f16.
            let to_float = |l: &mut Self, x: V| {
                if *a16 {
                    let f = l.s.t.f32;
                    l.s.op(l.cur, "legacyF16ToF32", Attr::ReadNone, f, 131, &[x])
                } else {
                    l.as_float(x)
                }
            };
            if kind == 5 && (*unorm || op == 81) {
                return Err("a cube sample with texel coordinates or a level query");
            }
            for (k, slot) in cs.iter_mut().enumerate().take(coords) {
                let x = addr_packed(self, coord_base, k)?;
                *slot = to_float(self, x);
            }
            if kind == 5 {
                let dir = self.cube_direction(cs[0], cs[1], cs[2]);
                cs[..3].copy_from_slice(&dir);
            }
            if *unorm {
                // Texel coordinates: divided by the size of level 0.
                let i32t = self.s.t.i32;
                let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
                let z = self.s.m.i32(0);
                let d = self
                    .s
                    .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, z]);
                for (k, slot) in cs.iter_mut().enumerate().take(spatial) {
                    let n = self.s.m.extract(c, d, k as u32);
                    let n = self.s.m.cast(c, Cast::UiToFp, n, self.s.t.f32);
                    *slot = self.s.m.bin(c, Bin::SDiv, *slot, n);
                }
            }
            if op == 81 {
                // The clamped then the unclamped level of detail.
                let Operand::Vector { reg, .. } = *data else {
                    return Err("an image result");
                };
                let f32t = self.s.t.f32;
                for (k, ch) in (0..2u32).filter(|ch| dmask >> ch & 1 != 0).enumerate() {
                    let clamped = self.s.m.bool(ch == 0);
                    let lod = self.s.op(
                        c,
                        "calculateLOD.f32",
                        Attr::ReadOnly,
                        f32t,
                        81,
                        &[h, sh, cs[0], cs[1], cs[2], clamped],
                    );
                    let x = self.as_int(lod);
                    self.write(inst, Reg::V(reg + k as u8), x, guard);
                }
                return Ok(None);
            }
            let offsets = if kind == 5 {
                0
            } else {
                (coords - usize::from(kind == 6 || kind == 7)).min(3)
            };
            let zero = self.s.m.i32(0);
            let mut os = vec![iu; 3];
            for o in os.iter_mut().take(offsets) {
                *o = zero;
            }
            if offset_word && offsets > 0 {
                let w = addr_val(self, 0)?;
                for (k, at) in [(0, 26), (1, 18)].into_iter().take(offsets.min(2)) {
                    // Bits at..at+5 sign-extended: shift up, then down.
                    let left = self.s.m.i32(at);
                    let t = self.s.m.bin(c, Bin::Shl, w, left);
                    let k26 = self.s.m.i32(26);
                    os[k] = self.s.m.bin(c, Bin::AShr, t, k26);
                }
            }
            let mut args = vec![h, sh];
            args.extend(&cs);
            let clamp = if has_cl {
                let x = addr_packed(self, coord_base, coords)?;
                to_float(self, x)
            } else {
                fu
            };
            let call_sample = |l: &mut Self,
                                   intrinsic: &str,
                                   opcode: u32,
                                   args: &[V],
                                   gathered: &mut Option<[V; 4]>|
             -> V {
                let Some((idx, ref regs)) = sampler_choice else {
                    return l
                        .s
                        .op(c, intrinsic, Attr::ReadOnly, l.s.t.res_ret, opcode, args);
                };
                let merge = l.s.m.new_block();
                let mut cur_test = c;
                let mut per_ch: [Vec<(V, usize)>; 4] = Default::default();
                let mut last_r = args[0];
                for (i, &reg) in regs.iter().enumerate() {
                    let arm = if i + 1 == regs.len() {
                        cur_test
                    } else {
                        let arm = l.s.m.new_block();
                        let next = l.s.m.new_block();
                        let k = l.s.m.i32(i as u32);
                        let cond = l.s.m.cmp(cur_test, pred::EQ, idx, k);
                        l.s.m.terminate(cur_test, Term::CondBr(cond, arm, next));
                        cur_test = next;
                        arm
                    };
                    let mut arm_args = args.to_vec();
                    arm_args[1] = l.s.sampler_handle(arm, reg);
                    let r_i = l.s.op(
                        arm,
                        intrinsic,
                        Attr::ReadOnly,
                        l.s.t.res_ret,
                        opcode,
                        &arm_args,
                    );
                    for (ch, slot) in per_ch.iter_mut().enumerate() {
                        let v = l.s.m.extract(arm, r_i, ch as u32);
                        slot.push((v, arm));
                    }
                    l.s.m.terminate(arm, Term::Br(merge));
                    last_r = r_i;
                }
                let mut phis = [fu; 4];
                for (ch, incoming) in per_ch.into_iter().enumerate() {
                    let phi = l.s.m.phi(merge, l.s.t.f32);
                    l.s.m.set_phi(phi, incoming);
                    phis[ch] = phi;
                }
                *gathered = Some(phis);
                l.cur = merge;
                last_r
            };
            match op {
                // Outside a pixel shader there are no screen derivatives:
                // the validator refuses `sample` there ("features
                // incompatible with the shader stage": 730 compute programs,
                // among them the G-buffer resolves and the sky), and what a
                // driver does with it is its own affair.
                // shortcut: such a sample reads level 0 (a bias: that
                // level). The console takes derivatives from neighbouring
                // lanes there; compute them from the lanes if a resolve
                // shows the wrong mip.
                60 if !self.s.pixel => {
                    args.extend(&os);
                    args.push(self.s.m.f32(0));
                    call_sample(self, "sampleLevel.f32", 62, &args, gathered)
                }
                60 => {
                    if offset_word {
                        // DXIL wants immediate offsets in -8..=7 here; the
                        // console's are 6-bit and may be computed. A
                        // constant word in range becomes immediates
                        // (upstream PR #634); anything else keeps the
                        // offsets computed above (upstream refuses those).
                        let w = addr_val(self, 0)?;
                        if let Some(w) = self.s.m.int_value(w) {
                            let w = w as u32;
                            let field = |k: usize| {
                                let x = (w >> (8 * k) & 0x3F) as i32;
                                if x >= 32 { x - 64 } else { x }
                            };
                            let n = offsets.min(2);
                            if (0..n).all(|k| (-8..=7).contains(&field(k))) {
                                for (k, o) in os.iter_mut().enumerate().take(n) {
                                    *o = self.s.m.i32(field(k) as u32);
                                }
                            }
                        }
                    }
                    args.extend(&os);
                    args.push(clamp);
                    call_sample(self, "sample.f32", 60, &args, gathered)
                }
                61 => {
                    // With a16 the bias is an f16 in the low half of its
                    // word (the word is whole, the value is not).
                    let bias = addr_u32(self, usize::from(offset_word))?;
                    let bias = if *a16 {
                        let low = self.s.m.i32(0xFFFF);
                        let bias = self.s.m.bin(c, Bin::And, bias, low);
                        to_float(self, bias)
                    } else {
                        self.as_float(bias)
                    };
                    args.extend(&os);
                    if self.s.pixel {
                        args.extend([bias, clamp]);
                        call_sample(self, "sampleBias.f32", 61, &args, gathered)
                    } else {
                        // As above: the bias is the level.
                        args.push(bias);
                        call_sample(self, "sampleLevel.f32", 62, &args, gathered)
                    }
                }
                63 if kind == 5 => {
                    // A cube with explicit gradients: the host wants them
                    // as changes of the direction, the console gives the
                    // changes of the face coordinates (the face is one
                    // unit wide). The level they select is taken from the
                    // longer of the two, in texels, and sampled directly.
                    // shortcut: no anisotropic filtering for these samples
                    // (the longer axis alone decides the level).
                    let mut len = [fu; 2];
                    for (axis, slot) in len.iter_mut().enumerate() {
                        let mut sum = self.s.m.f32(0);
                        for k in 0..2 {
                            let x = addr_packed(self, first + axis * deriv_regs, k)?;
                            let x = to_float(self, x);
                            let sq = self.s.m.bin(c, Bin::Mul, x, x);
                            sum = self.s.m.bin(c, Bin::Add, sum, sq);
                        }
                        *slot = self.unary(24, sum); // square root
                    }
                    let longer = self.binary(35, len[0], len[1]); // maximum
                    let i32t = self.s.t.i32;
                    let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
                    let z = self.s.m.i32(0);
                    let d = self
                        .s
                        .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, z]);
                    let width = self.s.m.extract(c, d, 0);
                    let width = self.s.m.cast(c, Cast::UiToFp, width, self.s.t.f32);
                    let texels = self.s.m.bin(c, Bin::Mul, longer, width);
                    // DXIL's logarithm is the one to base 2 (it was taken for
                    // the natural one: every level came out 1.44 times too
                    // high; DexNom/Oathkeep 921d8d3b). A length of 0 gives
                    // minus infinity, which the sampler clamps to level 0.
                    // SampleLevel takes no clamp: the instruction's least
                    // level is applied here.
                    let lod = self.unary(23, texels);
                    let lod = if has_cl { self.binary(35, lod, clamp) } else { lod };
                    args.extend(&os);
                    args.push(lod);
                    call_sample(self, "sampleLevel.f32", 62, &args, gathered)
                }
                63 => {
                    let mut g = vec![fu; 6];
                    for k in 0..spatial {
                        let x = addr_packed(self, first, k)?;
                        g[k] = to_float(self, x);
                        let y = addr_packed(self, first + deriv_regs, k)?;
                        g[3 + k] = to_float(self, y);
                    }
                    args.extend(&os);
                    args.extend(g);
                    args.push(clamp);
                    call_sample(self, "sampleGrad.f32", 63, &args, gathered)
                }
                64 | 65 => {
                    let reference = addr_u32(self, first - 1)?;
                    let reference = self.as_float(reference);
                    args.extend(&os);
                    args.push(reference);
                    let name = if op == 64 {
                        args.push(clamp);
                        "sampleCmp.f32"
                    } else {
                        "sampleCmpLevelZero.f32"
                    };
                    call_sample(self, name, op, &args, gathered)
                }
                62 => {
                    let lod = if name == "image_sample_l" {
                        let x = addr_packed(self, coord_base, coords)?;
                        to_float(self, x)
                    } else {
                        self.s.m.f32(0)
                    };
                    args.extend(&os);
                    args.push(lod);
                    call_sample(self, "sampleLevel.f32", 62, &args, gathered)
                }
                _ => {
                    if name == "image_gather4" {
                        return Err("a gather with implicit level");
                    }
                    if dmask.count_ones() != 1 {
                        return Err("a gather of several channels");
                    }
                    if matches!(name, "image_gather4_l" | "image_gather4_l_o")
                        && coords == 2
                        && spatial == 2
                    {
                        // The host's gather has no level: it read level 0
                        // where the title names one (the depth pyramid its
                        // occlusion tests read: most of the world was
                        // culled against the wrong depths). The four texels
                        // are read at the level instead, in the gather's
                        // order: (u0,v1) (u1,v1) (u1,v0) (u0,v0).
                        // With an offset too (`image_gather4_l_o`): the
                        // offset is in texels of that level.
                        // The title's meshlet culling takes a 4x4 block of
                        // the pyramid as one gather and three offset ones
                        // ((2,0), (0,2), (2,2)); the three read level 0,
                        // two texels beside the first one's place, so a
                        // meshlet three or four texels wide was tested
                        // against a quarter of what it covers.
                        // shortcut: the level is rounded to a whole one and
                        // coordinates past the edge are clamped, whatever
                        // the sampler says.
                        let lod = addr_packed(self, coord_base, coords)?;
                        let lod = to_float(self, lod);
                        let (i32t, f32t) = (self.s.t.i32, self.s.t.f32);
                        let zero = self.s.m.f32(0);
                        let lod = self.binary(35, lod, zero); // maximum
                        let lod = self.unary(26, lod); // round to nearest
                        let mip = self.s.m.cast(c, Cast::FpToUi, lod, i32t);
                        let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
                        let d = self
                            .s
                            .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, mip]);
                        let half = self.s.m.f32(0.5f32.to_bits());
                        let one = self.s.m.f32(1.0f32.to_bits());
                        let (mut lo, mut hi) = ([iu; 2], [iu; 2]);
                        for k in 0..2 {
                            let n = self.s.m.extract(c, d, k as u32);
                            let n = self.s.m.cast(c, Cast::UiToFp, n, f32t);
                            let last = self.s.m.bin(c, Bin::Sub, n, one);
                            let t = self.s.m.bin(c, Bin::Mul, cs[k], n);
                            let t = self.s.m.bin(c, Bin::Sub, t, half);
                            let first = self.unary(27, t); // round down
                            // The offset word's field (0 without one).
                            let by = self.s.m.cast(c, Cast::SiToFp, os[k], f32t);
                            let first = self.s.m.bin(c, Bin::Add, first, by);
                            let next = self.s.m.bin(c, Bin::Add, first, one);
                            for (slot, x) in [(&mut lo[k], first), (&mut hi[k], next)] {
                                let x = self.binary(35, x, zero); // maximum
                                let x = self.binary(36, x, last); // minimum
                                *slot = self.s.m.cast(c, Cast::FpToSi, x, i32t);
                            }
                        }
                        let ch = dmask.trailing_zeros();
                        let mut g = [fu; 4];
                        let mut read = None;
                        let texels = [(lo[0], hi[1]), (hi[0], hi[1]), (hi[0], lo[1]), (lo[0], lo[1])];
                        for (slot, (x, y)) in g.iter_mut().zip(texels) {
                            let r = self.s.op(
                                c,
                                "textureLoad.f32",
                                Attr::ReadOnly,
                                self.s.t.res_ret,
                                66,
                                &[h, mip, x, y, iu, iu, iu, iu],
                            );
                            *slot = self.s.m.extract(c, r, ch);
                            read = Some(r);
                        }
                        *gathered = Some(g);
                        read.expect("four reads")
                    } else {
                        args.extend(&os[..2]);
                        args.push(self.s.m.i32(dmask.trailing_zeros()));
                        call_sample(self, "textureGather.f32", 73, &args, gathered)
                    }
                }
            }
        };
        Ok(Some(res))
    }
}
