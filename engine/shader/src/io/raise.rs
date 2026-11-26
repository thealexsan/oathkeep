use crate::ShaderError;
use crate::gfx10::{Fields, Operand};
use crate::ir::{Exit, Inst, Program, Reg, Value, operand_reads};
use crate::native::{HwStage, NativeProgram, PsInput, StageIo};
use crate::resource::Site;

use super::halves::{Halves, ps_vgprs, push_pair};
use super::{Attribute, Export, Input, Interp, Io, Target};

/// Raises the I/O of `p`, built from `native`'s patched code. Fails for
/// stages whose stage data is not read and on constructs not covered yet.
pub fn raise(p: &Program, native: &NativeProgram) -> Result<Io, ShaderError> {
    let unsupported = |offset, what| ShaderError::Unsupported { offset, what };
    let (loaded, ps_inputs, params) = match (&native.io, native.stage) {
        (
            StageIo::Ps {
                inputs, input_addr, ..
            },
            HwStage::Ps,
        ) => {
            // A pixel shader exports no parameters; `[]` never reaches `Unread`.
            let loaded = ps_vgprs(*input_addr)
                .into_iter()
                .map(|(r, i)| (Reg::V(r), i))
                .collect();
            (loaded, &inputs[..], &[][..])
        }
        (StageIo::Vs { inputs, outputs }, HwStage::Vs) => {
            let mut loaded = vec![
                (Reg::V(0), Input::VertexId),
                (Reg::V(2), Input::PrimitiveId),
                (Reg::V(3), Input::InstanceId),
            ];
            for i in inputs {
                for channel in (0..4).filter(|c| i.mask >> c & 1 != 0) {
                    let input = Input::Attribute {
                        row: i.row,
                        channel,
                    };
                    loaded.push((Reg::V(i.vgpr + channel), input));
                }
            }
            (loaded, &[][..], &outputs[..])
        }
        // Compute: thread ids in v0-v2, group ids and the group info in the
        // SGPRs after the user data.
        (StageIo::Cs(cs), HwStage::Cs) => {
            let (ids, info) = cs.system_sgprs();
            let mut loaded: Vec<(Reg, Input)> = (0..cs.thread_id_vgprs())
                .map(|c| (Reg::V(c), Input::ThreadIdInGroup(c)))
                .collect();
            for (c, s) in ids.into_iter().enumerate() {
                if let Some(s) = s {
                    loaded.push((Reg::S(s), Input::GroupId(c as u8)));
                }
            }
            loaded.extend(info.map(|s| (Reg::S(s), Input::GroupInfo)));
            (loaded, &[][..], &[][..])
        }
        (StageIo::Ms(ms), HwStage::Ms) => {
            // The group input, and the phis that may carry it past a join.
            let mut group: Vec<Value> = p
                .inputs
                .iter()
                .filter(|i| i.0 == Reg::V(5))
                .map(|i| i.1)
                .collect();
            // No lane is active over the EXEC-zero edge of s_cbranch_execz or
            // _execnz, the same case as a partial-EXEC write below.
            let no_lanes = |q: usize, b: usize| match (
                p.blocks[q].insts.last().map(Inst::name),
                p.blocks[q].exit,
            ) {
                (Some("s_cbranch_execz"), Exit::Branch { taken, not_taken }) => {
                    taken == b && not_taken != b
                }
                (Some("s_cbranch_execnz"), Exit::Branch { taken, not_taken }) => {
                    not_taken == b && taken != b
                }
                _ => false,
            };
            while let Some(v) = p
                .blocks
                .iter()
                .enumerate()
                .flat_map(|(b, k)| k.phis.iter().map(move |f| (b, f)))
                .find_map(|(b, f)| {
                    let mut args = f.args.iter().zip(&p.blocks[b].preds);
                    let carried = args.any(|(a, &q)| group.contains(a) && !no_lanes(q, b));
                    (carried && !group.contains(&f.value)).then_some(f.value)
                })
            {
                group.push(v);
            }
            // The group input holds the same value in every lane only as the
            // first lane reads it. A write to v5 reads its old value too, which
            // only lanes outside EXEC keep.
            // shortcut: writes that may keep the old v5 in active lanes
            // (v_fmac_f32's accumulator, a 16-bit half or d16 load,
            // v_writelane_b32, v_movreld_b32, DPP that skips lanes) and
            // partial-EXEC writes pass; check those too if a mesh program
            // writes v5 that way.
            let per_lane = p.blocks.iter().flat_map(|b| &b.insts).find(|i| {
                i.read(Reg::V(5)).is_some_and(|v| group.contains(&v))
                    && i.name() != "v_readfirstlane_b32"
                    && (i.write(Reg::V(5)).is_none() || operand_reads(p, i).contains(&Reg::V(5)))
            });
            if let Some(i) = per_lane {
                return Err(unsupported(
                    i.machine.offset,
                    "the mesh group input read per lane",
                ));
            }
            let loaded = vec![
                (Reg::V(5), Input::MeshGroup),
                (Reg::S(3), Input::MergedWaveInfo),
            ];
            (loaded, &[][..], &ms.outputs[..])
        }
        _ => return Err(unsupported(0, "I/O of this stage")),
    };
    let inputs: Vec<(Value, Input)> = p
        .inputs
        .iter()
        .filter_map(|&(reg, v)| loaded.iter().find(|l| l.0 == reg).map(|l| (v, l.1)))
        .collect();
    let input_of = |v: Value| inputs.iter().find(|i| i.0 == v).map(|i| i.1);
    let attribute = |inst: &Inst, attr: &Operand| match *attr {
        Operand::Attr { index, chan, high } => ps_inputs
            .iter()
            .find(|i: &&PsInput| i.attr == index)
            .map(|i| Attribute {
                usage: i.usage,
                row: i.row,
                channel: chan,
                high,
            })
            .ok_or(unsupported(
                inst.machine.offset,
                "an interpolated attribute the stage data does not list",
            )),
        _ => Err(unsupported(inst.machine.offset, "an interpolation operand")),
    };
    // A VGPR value's barycentric kind and index, if it is one as loaded.
    let barycentric = |v: Option<Value>| match v.and_then(input_of) {
        Some(Input::Barycentric { kind, index }) => Some((kind, index)),
        _ => None,
    };

    let mut io = Io {
        inputs: inputs.clone(),
        initialized_sgprs: match &native.io {
            StageIo::Cs(cs) => Some(cs.initialized_sgprs()),
            _ => None,
        },
        ..Io::default()
    };
    let defs: std::collections::HashMap<Value, Site> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, block)| {
            block
                .insts
                .iter()
                .enumerate()
                .flat_map(move |(i, inst)| inst.writes.iter().map(move |w| (w.1, (b, i))))
        })
        .collect();
    let phis: std::collections::HashMap<Value, (usize, usize)> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, block)| {
            block
                .phis
                .iter()
                .enumerate()
                .map(move |(i, phi)| (phi.value, (b, i)))
        })
        .collect();
    let halves = Halves {
        p,
        defs: &defs,
        phis: &phis,
    };
    let mut first_halves = Vec::new();
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            let site = (b, i);
            let at = inst.machine.offset;
            let name = inst.name();
            match &inst.fields {
                Fields::Vintrp { dst, src, attr } => {
                    let attribute = attribute(inst, attr)?;
                    match (name, *src) {
                        ("v_interp_mov_f32", Operand::Const(vertex)) => {
                            io.interps
                                .push((site, Interp::Vertex { attribute, vertex }));
                        }
                        ("v_interp_p1_f32", _) => first_halves.push((site, attribute)),
                        ("v_interp_p2_f32", Operand::Vector { reg, .. }) => {
                            let Operand::Vector { reg: d, .. } = *dst else {
                                return Err(unsupported(at, "an interpolation operand"));
                            };
                            // The first half's result is this one's old dst.
                            let interp = halves
                                .pair(
                                    attr,
                                    inst.read(Reg::V(d)),
                                    inst.read(Reg::V(reg)),
                                    &barycentric,
                                )
                                .map_err(|what| unsupported(at, what))?;
                            push_pair(&mut io, site, interp, attribute);
                        }
                        _ => return Err(unsupported(at, "an interpolation form")),
                    }
                }
                Fields::Valu(v) if name.starts_with("v_interp_") => match name {
                    "v_interp_p1ll_f16" => {
                        first_halves.push((site, attribute(inst, &v.src[0])?));
                    }
                    "v_interp_p2_f16" => {
                        let attribute = attribute(inst, &v.src[0])?;
                        let read = |k: usize| match v.src.get(k) {
                            Some(&Operand::Vector { reg, .. }) => inst.read(Reg::V(reg)),
                            _ => None,
                        };
                        let interp = halves
                            .pair(&v.src[0], read(2), read(1), &barycentric)
                            .map_err(|what| unsupported(at, what))?;
                        push_pair(&mut io, site, interp, attribute);
                    }
                    _ => return Err(unsupported(at, "an interpolation form")),
                },
                Fields::Exp {
                    target,
                    src,
                    compr,
                    done,
                    ..
                } => {
                    let target = match *target {
                        t @ 0..=7 => Target::Color(t),
                        8 => Target::Depth,
                        9 => Target::Null,
                        t @ 12..=16 => Target::Position(t - 12),
                        20 => Target::Primitive,
                        32 if params.is_empty() => Target::Unread,
                        t @ 32..=63 => {
                            // Bit 7: a mesh program's per-primitive output.
                            let o = params.iter().find(|o| o.param & 0x7F == t - 32).ok_or(
                                unsupported(at, "a parameter export the stage data does not list"),
                            )?;
                            let (usage, row) = (o.usage, o.row);
                            if o.param & 0x80 != 0 {
                                Target::PrimitiveParam { usage, row }
                            } else {
                                Target::Param { usage, row }
                            }
                        }
                        _ => return Err(unsupported(at, "an export target")),
                    };
                    let values = src.map(|s| match s {
                        Some(Operand::Vector { reg, .. }) => inst.read(Reg::V(reg)),
                        _ => None,
                    });
                    io.exports.push(Export {
                        site,
                        target,
                        values,
                        compressed: *compr,
                        done: *done,
                    });
                }
                _ => {}
            }
        }
    }
    if first_halves.is_empty() {
        return Ok(io);
    }
    // A loaded i/j pair normally becomes one host interpolation: its first
    // half need not be evaluated. A first half also read as data (or used
    // without a second half) must keep P0 + i * P10. Follow phi uses too,
    // without forcing ordinary pairs to read per-vertex attributes.
    let folded: std::collections::HashSet<_> = io
        .interps
        .iter()
        .filter_map(|(s, i)| matches!(i, Interp::Interpolated { .. }).then_some(*s))
        .collect();
    let mut used: std::collections::HashSet<_> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, blk)| {
            blk.insts
                .iter()
                .enumerate()
                .map(move |(i, inst)| ((b, i), inst))
        })
        .filter(|(s, _)| !folded.contains(s))
        .flat_map(|(_, inst)| inst.reads.iter().map(|r| r.1))
        .collect();
    loop {
        let before = used.len();
        for phi in p.blocks.iter().flat_map(|b| &b.phis) {
            if used.contains(&phi.value) {
                used.extend(&phi.args);
            }
        }
        if used.len() == before {
            break;
        }
    }
    for (site, attribute) in first_halves {
        if !io.interps.iter().any(|(s, _)| *s == site)
            && p.blocks[site.0].insts[site.1]
                .writes
                .iter()
                .any(|w| used.contains(&w.1))
        {
            io.interps.push((site, Interp::Weighted { attribute }));
        }
    }
    Ok(io)
}

