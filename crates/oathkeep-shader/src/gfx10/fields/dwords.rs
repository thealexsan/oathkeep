use super::Operand;

/// VOP3-only opcodes that take two sources (the rest take three).
pub(super) const VOP3_TWO_SOURCES: &[&str] = &[
    "v_add_f64",
    "v_mul_f64",
    "v_min_f64",
    "v_max_f64",
    "v_ldexp_f64",
    "v_mul_lo_u32",
    "v_mul_hi_u32",
    "v_mul_lo_i32",
    "v_mul_hi_i32",
    "v_trig_preop_f64",
    "v_lshlrev_b64",
    "v_lshrrev_b64",
    "v_ashrrev_i64",
    "v_add_nc_u16",
    "v_sub_nc_u16",
    "v_mul_lo_u16",
    "v_lshrrev_b16",
    "v_ashrrev_i16",
    "v_max_u16",
    "v_max_i16",
    "v_min_u16",
    "v_min_i16",
    "v_add_nc_i16",
    "v_sub_nc_i16",
    "v_add_co_u32",
    "v_sub_co_u32",
    "v_subrev_co_u32",
    "v_pack_b32_f16",
    "v_cvt_pknorm_i16_f16",
    "v_cvt_pknorm_u16_f16",
    "v_lshlrev_b16",
    "v_interp_p1ll_f16",
    "v_readlane_b32",
    "v_writelane_b32",
    "v_ldexp_f32",
    "v_bfm_b32",
    "v_bcnt_u32_b32",
    "v_mbcnt_lo_u32_b32",
    "v_mbcnt_hi_u32_b32",
    "v_cvt_pknorm_i16_f32",
    "v_cvt_pknorm_u16_f32",
    "v_cvt_pk_u16_u32",
    "v_cvt_pk_i16_i32",
    "v_sub_nc_i32",
    "v_add_nc_i32",
];

/// VOP3 opcodes whose bits 14:8 hold a scalar destination instead of `abs`/`opsel`.
pub(super) const VOP3B: &[&str] = &[
    "v_add_co_u32",
    "v_sub_co_u32",
    "v_subrev_co_u32",
    "v_add_co_ci_u32",
    "v_sub_co_ci_u32",
    "v_subrev_co_ci_u32",
    "v_div_scale_f32",
    "v_div_scale_f64",
    "v_mad_u64_u32",
    "v_mad_i64_i32",
];

pub(super) fn scalar(code: u8, dwords: u8) -> Operand {
    Operand::Scalar { code, dwords }
}

pub(super) fn vector(reg: u32, dwords: u8) -> Operand {
    Operand::Vector {
        reg: reg as u8,
        dwords,
    }
}

/// A 9-bit source field (8-bit scalar fields are the same codes below 256).
pub(super) fn source(code: u32, dwords: u8, literal: u32) -> Operand {
    match code {
        0..=127 => scalar(code as u8, dwords),
        255 => Operand::Literal(literal),
        256.. => vector(code - 256, dwords),
        _ => Operand::Const(code as u8),
    }
}

/// VALU destinations that are SGPRs.
pub(super) fn valu_dst(name: &str, field: u32, dwords: u8) -> Operand {
    match name {
        "v_readlane_b32" | "v_readfirstlane_b32" => scalar(field as u8, 1),
        _ => vector(field, dwords),
    }
}

/// LDS opcodes on two addresses (`ds_read2_b32`...), with `offset0`/`offset1`.
pub(in crate::gfx10) fn ds_pair(name: &str) -> bool {
    name.split('_').any(|t| {
        ["read2", "write2", "wrxchg2"]
            .iter()
            .any(|p| t.starts_with(p))
    })
}

/// Dwords of a type suffix such as `b64` or `f32`.
pub(super) fn type_dwords(token: &str) -> u8 {
    match token {
        "b64" | "u64" | "i64" | "f64" => 2,
        _ => 1,
    }
}

/// (sdst, ssrc0, ssrc1) dwords of a scalar ALU opcode.
pub(super) fn salu_dwords(name: &str) -> (u8, u8, u8) {
    match name {
        "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" | "s_bfe_u64" | "s_bfe_i64"
        | "s_bitcmp0_b64" | "s_bitcmp1_b64" => (2, 2, 1),
        "s_bfm_b64" | "s_bitset0_b64" | "s_bitset1_b64" | "s_bitreplicate_b64_b32" => (2, 1, 1),
        "s_bcnt0_i32_b64" | "s_bcnt1_i32_b64" | "s_ff0_i32_b64" | "s_ff1_i32_b64"
        | "s_flbit_i32_b64" | "s_flbit_i32_i64" => (1, 2, 2),
        _ => {
            let n = type_dwords(name.rsplit('_').next().unwrap_or(""));
            (n, n, n)
        }
    }
}

/// (vdst, [src0, src1, src2]) dwords of a VALU opcode.
pub(super) fn valu_dwords(name: &str) -> (u8, [u8; 3]) {
    let parts: Vec<&str> = name.split('_').collect();
    match name {
        "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => (2, [1, 2, 2]),
        "v_ldexp_f64" | "v_trig_preop_f64" | "v_cmp_class_f64" | "v_cmpx_class_f64" => {
            (2, [2, 1, 2])
        }
        "v_mad_u64_u32" | "v_mad_i64_i32" => (2, [1, 1, 2]),
        "v_qsad_pk_u16_u8" | "v_mqsad_pk_u16_u8" => (2, [2, 1, 2]),
        "v_mqsad_u32_u8" => (4, [2, 1, 4]),
        "v_frexp_exp_i32_f64" => (1, [2; 3]),
        _ if parts.len() == 4 && parts[1] == "cvt" => {
            (type_dwords(parts[2]), [type_dwords(parts[3]); 3])
        }
        _ => {
            let n = type_dwords(parts.last().copied().unwrap_or(""));
            (n, [n; 3])
        }
    }
}

/// Dwords of loaded, stored or atomic data, from the opcode name.
pub(super) fn data_dwords(name: &str) -> u8 {
    let mut n: u8 = 1;
    for token in name.split('_') {
        n = match token {
            "dwordx2" | "x2" | "xy" | "b64" | "u64" | "i64" | "f64" => 2,
            "dwordx3" | "xyz" | "b96" => 3,
            "dwordx4" | "xyzw" | "b128" => 4,
            "dwordx8" => 8,
            "dwordx16" => 16,
            _ => n,
        };
    }
    if name.contains("cmpswap") {
        n *= 2;
    }
    if name.contains("format_d16") {
        n = n.div_ceil(2);
    }
    n
}

/// Address dwords of an image opcode: offset, bias, compare, derivatives,
/// coordinates, then LOD/clamp/mip, packed in pairs with `a16`.
pub(super) fn image_addr_dwords(name: &str, dim: u8, a16: bool) -> u8 {
    // (coordinates, derivatives per direction) for 1D 2D 3D CUBE 1D_ARRAY
    // 2D_ARRAY 2D_MSAA 2D_MSAA_ARRAY.
    const DIMS: [(u8, u8); 8] = [
        (1, 1),
        (2, 2),
        (3, 3),
        (3, 2),
        (2, 1),
        (3, 2),
        (3, 2),
        (4, 2),
    ];
    let (coords, grads) = DIMS[usize::from(dim & 7)];
    match name {
        "image_bvh_intersect_ray" => return if a16 { 8 } else { 11 },
        "image_bvh64_intersect_ray" => return if a16 { 9 } else { 12 },
        "image_get_resinfo" => return 1,
        _ => {}
    }
    let (mut whole, mut derivs, mut packed): (u8, u8, u8) = (0, 0, coords);
    if let Some(rest) = name
        .strip_prefix("image_sample")
        .or_else(|| name.strip_prefix("image_gather4"))
    {
        for token in rest.split('_') {
            match token {
                "o" | "b" | "c" => whole += 1,
                "d" | "cd" => derivs = grads,
                "l" | "cl" => packed += 1,
                _ => {}
            }
        }
    } else if name.contains("_mip") {
        packed += 1;
    }
    if a16 {
        whole + 2 * derivs.div_ceil(2) + packed.div_ceil(2)
    } else {
        whole + 2 * derivs + packed
    }
}
