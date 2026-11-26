use super::super::fields::{Cache, Dpp, Operand, Valu};
use super::super::{Encoding, Instruction};

/// `s_atc_probe` immediate: 0..=64 in decimal, larger values in hex.
pub(super) fn atc_immediate(imm: u32) -> String {
    if imm <= 64 {
        imm.to_string()
    } else {
        format!("0x{imm:x}")
    }
}

/// Cache flags; images write `dlc` first.
pub(super) fn flags(out: &mut String, cache: &Cache, dlc_first: bool) {
    let dlc = (cache.dlc, " dlc");
    let (first, last) = if dlc_first {
        (dlc, (false, ""))
    } else {
        ((false, ""), dlc)
    };
    for (on, text) in [first, (cache.glc, " glc"), (cache.slc, " slc"), last] {
        if on {
            out.push_str(text);
        }
    }
    if out.starts_with(' ') {
        out.remove(0);
    }
}

pub(super) fn label(inst: &Instruction, imm: u16) -> String {
    let target = inst.offset as i64 + 4 * inst.words.len() as i64 + 4 * i64::from(imm as i16);
    format!("label_{target:04X}")
}

pub(super) fn hwreg(imm: u16) -> String {
    let (id, offset, size) = (imm & 0x3F, imm >> 6 & 0x1F, (imm >> 11 & 0x1F) + 1);
    let name = match id {
        1 => "HW_REG_MODE".into(),
        20 => "HW_REG_SHADER_FLAT_SCRATCH_LO".into(),
        21 => "HW_REG_SHADER_FLAT_SCRATCH_HI".into(),
        25 => "HW_REG_POPS_PACKER".into(),
        _ => id.to_string(),
    };
    if offset == 0 && size == 32 {
        format!("hwreg({name})")
    } else {
        format!("hwreg({name}, {offset}, {size})")
    }
}

pub(super) fn waitcnt(imm: u16) -> String {
    let vm = imm & 0xF | (imm >> 14) << 4;
    let exp = imm >> 4 & 7;
    let lgkm = imm >> 8 & 0x3F;
    let mut parts = Vec::new();
    if vm != 0x3F {
        parts.push(format!("vmcnt({vm})"));
    }
    if exp != 7 {
        parts.push(format!("expcnt({exp})"));
    }
    if lgkm != 0x3F {
        parts.push(format!("lgkmcnt({lgkm})"));
    }
    if matches!(parts.len(), 0 | 3) {
        return format!("0x{imm:04x}");
    }
    parts.join(" & ")
}

pub(super) fn sendmsg(imm: u16) -> String {
    let (msg, op, stream) = (imm & 0xF, imm >> 4 & 7, imm >> 8 & 3);
    let msg_name = match msg {
        2 => "MSG_GS".into(),
        3 => "MSG_GS_DONE".into(),
        7 => "MSG_ORDERED_PS_DONE".into(),
        9 => "MSG_GS_ALLOC_REQ".into(),
        _ => msg.to_string(),
    };
    let op_name = ["GS_OP_NOP", "GS_OP_CUT", "GS_OP_EMIT", "GS_OP_EMIT_CUT"]
        .get(usize::from(op))
        .map_or(op.to_string(), |s| s.to_string());
    if msg == 3 && op == 0 {
        format!("sendmsg({msg_name}, {op_name})")
    } else {
        format!("sendmsg({msg_name}, {op_name}, {stream})")
    }
}

pub(super) fn buffer_format(format: u8) -> String {
    const NAMES: [&str; 78] = [
        "INVALID",
        "8_UNORM",
        "8_SNORM",
        "8_USCALED",
        "8_SSCALED",
        "8_UINT",
        "8_SINT",
        "16_UNORM",
        "16_SNORM",
        "16_USCALED",
        "16_SSCALED",
        "16_UINT",
        "16_SINT",
        "16_FLOAT",
        "8_8_UNORM",
        "8_8_SNORM",
        "8_8_USCALED",
        "8_8_SSCALED",
        "8_8_UINT",
        "8_8_SINT",
        "32_UINT",
        "32_SINT",
        "32_FLOAT",
        "16_16_UNORM",
        "16_16_SNORM",
        "16_16_USCALED",
        "16_16_SSCALED",
        "16_16_UINT",
        "16_16_SINT",
        "16_16_FLOAT",
        "10_11_11_UNORM",
        "10_11_11_SNORM",
        "10_11_11_USCALED",
        "10_11_11_SSCALED",
        "10_11_11_UINT",
        "10_11_11_SINT",
        "10_11_11_FLOAT",
        "11_11_10_UNORM",
        "11_11_10_SNORM",
        "11_11_10_USCALED",
        "11_11_10_SSCALED",
        "11_11_10_UINT",
        "11_11_10_SINT",
        "11_11_10_FLOAT",
        "10_10_10_2_UNORM",
        "10_10_10_2_SNORM",
        "10_10_10_2_USCALED",
        "10_10_10_2_SSCALED",
        "10_10_10_2_UINT",
        "10_10_10_2_SINT",
        "2_10_10_10_UNORM",
        "2_10_10_10_SNORM",
        "2_10_10_10_USCALED",
        "2_10_10_10_SSCALED",
        "2_10_10_10_UINT",
        "2_10_10_10_SINT",
        "8_8_8_8_UNORM",
        "8_8_8_8_SNORM",
        "8_8_8_8_USCALED",
        "8_8_8_8_SSCALED",
        "8_8_8_8_UINT",
        "8_8_8_8_SINT",
        "32_32_UINT",
        "32_32_SINT",
        "32_32_FLOAT",
        "16_16_16_16_UNORM",
        "16_16_16_16_SNORM",
        "16_16_16_16_USCALED",
        "16_16_16_16_SSCALED",
        "16_16_16_16_UINT",
        "16_16_16_16_SINT",
        "16_16_16_16_FLOAT",
        "32_32_32_UINT",
        "32_32_32_SINT",
        "32_32_32_FLOAT",
        "32_32_32_32_UINT",
        "32_32_32_32_SINT",
        "32_32_32_32_FLOAT",
    ];
    NAMES
        .get(usize::from(format))
        .map_or(format!("{format}"), |n| format!("BUF_FMT_{n}"))
}

pub(super) fn valu(name: &str, v: &Valu, encoding: Encoding) -> String {
    let bit = |mask: u8, i: usize| mask >> i & 1 != 0;
    let mix = name.starts_with("v_fma_mix");
    let src = |i: usize| {
        let o = v.src[i];
        let mut s = o.to_string();
        if let Some(sdwa) = v.sdwa
            && bit(sdwa.sext, i)
        {
            s = format!("sext({s})");
        }
        if encoding == Encoding::Vop3p {
            if mix {
                if bit(v.opsel_hi, i) {
                    s = format!("{}({s})", if bit(v.opsel, i) { "sel_hi" } else { "sel_lo" });
                }
                if bit(v.neg_hi, i) {
                    s = format!("abs({s})");
                }
                if bit(v.neg, i) {
                    s = format!("-{s}");
                }
            } else {
                if bit(v.neg, i) && bit(v.neg_hi, i) {
                    s = format!("-{s}");
                }
                match (bit(v.opsel, i), bit(v.opsel_hi, i)) {
                    (false, true) => {}
                    (false, false) => s = format!("sel_lo({s})"),
                    (true, true) => s = format!("sel_hi({s})"),
                    (true, false) => s = format!("sel_hi_lo({s})"),
                }
            }
        } else {
            if bit(v.abs, i) {
                s = format!("abs({s})");
            }
            if bit(v.neg, i) {
                s = format!("-{s}");
            }
            // v_permlane*: the opsel bits are fi and bound_ctrl.
            if bit(v.opsel, i) && !name.starts_with("v_permlane") {
                s = format!("sel_hi({s})");
            }
        }
        s
    };
    let mut ops: Vec<String> = v
        .dst
        .iter()
        .chain(&v.carry)
        .map(|o| o.to_string())
        .collect();
    let mut order: Vec<usize> = (0..v.src.len()).collect();
    if name.starts_with("v_interp_") && order.len() > 1 {
        // dst, i/j, attribute, then the p1 result for p2.
        order.swap(0, 1);
    }
    ops.extend(order.into_iter().map(src));
    let mut out = ops.join(", ");
    let mut suffix = |s: &str| {
        out.push(' ');
        out.push_str(s);
    };
    if let Some(Operand::Attr { high: true, .. }) = v.src.first() {
        suffix("high");
    }
    let omod = ["", "mul:2", "mul:4", "div:2"][usize::from(v.omod & 3)];
    if name.starts_with("v_permlane") {
        if v.opsel & 1 != 0 {
            suffix("fi:1");
        }
        if v.opsel & 2 != 0 {
            suffix("bound_ctrl:1");
        }
    }
    if encoding == Encoding::Vop3p && !mix {
        // `-src` above covers a source negated in both halves; LLVM syntax
        // for the one-sided rest.
        let n = v.src.len();
        for (field, only) in [("neg_lo", v.neg & !v.neg_hi), ("neg_hi", v.neg_hi & !v.neg)] {
            if (0..n).any(|i| bit(only, i)) {
                let bits: Vec<String> =
                    (0..n).map(|i| u8::from(bit(only, i)).to_string()).collect();
                suffix(&format!("{field}:[{}]", bits.join(",")));
            }
        }
    }
    if let Some(sdwa) = v.sdwa {
        if v.clamp {
            suffix("clamp");
        }
        if !omod.is_empty() {
            suffix(omod);
        }
        let sel = [
            "BYTE_0", "BYTE_1", "BYTE_2", "BYTE_3", "WORD_0", "WORD_1", "DWORD", "?",
        ];
        if sdwa.dst_sel != 6 {
            suffix(&format!("dst_sel:{}", sel[usize::from(sdwa.dst_sel & 7)]));
        }
        if sdwa.dst_unused != 0 {
            let unused = ["UNUSED_PAD", "UNUSED_SEXT", "UNUSED_PRESERVE", "?"];
            suffix(&format!(
                "dst_unused:{}",
                unused[usize::from(sdwa.dst_unused & 3)]
            ));
        }
        for (i, s) in sdwa.src_sel.iter().take(v.src.len()).enumerate() {
            if *s != 6 {
                suffix(&format!("src{i}_sel:{}", sel[usize::from(s & 7)]));
            }
        }
    } else {
        if !omod.is_empty() {
            suffix(omod);
        }
        if v.clamp {
            suffix("clamp");
        }
    }
    match v.dpp {
        Some(Dpp::Row {
            ctrl,
            row_mask,
            bank_mask,
            bound_ctrl,
            fi,
        }) => {
            suffix(&match ctrl {
                0x000..=0x0FF => format!(
                    "quad_perm:[{},{},{},{}]",
                    ctrl & 3,
                    ctrl >> 2 & 3,
                    ctrl >> 4 & 3,
                    ctrl >> 6 & 3
                ),
                0x101..=0x10F => format!("row_shl:{}", ctrl & 0xF),
                0x111..=0x11F => format!("row_shr:{}", ctrl & 0xF),
                0x121..=0x12F => format!("row_ror:{}", ctrl & 0xF),
                0x140 => "row_mirror".into(),
                0x141 => "row_half_mirror".into(),
                0x150..=0x15F => format!("row_share:{}", ctrl & 0xF),
                0x160..=0x16F => format!("row_xmask:{}", ctrl & 0xF),
                _ => format!("dpp_ctrl:0x{ctrl:x}"),
            });
            if row_mask != 0xF {
                suffix(&format!("row_mask:0x{row_mask:x}"));
            }
            if bank_mask != 0xF {
                suffix(&format!("bank_mask:0x{bank_mask:x}"));
            }
            if bound_ctrl {
                suffix("bound_ctrl:1");
            }
            if fi {
                suffix("fi:1");
            }
        }
        Some(Dpp::Lanes { sel, fi }) => {
            let lanes: Vec<String> = (0..8).map(|i| (sel >> (3 * i) & 7).to_string()).collect();
            suffix(&format!("dpp8:[{}]", lanes.join(",")));
            if fi {
                suffix("fi:1");
            }
        }
        None => {}
    }
    out
}

