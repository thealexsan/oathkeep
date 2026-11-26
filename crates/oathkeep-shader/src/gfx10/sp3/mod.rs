//! Instruction text in AMD's SP3 assembly syntax.

use std::fmt::{self, Display, Write};

use super::fields::{Fields, NULL, Operand};
use super::Instruction;

mod format;
#[cfg(test)]
mod tests;

use format::{atc_immediate, buffer_format, flags, hwreg, label, sendmsg, valu, waitcnt};

impl Instruction {
    /// The instruction in SP3 syntax, e.g. `v_add_f32 v1, 0.5, v8`.
    /// Branch targets are written `label_<byte offset>`.
    pub fn text(&self, wave64: bool) -> String {
        let name = self.name().unwrap_or("?");
        let ops = operands(self, name, &self.fields(wave64));
        if ops.is_empty() {
            name.to_owned()
        } else {
            format!("{name} {ops}")
        }
    }
}

impl Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Operand::Scalar { code, dwords: 1 } => f.write_str(&scalar_name(code)),
            Operand::Scalar {
                code: 106,
                dwords: 2,
            } => f.write_str("vcc"),
            Operand::Scalar {
                code: 126,
                dwords: 2,
            } => f.write_str("exec"),
            Operand::Scalar { code, dwords } if code < 106 => {
                write!(f, "s[{code}:{}]", code + dwords - 1)
            }
            Operand::Scalar { code, dwords } if (108..124).contains(&code) => {
                write!(f, "ttmp[{}:{}]", code - 108, code - 108 + dwords - 1)
            }
            Operand::Scalar { code, .. } => f.write_str(&scalar_name(code)),
            Operand::Vector { reg, dwords: 1 } => write!(f, "v{reg}"),
            Operand::Vector { reg, dwords } => {
                write!(f, "v[{reg}:{}]", reg as u32 + dwords as u32 - 1)
            }
            Operand::Const(code) => f.write_str(&const_name(code)),
            Operand::Literal(value) => write!(f, "lit(0x{value:08x})"),
            Operand::Attr { index, chan, .. } => write!(
                f,
                "attr{index}.{}",
                ['x', 'y', 'z', 'w'][usize::from(chan & 3)]
            ),
        }
    }
}

pub(super) fn scalar_name(code: u8) -> String {
    match code {
        0..=105 => format!("s{code}"),
        106 => "vcc_lo".into(),
        107 => "vcc_hi".into(),
        108..=123 => format!("ttmp{}", code - 108),
        124 => "m0".into(),
        125 => "null".into(),
        126 => "exec_lo".into(),
        _ => "exec_hi".into(),
    }
}

pub(super) fn const_name(code: u8) -> String {
    match code {
        128..=192 => (i32::from(code) - 128).to_string(),
        193..=208 => (192 - i32::from(code)).to_string(),
        240 => "0.5".into(),
        241 => "-0.5".into(),
        242 => "1.0".into(),
        243 => "-1.0".into(),
        244 => "2.0".into(),
        245 => "-2.0".into(),
        246 => "4.0".into(),
        247 => "-4.0".into(),
        248 => "0.15915494".into(),
        235 => "src_shared_base".into(),
        236 => "src_shared_limit".into(),
        237 => "src_private_base".into(),
        238 => "src_private_limit".into(),
        239 => "src_pops_exiting_wave_id".into(),
        251 => "src_vccz".into(),
        252 => "src_execz".into(),
        253 => "src_scc".into(),
        254 => "src_lds_direct".into(),
        _ => format!("<src {code}>"),
    }
}

pub(super) fn join(ops: impl IntoIterator<Item = String>) -> String {
    ops.into_iter().collect::<Vec<_>>().join(", ")
}

fn operands(inst: &Instruction, name: &str, fields: &Fields) -> String {
    let mut out = String::new();
    match fields {
        Fields::Sop { dst, src, imm } => {
            let regs = join(dst.iter().chain(src).map(|o| o.to_string()));
            out = match (name, imm) {
                ("s_getreg_b32", Some(imm)) => format!("{regs}, {}", hwreg(*imm)),
                ("s_setreg_b32" | "s_setreg_imm32_b32", Some(imm)) => {
                    format!("{}, {regs}", hwreg(*imm))
                }
                ("s_call_b64", Some(imm)) => format!("{regs}, {}", label(inst, *imm)),
                (_, Some(imm)) if regs.is_empty() => format!("0x{imm:04x}"),
                (_, Some(imm)) => format!("{regs}, 0x{imm:04x}"),
                (_, None) => regs,
            };
        }
        Fields::Sopp { imm } => {
            out = match name {
                "s_endpgm"
                | "s_barrier"
                | "s_code_end"
                | "s_endpgm_saved"
                | "s_endpgm_ordered_ps_done"
                | "s_wakeup"
                | "s_icache_inv"
                | "s_ttracedata" => String::new(),
                "s_branch" => label(inst, *imm),
                n if n.starts_with("s_cbranch_") => label(inst, *imm),
                "s_waitcnt" => waitcnt(*imm),
                "s_sendmsg" | "s_sendmsghalt" => sendmsg(*imm),
                _ => format!("0x{imm:04x}"),
            };
        }
        Fields::Smem {
            data,
            base,
            offset,
            soffset,
            cache,
        } => {
            if let Some(data) = data {
                let data = match (name, data) {
                    ("s_atc_probe" | "s_atc_probe_buffer", Operand::Literal(imm)) => {
                        atc_immediate(*imm)
                    }
                    _ => data.to_string(),
                };
                let off = match (soffset, offset) {
                    (Operand::Scalar { code: NULL, .. }, 0) => soffset.to_string(),
                    (Operand::Scalar { code: NULL, .. }, _) => {
                        format!("0x{:06x}", offset & 0x1F_FFFF)
                    }
                    (_, 0) => soffset.to_string(),
                    _ => format!("{soffset} offset:0x{:06x}", offset & 0x1F_FFFF),
                };
                out = match name {
                    // The data is a counter or the wave ID; these have no address.
                    "s_memtime" | "s_memrealtime" | "s_get_waveid_in_workgroup" => data.to_string(),
                    _ => format!("{data}, {base}, {off}"),
                };
            }
            flags(&mut out, cache, false);
        }
        Fields::Valu(v) => out = valu(name, v, inst.encoding),
        Fields::Vintrp { dst, src, attr } => {
            let src = match (name, src) {
                ("v_interp_mov_f32", Operand::Const(p)) => ["p10", "p20", "p0"]
                    .get(usize::from(*p))
                    .map_or("p?".into(), |p| p.to_string()),
                _ => src.to_string(),
            };
            out = format!("{dst}, {src}, {attr}");
        }
        Fields::Ds {
            dst,
            addr,
            data,
            offset0,
            offset1,
            gds,
        } => {
            out = join(dst.iter().chain(addr).chain(data).map(|o| o.to_string()));
            if super::fields::ds_pair(name) {
                if *offset0 != 0 {
                    let _ = write!(out, " offset0:{offset0}");
                }
                if *offset1 != 0 {
                    let _ = write!(out, " offset1:{offset1}");
                }
            } else {
                let offset = u16::from(*offset1) << 8 | u16::from(*offset0);
                if offset != 0 {
                    let _ = write!(out, " offset:{offset}");
                }
            }
            if *gds {
                out.push_str(" gds");
            }
        }
        Fields::Buffer {
            data,
            addr,
            rsrc,
            soffset,
            offset,
            offen,
            idxen,
            format,
            lds,
            tfe,
            cache,
        } => {
            if let Some(data) = data {
                // An LDS load returns its data to LDS, so vdata is unused.
                out = if *lds && name.contains("_load_") {
                    format!("{addr}, {rsrc}, {soffset}")
                } else {
                    format!("{data}, {addr}, {rsrc}, {soffset}")
                };
                if *idxen {
                    out.push_str(" idxen");
                }
                if *offen {
                    out.push_str(" offen");
                }
                if *offset != 0 {
                    let _ = write!(out, " offset:{offset}");
                }
                if let Some(format) = format {
                    let _ = write!(out, " format:[{}]", buffer_format(*format));
                }
            }
            flags(&mut out, cache, false);
            if *lds {
                out.push_str(" lds");
            }
            if *tfe {
                out.push_str(" tfe");
            }
        }
        Fields::Image {
            data,
            addr,
            rsrc,
            sampler,
            dmask,
            dim,
            unorm,
            r128,
            a16,
            d16,
            tfe,
            lwe,
            cache,
        } => {
            let addr = match addr.as_slice() {
                [one] => one.to_string(),
                many => format!(
                    "[{}]",
                    many.iter()
                        .map(|o| o.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            };
            out = join(
                [data.to_string(), addr, rsrc.to_string()]
                    .into_iter()
                    .chain(sampler.map(|s| s.to_string())),
            );
            if *dmask != 1 {
                let _ = write!(out, " dmask:0x{dmask:x}");
            }
            let dims = [
                "1D",
                "2D",
                "3D",
                "CUBE",
                "1D_ARRAY",
                "2D_ARRAY",
                "2D_MSAA",
                "2D_MSAA_ARRAY",
            ];
            let _ = write!(out, " dim:SQ_RSRC_IMG_{}", dims[usize::from(*dim & 7)]);
            for (on, text) in [(*r128, "r128"), (*unorm, "unorm")] {
                if on {
                    out.push(' ');
                    out.push_str(text);
                }
            }
            flags(&mut out, cache, true);
            for (on, text) in [(*a16, "a16"), (*d16, "d16"), (*tfe, "tfe"), (*lwe, "lwe")] {
                if on {
                    out.push(' ');
                    out.push_str(text);
                }
            }
        }
        Fields::Flat {
            dst,
            addr,
            data,
            saddr,
            offset,
            lds,
            cache,
        } => {
            let addr = addr.map_or("off".into(), |a| a.to_string());
            out = join(
                dst.iter()
                    .map(|o| o.to_string())
                    .chain([addr])
                    .chain(data.iter().map(|o| o.to_string()))
                    .chain(saddr.map(|o| match o {
                        Operand::Scalar { code: NULL, .. } => "off".into(),
                        o => o.to_string(),
                    })),
            );
            if *offset != 0 {
                let _ = write!(out, " inst_offset:{offset}");
            }
            flags(&mut out, cache, false);
            if *lds {
                out.push_str(" lds");
            }
        }
        Fields::Exp {
            target,
            src,
            compr,
            done,
            vm,
        } => {
            let target = match *target {
                0..=7 => format!("mrt{target}"),
                8 => "mrtz".into(),
                9 => "null".into(),
                12..=15 => format!("pos{}", target - 12),
                20 => "prim".into(),
                32..=63 => format!("param{}", target - 32),
                _ => format!("<target {target}>"),
            };
            out = join(
                [target].into_iter().chain(
                    src.iter()
                        .map(|s| s.map_or("off".into(), |o| o.to_string())),
                ),
            );
            for (on, text) in [(*done, " done"), (*compr, " compr"), (*vm, " vm")] {
                if on {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

