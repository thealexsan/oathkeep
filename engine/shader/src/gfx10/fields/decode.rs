use super::dwords::{
    VOP3B, VOP3_TWO_SOURCES, data_dwords, ds_pair, image_addr_dwords, salu_dwords, scalar,
    source, valu_dst, valu_dwords, vector,
};
use super::{Cache, Dpp, EXEC, Fields, NULL, Operand, Sdwa, VCC, Valu};
use super::super::{Encoding, Instruction};

impl Instruction {
    /// Decodes the operand fields. `wave64` sizes lane masks.
    pub fn fields(&self, wave64: bool) -> Fields {
        let name = self.name().unwrap_or("");
        let w = |i: usize| self.words.get(i).copied().unwrap_or(0);
        let (w0, w1) = (w(0), w(1));
        let bit = |word: u32, n: u32| word >> n & 1 != 0;
        let cache = |glc, slc, dlc| Cache { glc, slc, dlc };
        match self.encoding {
            Encoding::Sop2 | Encoding::Sop1 | Encoding::Sopc => {
                let (dst, src0, src1) = salu_dwords(name);
                let sdst = scalar((w0 >> 16 & 0x7F) as u8, dst);
                let ssrc = |shift: u32, dwords| source(w0 >> shift & 0xFF, dwords, w1);
                let (dst, src) = match self.encoding {
                    Encoding::Sop2 => (Some(sdst), vec![ssrc(0, src0), ssrc(8, src1)]),
                    Encoding::Sopc => (None, vec![ssrc(0, src0), ssrc(8, src1)]),
                    _ => match name {
                        "s_getpc_b64" => (Some(sdst), vec![]),
                        "s_setpc_b64" | "s_rfe_b64" => (None, vec![ssrc(0, src0)]),
                        _ => (Some(sdst), vec![ssrc(0, src0)]),
                    },
                };
                Fields::Sop {
                    dst,
                    src,
                    imm: None,
                }
            }
            Encoding::Sopk => {
                let sdst = scalar((w0 >> 16 & 0x7F) as u8, 1 + u8::from(name == "s_call_b64"));
                let (dst, src) = match name {
                    "s_setreg_imm32_b32" => (None, vec![Operand::Literal(w1)]),
                    "s_version" => (None, vec![]),
                    n if n.starts_with("s_cmpk_")
                        || n.starts_with("s_waitcnt_")
                        || n == "s_setreg_b32" =>
                    {
                        (None, vec![sdst])
                    }
                    _ => (Some(sdst), vec![]),
                };
                Fields::Sop {
                    dst,
                    src,
                    imm: Some(w0 as u16),
                }
            }
            Encoding::Sopp => Fields::Sopp { imm: w0 as u16 },
            Encoding::Smem => {
                let sdata = (w0 >> 6 & 0x7F) as u8;
                // s_atc_probe*: SDATA is a 7-bit immediate, not an SGPR. The
                // buffer form's resource is 4 dwords. GLC and DLC are not operands.
                let probe = matches!(name, "s_atc_probe" | "s_atc_probe_buffer");
                let data = if probe {
                    Some(Operand::Literal(u32::from(sdata)))
                } else {
                    match name {
                        "s_gl1_inv" | "s_dcache_inv" | "s_dcache_wb" => None,
                        n if n.starts_with("s_dcache_discard") => None,
                        "s_memtime" | "s_memrealtime" => Some(scalar(sdata, 2)),
                        _ => Some(scalar(sdata, data_dwords(name))),
                    }
                };
                let base =
                    2 + 2 * u8::from(name.starts_with("s_buffer_") || name == "s_atc_probe_buffer");
                Fields::Smem {
                    data,
                    base: scalar(((w0 & 0x3F) << 1) as u8, base),
                    offset: (w1 << 11) as i32 >> 11,
                    soffset: scalar((w1 >> 25) as u8, 1),
                    cache: if probe {
                        Cache::default()
                    } else {
                        cache(bit(w0, 16), false, bit(w0, 14))
                    },
                }
            }
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc | Encoding::Vop3 | Encoding::Vop3p => {
                Fields::Valu(self.valu(name, if wave64 { 2 } else { 1 }))
            }
            Encoding::Vintrp => {
                let vsrc = (w0 & 0xFF) as u8;
                Fields::Vintrp {
                    dst: vector(w0 >> 18, 1),
                    src: if name == "v_interp_mov_f32" {
                        Operand::Const(vsrc)
                    } else {
                        vector(vsrc.into(), 1)
                    },
                    attr: Operand::Attr {
                        index: (w0 >> 10 & 0x3F) as u8,
                        chan: (w0 >> 8 & 3) as u8,
                        high: false,
                    },
                }
            }
            Encoding::Ds => {
                let ty = data_dwords(name);
                let pair = ds_pair(name);
                let returns = name.contains("read")
                    || name.contains("_rtn")
                    || matches!(
                        name,
                        "ds_swizzle_b32"
                            | "ds_permute_b32"
                            | "ds_bpermute_b32"
                            | "ds_append"
                            | "ds_consume"
                            | "ds_ordered_count"
                    );
                let no_addr = name.starts_with("ds_gws")
                    || name.contains("addtid")
                    || matches!(name, "ds_nop" | "ds_append" | "ds_consume");
                let data = if name.contains("read")
                    || name.contains("src2")
                    || matches!(
                        name,
                        "ds_swizzle_b32"
                            | "ds_nop"
                            | "ds_append"
                            | "ds_consume"
                            | "ds_ordered_count"
                            | "ds_gws_sema_v"
                            | "ds_gws_sema_p"
                            | "ds_gws_sema_release_all"
                    ) {
                    0
                } else if pair || name.contains("cmpst") || name.contains("mskor") {
                    2
                } else {
                    1
                };
                Fields::Ds {
                    dst: returns.then(|| vector(w1 >> 24, ty * (1 + u8::from(pair)))),
                    addr: (!no_addr).then(|| vector(w1 & 0xFF, 1)),
                    data: (0..data)
                        .map(|i| vector(w1 >> (8 + 8 * i) & 0xFF, ty))
                        .collect(),
                    offset0: w0 as u8,
                    offset1: (w0 >> 8) as u8,
                    gds: bit(w0, 17),
                }
            }
            Encoding::Mubuf | Encoding::Mtbuf => {
                let (offen, idxen, tfe) = (bit(w0, 12), bit(w0, 13), bit(w1, 23));
                let mtbuf = self.encoding == Encoding::Mtbuf;
                Fields::Buffer {
                    data: (!name.ends_with("_inv"))
                        .then(|| vector(w1 >> 8 & 0xFF, data_dwords(name) + u8::from(tfe))),
                    addr: vector(w1 & 0xFF, 1 + u8::from(offen && idxen)),
                    rsrc: scalar(((w1 >> 16 & 0x1F) << 2) as u8, 4),
                    soffset: source(w1 >> 24, 1, 0),
                    offset: (w0 & 0xFFF) as u16,
                    offen,
                    idxen,
                    format: mtbuf.then_some((w0 >> 19 & 0x7F) as u8),
                    lds: !mtbuf && bit(w0, 16),
                    tfe,
                    cache: cache(bit(w0, 14), bit(w1, 22), bit(w0, 15)),
                }
            }
            Encoding::Mimg => {
                let (dim, dmask, a16, d16) = (
                    (w0 >> 3 & 7) as u8,
                    (w0 >> 8 & 0xF) as u8,
                    bit(w1, 30),
                    bit(w1, 31),
                );
                let (r128, tfe, lwe) = (bit(w0, 15), bit(w0, 16), bit(w0, 17));
                let mut data = if name.starts_with("image_gather4") {
                    4
                } else {
                    dmask.count_ones().max(1) as u8
                };
                if d16 {
                    data = data.div_ceil(2);
                }
                let count = image_addr_dwords(name, dim, a16);
                let addr = if w0 >> 1 & 3 == 0 {
                    vec![vector(w1 & 0xFF, count)]
                } else {
                    // NSA: one VGPR per address, the first in dword 1, the rest
                    // in the extra dwords.
                    (0..usize::from(count))
                        .map(|i| match i {
                            0 => vector(w1 & 0xFF, 1),
                            i => vector(w(2 + (i - 1) / 4) >> (8 * ((i - 1) % 4)) & 0xFF, 1),
                        })
                        .collect()
                };
                let sampled = ["image_sample", "image_gather4", "image_get_lod"]
                    .iter()
                    .any(|p| name.starts_with(p));
                Fields::Image {
                    data: vector(w1 >> 8 & 0xFF, data + u8::from(tfe || lwe)),
                    addr,
                    rsrc: scalar(((w1 >> 16 & 0x1F) << 2) as u8, if r128 { 4 } else { 8 }),
                    sampler: sampled.then(|| scalar(((w1 >> 21 & 0x1F) << 2) as u8, 4)),
                    dmask,
                    dim,
                    unorm: bit(w0, 12),
                    r128,
                    a16,
                    d16,
                    tfe,
                    lwe,
                    cache: cache(bit(w0, 13), bit(w0, 25), bit(w0, 7)),
                }
            }
            Encoding::Flat | Encoding::Global | Encoding::Scratch => {
                let saddr = (w1 >> 16 & 0x7F) as u8;
                let glc = bit(w0, 16);
                let n = data_dwords(name);
                let (load, atomic) = (name.contains("_load_"), name.contains("_atomic_"));
                let addr = match self.encoding {
                    Encoding::Flat => Some(vector(w1 & 0xFF, 2)),
                    Encoding::Global => Some(vector(w1 & 0xFF, 1 + u8::from(saddr == NULL))),
                    // With an SGPR base, scratch takes no VGPR.
                    _ => (saddr == NULL).then(|| vector(w1 & 0xFF, 1)),
                };
                let ret = if atomic && name.contains("cmpswap") {
                    n / 2
                } else {
                    n
                };
                Fields::Flat {
                    dst: (load || atomic && glc).then(|| vector(w1 >> 24, ret)),
                    addr,
                    data: (!load).then(|| vector(w1 >> 8 & 0xFF, n)),
                    saddr: (self.encoding != Encoding::Flat)
                        .then(|| scalar(saddr, 1 + u8::from(self.encoding == Encoding::Global))),
                    offset: match self.encoding {
                        // GFX10 FLAT offsets are 11-bit and unsigned: the
                        // hardware ignores bit 11.
                        Encoding::Flat => (w0 & 0x7FF) as i16,
                        _ => ((w0 << 4) as i16) >> 4,
                    },
                    lds: bit(w0, 13),
                    cache: cache(glc, bit(w0, 17), bit(w0, 12)),
                }
            }
            Encoding::Exp => {
                let compr = bit(w0, 10);
                Fields::Exp {
                    target: (w0 >> 4 & 0x3F) as u8,
                    src: std::array::from_fn(|i| {
                        let slot = if compr { i / 2 } else { i };
                        bit(w0, i as u32).then(|| vector(w1 >> (8 * slot) & 0xFF, 1))
                    }),
                    compr,
                    done: bit(w0, 11),
                    vm: bit(w0, 12),
                }
            }
        }
    }

    fn valu(&self, name: &str, lanes: u8) -> Valu {
        // No operands; the console compiler stores a shader hash in their fields.
        if matches!(name, "v_nop" | "v_pipeflush" | "v_clrexcp") {
            return Valu::default();
        }
        let w = |i: usize| self.words.get(i).copied().unwrap_or(0);
        let w0 = w(0);
        let (dst_n, src_n) = valu_dwords(name);
        let compare = match self.encoding {
            Encoding::Vopc => true,
            Encoding::Vop3 => self.opcode < 0x100,
            _ => false,
        };
        let vop3b = VOP3B.contains(&name);
        let carry_in = name.ends_with("_co_ci_u32") || name == "v_cndmask_b32";
        let mut v = Valu::default();
        match self.encoding {
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc => {
                let ext = w(1);
                let mut src0 = w0 & 0x1FF;
                let vsrc1 = 256 + (w0 >> 9 & 0xFF);
                let mut src1 = vsrc1;
                match src0 {
                    0xF9 => {
                        let sdwa = Sdwa {
                            dst_sel: (ext >> 8 & 7) as u8,
                            dst_unused: (ext >> 11 & 3) as u8,
                            src_sel: [(ext >> 16 & 7) as u8, (ext >> 24 & 7) as u8],
                            sext: (ext >> 19 & 1 | ext >> 26 & 2) as u8,
                        };
                        // The s0/s1 bits select an SGPR or constant instead of a VGPR.
                        src0 = (ext & 0xFF) | if ext >> 23 & 1 == 0 { 256 } else { 0 };
                        if ext >> 31 == 1 {
                            src1 = vsrc1 - 256;
                        }
                        v.neg = (ext >> 20 & 1 | ext >> 27 & 2) as u8;
                        v.abs = (ext >> 21 & 1 | ext >> 28 & 2) as u8;
                        if compare {
                            if ext >> 15 & 1 == 1 {
                                v.dst = Some(scalar((ext >> 8 & 0x7F) as u8, lanes));
                            }
                            v.sdwa = Some(Sdwa {
                                dst_sel: 6,
                                dst_unused: 0,
                                ..sdwa
                            });
                        } else {
                            v.clamp = ext >> 13 & 1 == 1;
                            v.omod = (ext >> 14 & 3) as u8;
                            v.sdwa = Some(sdwa);
                        }
                    }
                    0xFA => {
                        src0 = 256 + (ext & 0xFF);
                        v.neg = (ext >> 20 & 1 | ext >> 21 & 2) as u8;
                        v.abs = (ext >> 21 & 1 | ext >> 22 & 2) as u8;
                        v.dpp = Some(Dpp::Row {
                            ctrl: (ext >> 8 & 0x1FF) as u16,
                            row_mask: (ext >> 28) as u8,
                            bank_mask: (ext >> 24 & 0xF) as u8,
                            bound_ctrl: ext >> 19 & 1 == 1,
                            fi: ext >> 18 & 1 == 1,
                        });
                    }
                    0xE9 | 0xEA => {
                        v.dpp = Some(Dpp::Lanes {
                            sel: ext >> 8,
                            fi: src0 == 0xEA,
                        });
                        src0 = 256 + (ext & 0xFF);
                    }
                    _ => {}
                }
                let op0 = source(src0, src_n[0], ext);
                let op1 = source(src1, src_n[1], ext);
                match self.encoding {
                    Encoding::Vopc => {
                        let mask = if name.starts_with("v_cmpx") {
                            EXEC
                        } else {
                            VCC
                        };
                        v.dst = v.dst.or(Some(scalar(mask, lanes)));
                        v.src = vec![op0, op1];
                    }
                    Encoding::Vop1 => {
                        v.dst = Some(valu_dst(name, w0 >> 17 & 0xFF, dst_n));
                        v.src = vec![op0];
                    }
                    _ => {
                        v.dst = Some(vector(w0 >> 17 & 0xFF, dst_n));
                        v.src = match name {
                            n if n.ends_with("mk_f32") || n.ends_with("mk_f16") => {
                                vec![op0, Operand::Literal(ext), op1]
                            }
                            n if n.ends_with("ak_f32") || n.ends_with("ak_f16") => {
                                vec![op0, op1, Operand::Literal(ext)]
                            }
                            _ => vec![op0, op1],
                        };
                        if carry_in {
                            v.src.push(scalar(VCC, lanes));
                        }
                        if name.ends_with("_co_ci_u32") {
                            v.carry = Some(scalar(VCC, lanes));
                        }
                    }
                }
            }
            Encoding::Vop3 | Encoding::Vop3p => {
                let w1 = w(1);
                let field = |i: u32| w1 >> (9 * i) & 0x1FF;
                let count = match self.opcode {
                    _ if self.encoding == Encoding::Vop3p => {
                        if name.starts_with("v_pk_")
                            && !name.contains("_fma_")
                            && !name.contains("_mad_")
                        {
                            2
                        } else {
                            3
                        }
                    }
                    0x000..0x100 => 2,
                    0x100..0x140 => 2 + usize::from(carry_in),
                    0x180..0x200 => 1,
                    _ if VOP3_TWO_SOURCES.contains(&name) => 2,
                    _ => 3,
                };
                v.src = (0..count)
                    .map(|i| {
                        let code = field(i as u32);
                        if i == 0 && name.starts_with("v_interp_") {
                            Operand::Attr {
                                index: (code & 0x3F) as u8,
                                chan: (code >> 6 & 3) as u8,
                                high: code >> 8 & 1 == 1,
                            }
                        } else if i == 2 && carry_in {
                            source(code, lanes, w(2))
                        } else {
                            source(code, src_n[i], w(2))
                        }
                    })
                    .collect();
                v.clamp = w0 >> 15 & 1 == 1;
                v.neg = (w1 >> 29) as u8;
                if self.encoding == Encoding::Vop3p {
                    v.neg_hi = (w0 >> 8 & 7) as u8;
                    v.opsel = (w0 >> 11 & 7) as u8;
                    v.opsel_hi = (w1 >> 27 & 3 | w0 >> 12 & 4) as u8;
                } else {
                    v.omod = (w1 >> 27 & 3) as u8;
                    if vop3b {
                        v.carry = Some(scalar((w0 >> 8 & 0x7F) as u8, lanes));
                    } else {
                        v.abs = (w0 >> 8 & 7) as u8;
                        v.opsel = (w0 >> 11 & 0xF) as u8;
                    }
                }
                let vdst = w0 & 0xFF;
                v.dst = Some(if compare {
                    scalar(vdst as u8, lanes)
                } else {
                    valu_dst(name, vdst, dst_n)
                });
            }
            _ => unreachable!("not a VALU encoding"),
        }
        v
    }
}

