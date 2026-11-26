use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

use super::IsaFamily;
use super::text::{
    collect_match_string_arms, collect_module_files, collect_string_literals_in_slice,
    extract_brace_body, extract_bracket_body, read_module_sources, strip_line_comment,
};

pub(super) fn scan_rdna2_isa(root: &Path) -> Result<Vec<IsaFamily>> {
    let shader_src = root.join("engine/shader/src");
    let opcodes_src = read_module_sources(&shader_src.join("gfx10/opcodes"))?;
    let gfx10_mod = read_module_sources(&shader_src.join("gfx10/mod.rs"))?;
    let ir_raw = read_module_sources(&shader_src.join("ir"))?;
    let lower_raw = read_module_sources(&shader_src.join("emit/lower"))?;
    let intersect_raw = if collect_module_files(&shader_src.join("emit/intersect")).is_empty() {
        String::new()
    } else {
        read_module_sources(&shader_src.join("emit/intersect"))?
    };

    let ir_src = ir_raw.as_str();
    let lower_src = lower_raw.as_str();
    let intersect_src = intersect_raw.as_str();

    // 1. Parse all 19 opcode tables from `gfx10/opcodes.rs`
    let mut raw_families: Vec<(String, Vec<String>)> = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = opcodes_src[pos..].find("pub(super) const ") {
        let start = pos + rel + "pub(super) const ".len();
        let Some(colon) = opcodes_src[start..].find(':') else {
            break;
        };
        let fam_name = opcodes_src[start..start + colon].trim().to_owned();
        let Some(eq_bracket) = opcodes_src[start + colon..].find("= &[") else {
            break;
        };
        let open = start + colon + eq_bracket + 3;
        let Some(body) = extract_bracket_body(&opcodes_src, open) else {
            break;
        };
        let mut ops = Vec::new();
        for line in body.lines() {
            let clean = strip_line_comment(line);
            if let Some(q1) = clean.find('"') {
                let after = &clean[q1 + 1..];
                if let Some(q2) = after.find('"') {
                    ops.push(after[..q2].to_owned());
                }
            }
        }
        raw_families.push((fam_name, ops));
        pos = open + body.len() + 1;
    }
    // Plus `EXP` which is decoded inline in `gfx10/mod.rs` (`Encoding::Exp => return Some("exp")`).
    if gfx10_mod.contains("Encoding::Exp") {
        raw_families.push(("EXP".into(), vec!["exp".into()]));
    }

    // 2. Collect exact string literals from `lower.rs` (excluding explicit error/rejection lines) + `intersect.rs`
    let lower_literals = collect_lowered_literals(lower_src, intersect_src);
    let ir_sopp_accepted = collect_sopp_accepted(ir_src);
    let packed16_ops = collect_match_string_arms(lower_src, "fn packed16(");
    let ds_atomic_ops = collect_match_string_arms(lower_src, "let rmw = match op.as_str()");
    let buf_atomic_ops = collect_match_string_arms(lower_src, "match name.strip_prefix(\"buffer_atomic_\")");
    let img_atomic_ops = collect_match_string_arms(lower_src, "if let Some(op) = name.strip_prefix(\"image_atomic_\")");
    let glob_atomic_ops = collect_match_string_arms(lower_src, "let simple = match op");
    let glob_load_suffixes = collect_match_string_arms(lower_src, "let (n, sub) = match rest");
    let glob_store_suffixes = collect_match_string_arms(lower_src, "let count = |rest: &str| match rest");

    // 3. Evaluate decoded and lowered flags per opcode per family
    let mut families = Vec::new();
    for (fam, ops) in raw_families {
        let fam_decoded = fam == "EXP" || gfx10_mod.contains(&format!("t::{fam}"));
        let decoded = vec![fam_decoded; ops.len()];
        let lowered = ops
            .iter()
            .map(|op| {
                is_opcode_lowered(
                    &fam,
                    op,
                    &lower_literals,
                    &ir_sopp_accepted,
                    &packed16_ops,
                    &ds_atomic_ops,
                    &buf_atomic_ops,
                    &img_atomic_ops,
                    &glob_atomic_ops,
                    &glob_load_suffixes,
                    &glob_store_suffixes,
                )
            })
            .collect();
        families.push(IsaFamily {
            name: fam,
            opcodes: ops,
            decoded,
            lowered,
        });
    }

    Ok(families)
}

fn collect_lowered_literals(lower_src: &str, intersect_src: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for src in [lower_src, intersect_src] {
        for line in src.lines() {
            let clean = strip_line_comment(line).trim();
            // Skip explicit rejection lines & diagnostic strings
            if clean.starts_with("return Err(")
                || clean.starts_with("Err(")
                || clean.contains("Val::Missing(")
                || clean.contains(".op(")
            {
                continue;
            }
            set.extend(collect_string_literals_in_slice(clean));
        }
    }
    // Remove explicitly rejected SOP/IR opcodes even though their literal names appear in the rejection `matches!`.
    for rejected in [
        "s_setreg_imm32_b32",
        "s_setreg_b32",
        "s_swappc_b64",
        "s_version",
        "s_call_b64",
        "s_rfe_b64",
    ] {
        set.remove(rejected);
    }
    set
}

fn collect_sopp_accepted(ir_src: &str) -> HashSet<String> {
    let Some(pos) = ir_src.find("Fields::Sopp { .. } => match name {") else {
        return HashSet::new();
    };
    let Some(brace_rel) = ir_src[pos..].find("match name {") else {
        return HashSet::new();
    };
    let open = pos + brace_rel + "match name ".len();
    let Some(body) = extract_brace_body(ir_src, open) else {
        return HashSet::new();
    };
    collect_string_literals_in_slice(body)
        .into_iter()
        .filter(|s| s.starts_with("s_"))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn is_opcode_lowered(
    family: &str,
    op: &str,
    lower_literals: &HashSet<String>,
    ir_sopp_accepted: &HashSet<String>,
    packed16_ops: &HashSet<String>,
    ds_atomic_ops: &HashSet<String>,
    buf_atomic_ops: &HashSet<String>,
    img_atomic_ops: &HashSet<String>,
    glob_atomic_ops: &HashSet<String>,
    glob_load_suffixes: &HashSet<String>,
    glob_store_suffixes: &HashSet<String>,
) -> bool {
    match family {
        "EXP" => true,
        // `Fields::Flat` in `lower.rs:6219-6220` only handles `global_*`; `flat_*` and `scratch_*` return `Err("flat memory")`.
        "FLAT" | "SCRATCH" => false,
        "SOPP" => ir_sopp_accepted.contains(op),
        "SOP1" | "SOP2" | "SOPK" | "SOPC" => {
            if op.starts_with("s_movrel")
                || op.starts_with("s_subvector_loop")
                || op.starts_with("s_waitcnt")
                || matches!(
                    op,
                    "s_setreg_imm32_b32" | "s_setreg_b32" | "s_swappc_b64" | "s_version" | "s_call_b64" | "s_rfe_b64"
                )
            {
                return false;
            }
            if op.starts_with("s_bitcmp") {
                return true;
            }
            if let Some(rest) = op.strip_prefix("s_cmp_") {
                if rest.ends_with("_u64") {
                    return rest.starts_with("eq") || rest.starts_with("lg");
                }
                if rest.ends_with("_i32") || rest.ends_with("_u32") {
                    return matches!(&rest[..2], "eq" | "lg" | "gt" | "ge" | "lt" | "le");
                }
                return false;
            }
            if let Some(rest) = op.strip_prefix("s_cmpk_") {
                return (rest.ends_with("_i32") || rest.ends_with("_u32"))
                    && matches!(&rest[..2], "eq" | "lg" | "gt" | "ge" | "lt" | "le");
            }
            lower_literals.contains(op)
        }
        "SMEM" => {
            matches!(
                op,
                "s_load_dword"
                    | "s_load_dwordx2"
                    | "s_load_dwordx4"
                    | "s_load_dwordx8"
                    | "s_load_dwordx16"
                    | "s_buffer_load_dword"
                    | "s_buffer_load_dwordx2"
                    | "s_buffer_load_dwordx4"
                    | "s_buffer_load_dwordx8"
                    | "s_buffer_load_dwordx16"
            )
        }
        "VINTRP" => lower_literals.contains(op) || op.starts_with("v_interp_"),
        "VOPC" => {
            let Some(rest) = op
                .strip_prefix("v_cmp_")
                .or_else(|| op.strip_prefix("v_cmpx_"))
            else {
                return lower_literals.contains(op);
            };
            let Some((pred, ty)) = rest.rsplit_once('_') else {
                return false;
            };
            match ty {
                "f32" | "f16" => {
                    pred == "class"
                        || matches!(
                            pred,
                            "f" | "lt"
                                | "eq"
                                | "le"
                                | "gt"
                                | "lg"
                                | "ge"
                                | "o"
                                | "u"
                                | "nge"
                                | "nlg"
                                | "ngt"
                                | "nle"
                                | "neq"
                                | "nlt"
                                | "tru"
                        )
                }
                "u64" | "i64" => matches!(pred, "eq" | "ne" | "lg" | "lt" | "le" | "gt" | "ge"),
                "u32" | "i32" | "u16" | "i16" => {
                    matches!(pred, "eq" | "ne" | "lg" | "lt" | "le" | "gt" | "ge" | "f" | "t")
                }
                _ => false,
            }
        }
        "VOP3P" => {
            if let Some(rest) = op.strip_prefix("v_pk_") {
                packed16_ops.contains(rest)
            } else {
                lower_literals.contains(op)
            }
        }
        "VOP1" | "VOP2" | "VOP3" => {
            if op.starts_with("v_interp_") {
                return true;
            }
            lower_literals.contains(op)
        }
        "DS" => {
            if op == "ds_swizzle_b32" || op == "ds_bpermute_b32" {
                return true;
            }
            let base = op.trim_end_matches("_hi").trim_end_matches("_d16");
            if matches!(
                base,
                "ds_read_u8" | "ds_read_i8" | "ds_read_u16" | "ds_read_i16" | "ds_write_b8" | "ds_write_b16"
            ) && (base == op
                || (op.ends_with("_d16") && base.starts_with("ds_read"))
                || op.ends_with("_d16_hi"))
            {
                return true;
            }
            if let Some(rest) = op.strip_prefix("ds_read")
                && (rest.starts_with('2') || matches!(rest, "_b32" | "_b64" | "_b96" | "_b128"))
            {
                return true;
            }
            if let Some(rest) = op.strip_prefix("ds_write")
                && (rest.starts_with('2') || matches!(rest, "_b32" | "_b64" | "_b96" | "_b128"))
            {
                return true;
            }
            if let Some(rest) = op.strip_prefix("ds_") {
                let norm = rest.replace("_rtn", "");
                return ds_atomic_ops.contains(&norm);
            }
            false
        }
        "MUBUF" => {
            if op == "buffer_gl0_inv" || op == "buffer_gl1_inv" {
                return true;
            }
            if let Some(atom) = op.strip_prefix("buffer_atomic_") {
                return buf_atomic_ops.contains(atom);
            }
            if matches!(
                op,
                "buffer_load_ubyte"
                    | "buffer_load_sbyte"
                    | "buffer_load_ushort"
                    | "buffer_load_sshort"
                    | "buffer_store_byte"
                    | "buffer_store_short"
            ) {
                return true;
            }
            let plain = op.replace("_format_d16_", "_format_");
            if let Some(rest) = plain
                .strip_prefix("buffer_load_")
                .or_else(|| plain.strip_prefix("buffer_store_"))
            {
                return matches!(
                    rest,
                    "dword"
                        | "dwordx2"
                        | "dwordx3"
                        | "dwordx4"
                        | "format_x"
                        | "format_xy"
                        | "format_xyz"
                        | "format_xyzw"
                );
            }
            false
        }
        "MTBUF" => {
            let plain = op.replace("_format_d16_", "_format_");
            plain
                .strip_prefix("tbuffer_load_")
                .is_some_and(|rest| {
                    matches!(rest, "format_x" | "format_xy" | "format_xyz" | "format_xyzw")
                })
        }
        "MIMG" => {
            if op == "image_bvh_intersect_ray" || op == "image_bvh64_intersect_ray" {
                return true;
            }
            if let Some(atom) = op.strip_prefix("image_atomic_") {
                return img_atomic_ops.contains(atom);
            }
            lower_literals.contains(op)
        }
        "GLOBAL" => {
            if let Some(rest) = op.strip_prefix("global_load_") {
                return glob_load_suffixes.contains(rest);
            }
            if let Some(rest) = op.strip_prefix("global_store_") {
                return glob_store_suffixes.contains(rest);
            }
            if let Some(atom) = op.strip_prefix("global_atomic_") {
                return glob_atomic_ops.contains(atom);
            }
            false
        }
        _ => lower_literals.contains(op),
    }
}
