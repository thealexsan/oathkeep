use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram};
use crate::{ShaderError, io, ir, resource, structure};

use super::signature::{signature, vertex_output_interpolation};
use super::{HostShader, StaticSampler, container, lower};

/// `p` (a console wave64 compute program on a host of 32-lane waves) with the per-lane
/// branches around its cross-lane code taken wave-wide ([`structure::structure_wide`]): the
/// lowering names the blocks where the two host waves must meet and could not
/// (`lower::take_needs`), the branches whose regions hold them are widened, and so on until
/// it translates or a branch cannot be widened. Only where a thread group is one console
/// wave: with several, the waves of a group would decide differently and a barrier inside
/// such a branch would wait for threads that never come.
pub(super) fn widen(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    why: bool,
    active_loop_only: bool,
) -> Result<(HostShader, Option<u32>), ShaderError> {
    const NOT: ShaderError = ShaderError::Unsupported {
        offset: 0,
        what: "wave-wide branches in a thread group of several console waves",
    };
    let multi_wave = match native.io {
        // (a program whose waves are the host's meets no one: no barrier, any group)
        crate::native::StageIo::Cs(_) if !(native.wave64 && lower::host_max_wave() == 32) => None,
        crate::native::StageIo::Cs(cs) if cs.threads.iter().product::<u32>() == 64 => None,
        crate::native::StageIo::Cs(cs)
            if cs.threads.iter().product::<u32>() > 64
                && cs.threads.iter().product::<u32>() % 64 == 0 =>
        {
            Some(cs.threads)
        }
        crate::native::StageIo::Cs(cs) => {
            if why {
                eprintln!("WAVE64-WIDE not tried: thread groups of {:?}", cs.threads);
            }
            return Err(NOT);
        }
        _ => return Err(NOT),
    };
    let mut wide = std::collections::HashSet::new();
    loop {
        let st = structure::structure_wide(p, &wide)?;
        let io = io::raise(p, native)?;
        let res = resource::track(p, &native.layout, native.stage, Some(&st))?;
        lower::take_needs();
        let r = emit_program(c, native, p, &st, &io, &res);
        let needs = lower::take_needs();
        // (or translated for 64-lane waves still: shared memory read and written where not
        // every lane runs, whose blocks the lowering names too)
        let stuck = matches!(&r, Err(ShaderError::Unsupported { what, .. })
                if lower::needs_every_lane(what) || *what == lower::LANE_MASK && !needs.is_empty())
            || matches!(&r, Ok((_, Some(64)))) && !needs.is_empty();
        if !stuck {
            if let Some(threads) = multi_wave
                && r.is_ok()
                && !wide.is_empty()
                && !lower::multi_wave_wide_safe(p, &st, &io, threads)
            {
                if why {
                    eprintln!("WAVE64-WIDE rejected: multi-wave widened branches not single-wave safe");
                }
                return Err(NOT);
            }
            if why {
                let order = ir::reverse_postorder(&p.blocks);
                let mut rpo = vec![0usize; p.blocks.len()];
                for (i, &b) in order.iter().enumerate() {
                    rpo[b] = i;
                }
                let mut sorted: Vec<usize> = wide.iter().copied().collect();
                sorted.sort_unstable();
                let desc: Vec<String> = sorted
                    .iter()
                    .map(|&b| {
                        let back = matches!(p.blocks[b].exit, ir::Exit::Branch { taken, not_taken } if rpo[taken] <= rpo[b] || rpo[not_taken] <= rpo[b]);
                        let nm = p.blocks[b].insts.last().map_or("?", |i| i.name());
                        format!("B{b}({nm}{})", if back { ",BACK" } else { "" })
                    })
                    .collect();
                eprintln!("WAVE64-WIDE {} branches taken wave-wide: [{}]", wide.len(), desc.join(", "));
            }
            return r;
        }
        let more: Vec<usize> = lower::branches_around(&st, &needs)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| !wide.contains(b))
            .collect();
        if more.is_empty() {
            if why {
                eprintln!("WAVE64-WIDE stopped with {} branches wave-wide: a branch around it cannot be", wide.len());
            }
            return r;
        }
        if active_loop_only {
            // Only a single backward branch on the SCC just set by
            // s_andn2_wrexec_b64. This instruction restricts EXEC to the
            // unfinished lanes. The guest repeats the loop if *either*
            // half still has work, while individual vector writes retain
            // their EXEC predicates.
            let [latch] = more.as_slice() else {
                return r;
            };
            let block = &p.blocks[*latch];
            let [.., update, branch] = block.insts.as_slice() else {
                return r;
            };
            if !wide.is_empty()
                || update.name() != "s_andn2_wrexec_b64"
                || branch.name() != "s_cbranch_scc1"
                || !matches!(
                    block.exit,
                    ir::Exit::Branch { taken, not_taken } if taken <= *latch && not_taken > *latch
                )
            {
                return r;
            }
        }
        wide.extend(more);
    }
}

/// The translated program, and the wave size it requires of the host.
pub(super) fn emit_program(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
) -> Result<(HostShader, Option<u32>), ShaderError> {
    let sig = |fourcc| -> Result<Vec<container::SigElement>, ShaderError> {
        match c.part(fourcc) {
            Some(part) => signature(part.data),
            None => Ok(Vec::new()),
        }
    };
    let pixel = native.stage == HwStage::Ps;
    let mut inputs = sig(FourCC::ISG1)?;
    let mut outputs = sig(FourCC::OSG1)?;
    // Interpolation as DXC declares it by default; pixel inputs that are
    // read get the mode their barycentrics say.
    let interpolated = if pixel { &mut inputs } else { &mut outputs };
    for e in interpolated {
        e.interp = match (e.kind, e.comp) {
            (3, _) => 4,
            // Constant, as DXC declares it (the AMD compiler crashed on a
            // front face without a mode).
            (13, _) => 1,
            // Until a read says which mode (`lower`).
            (28, _) => 2,
            (0 | 6 | 7, 9) => 2,
            (0, _) => 1,
            _ => 0,
        };
    }
    // (DexNom/Oathkeep 5f4c3df4.)
    if native.stage == HwStage::Vs
        && let Some(psv) = c.part(FourCC::PSV0)
    {
        vertex_output_interpolation(psv.data, &mut outputs)?;
    }
    // With a conservative depth output (SV_DepthLessEqual, GreaterEqual) DXIL
    // wants the position centroid (or per sample), as DXC declares it then.
    if pixel && outputs.iter().any(|e| matches!(e.kind, 18 | 19)) {
        for e in inputs.iter_mut().filter(|e| e.kind == 3) {
            e.interp = 5;
        }
    }
    // Per-primitive outputs are constant. The console never exports a
    // primitive id or cull flag a mesh shader writes (it culls with the
    // primitive export), so those rows are left out.
    let mut prims = sig(FourCC::PSG1)?;
    prims.retain(|e| !matches!(e.kind, 10 | 30));
    for e in &mut prims {
        e.interp = 1;
    }
    // A mesh group's id arrives multiplied by max(threads, vertices,
    // primitives) (the code divides by that); the thread count is in PSV0
    // from runtime info version 2 on.
    // shortcut: older containers fall back to the group's lane count, right
    // when the HLSL group fills its waves (the title's do).
    let mesh_stride = match &native.io {
        crate::native::StageIo::Ms(ms) => {
            let psv = c.part(FourCC::PSV0).map(|p| p.data);
            let threads = psv
                .filter(|d| crate::dxbc::u32_at(d, 0).is_ok_and(|n| n >= 48))
                .and_then(|d| crate::dxbc::u32_at(d, 4 + 36).ok());
            threads.map_or(ms.threads, |t| {
                t.max(ms.max_vertices).max(ms.max_primitives)
            })
        }
        _ => 0,
    };
    let l = lower::lower(&lower::Parts {
        native,
        p,
        st,
        io,
        res,
        inputs,
        outputs,
        prims,
        mesh_stride,
    })?;
    let wave_size = l.wave_size;
    let shader = HostShader {
        stage: native.stage,
        dxil: l.dxil,
        user_data: l.user_data,
        static_samplers: l
            .samplers
            .iter()
            .enumerate()
            .map(|(n, &w)| {
                let mut s = StaticSampler::from_words(w);
                if l.cmp_samplers >> n & 1 != 0 {
                    s.filter |= 0x80;
                }
                s
            })
            .collect(),
        samplers: l.samplers,
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        root_indexed_views: res.root_indexed_views.clone(),
    };
    Ok((shader, wave_size))
}

