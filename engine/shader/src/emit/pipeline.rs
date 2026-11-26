use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram};
use crate::{ShaderError, io, ir, resource, structure};

use super::program::{emit_program, widen};
use super::{HostShader, lower, threading};

/// Translates the vertex, pixel, compute or mesh program of a console
/// shader container.
///
/// A program refused because the first half of an interpolation is read as
/// data is translated once more with first halves evaluated as the pair
/// they start (`lower::with_first_half_pairs`), and only such a program:
/// with that on for every program, 8,551 pixel programs of the title that
/// already translated came out changed (an extra evaluation in each), for
/// the sake of 9 that did not translate.
pub fn translate(container: &[u8]) -> Result<HostShader, ShaderError> {
    match translate_plain(container) {
        Err(ShaderError::Unsupported { what, .. }) if what == lower::FIRST_HALF => {
            lower::with_first_half_pairs(|| translate_plain(container))
        }
        other => other,
    }
}

fn translate_plain(container: &[u8]) -> Result<HostShader, ShaderError> {
    let c = Container::parse(container)?;
    let dxil = c
        .part(FourCC::DXIL)
        .or(c.part(FourCC::ACCC))
        .ok_or(ShaderError::Malformed("no DXIL part"))?;
    let native = crate::native_programs(dxil.data)?
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms
            )
        })
        .ok_or(ShaderError::Unsupported {
            offset: 0,
            what: "a container without a vertex, pixel, compute or mesh program",
        })?;
    let p = crate::spill::build(&native)?;
    // Structurisation first: tracing per-lane descriptors needs it (its
    // failure is reported after the earlier stages').
    let st = structure::structure(&p);
    let res = resource::track(&p, &native.layout, native.stage, st.as_ref().ok())?;
    let io = io::raise(&p, &native)?;
    let st = st?;
    emit(&c, &native, &p, &st, &io, &res)
}

/// Emits a program whose earlier stages already ran.
pub fn emit(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
) -> Result<HostShader, ShaderError> {
    // (what an earlier program's lowering on this thread left behind)
    lower::take_needs();
    let original = emit_program(c, native, p, st, io, res);
    // A console wave64 compute program on a host of 32-lane waves that
    // still needs 64 lanes, or whose cross-lane code sits where not every
    // lane runs: once more with its early-outs left out
    // ([`ir::without_early_outs`]), which is what usually keeps the whole
    // body from being a block every lane runs.
    let stuck = match &original {
        Ok((_, wave)) => *wave == Some(64),
        Err(ShaderError::Unsupported { what, .. }) => lower::needs_every_lane(what),
        Err(_) => false,
    };
    // (a mask with lanes elsewhere is only helped by the exact forms: not by each half-wave
    // on its own)
    let exact_only = matches!(&original, Err(ShaderError::Unsupported { what, .. }) if *what == lower::MASK_ELSEWHERE);
    if stuck && native.stage == HwStage::Cs && native.wave64 && lower::host_max_wave() == 32 {
        let why = std::env::var_os("OATHKEEP_WAVE_WHY").is_some();
        let report = |what: &str, r: &Result<(HostShader, Option<u32>), ShaderError>| {
            if why {
                match r {
                    Ok((_, wave)) => eprintln!("WAVE64-RETRY {what}: wave {wave:?}"),
                    Err(e) => eprintln!("WAVE64-RETRY {what}: {e}"),
                }
            }
        };
        let done = |r: &Result<(HostShader, Option<u32>), ShaderError>| matches!(r, Ok((_, wave)) if *wave != Some(64));
        // Each host wave as a wave of its own (`lower::with_local_waves`),
        // for programs still stopped where not every lane runs.
        let local_on = std::env::var_os("OATHKEEP_WAVE_LOCAL").is_none_or(|v| v != "0");
        // Where the wave operations are the allocation idiom
        // ([`lower::wave_local`]) that is exact. Other programs are taken
        // the same way last of all, each half-wave giving its own results
        // (approximate: a tile-wide result becomes two half-tile ones);
        // `OATHKEEP_WAVE_LOCAL=idiom` refuses those instead.
        let strict = std::env::var_os("OATHKEEP_WAVE_LOCAL").is_some_and(|v| v == "idiom");
        let idiom = |p: &ir::Program, st: &structure::Structured| match lower::wave_local(p, st) {
            Ok(()) => true,
            Err((offset, what)) => {
                if why {
                    eprintln!("WAVE64-RETRY local waves: not the allocation idiom at {offset:#x}: {what}");
                }
                false
            }
        };
        // The program in two other forms: without its early-outs, and
        // without any branch that only skips code no lane is left for.
        let form = |label: &str, p: Option<ir::Program>| {
            let Some(p) = p else {
                if why {
                    eprintln!("WAVE64-FORM {label}: ir returned None");
                }
                return None;
            };
            let st = match structure::structure(&p) {
                Ok(st) => st,
                Err(e) => {
                    if why {
                        eprintln!("WAVE64-FORM {label}: structure failed: {e}");
                    }
                    return None;
                }
            };
            let io = match io::raise(&p, native) {
                Ok(io) => io,
                Err(e) => {
                    if why {
                        eprintln!("WAVE64-FORM {label}: io::raise failed: {e}");
                    }
                    return None;
                }
            };
            let res = match resource::track(&p, &native.layout, native.stage, Some(&st)) {
                Ok(res) => res,
                Err(e) => {
                    if why {
                        eprintln!("WAVE64-FORM {label}: resource::track failed: {e}");
                    }
                    return None;
                }
            };
            Some((p, st, io, res))
        };
        let forms = [
            ("without early-outs", form("without early-outs", ir::without_early_outs(p))),
            ("without skips", form("without skips", ir::without_skips(p, false))),
        ];
        for (name, f) in &forms {
            if let Some((p, st, io, res)) = f {
                let again = emit_program(c, native, p, st, io, res);
                report(name, &again);
                if done(&again) {
                    return again.map(|(shader, _)| shader);
                }
            }
        }
        // Off unless asked for (`OATHKEEP_WAVE_WIDE=1`), for a console wave run as two host
        // waves: this form puts group barriers inside branches and loops, and runs 121-124
        // lost the card in a submission of two programs that had just moved to it
        // (`ptfx_CS_lightAtlas_Sprite` and `_Trail`). The 32-lane programs' form below, which
        // has no barrier, stays on.
        let wide_option = std::env::var_os("OATHKEEP_WAVE_WIDE");
        // Both archived kernels use one logical wave per group and have
        // validated wave32 PSOs on RTX. Explicit policy wins.
        let scene_kernel = matches!(native.io, crate::native::StageIo::Cs(cs) if cs.threads.iter().product::<u32>() == 64)
            && matches!(
                c.digest,
                [0x58, 0x1d, 0xab, 0x2c, 0xd8, 0xbc, 0xf2, 0x2e, 0x5c, 0x96, 0x4b, 0x98, 0xe9, 0x03, 0x48, 0xd1]
                    | [0x24, 0x31, 0xbb, 0xe7, 0xdc, 0x15, 0x4a, 0xd7, 0x59, 0xd2, 0x0b, 0xc9, 0x54, 0x82, 0x9c, 0x1c]
            );
        // Single-console-wave (64-thread) compute kernels that allocate LDS and
        // stayed at Wave64 in `original` only because `lds_meetings` flagged an
        // LDS read+write inside a lane-divergent branch (`Ok((_, Some(64)))`)
        // safely lower via `widen()` without affecting particle kernels (`lds == 0`)
        // that failed `original` on wave intrinsics inside loops.
        let single_wave_lds = matches!(
            native.io,
            crate::native::StageIo::Cs(cs)
                if cs.threads.iter().product::<u32>() == 64 && ((cs.rsrc2 >> 15) & 0x1ff) > 0
        ) && matches!(&original, Ok((_, Some(64))));
        let multi_wave_group = matches!(
            native.io,
            crate::native::StageIo::Cs(cs)
                if cs.threads.iter().product::<u32>() > 64
                    && cs.threads.iter().product::<u32>() % 64 == 0
        );
        let wide_on = wide_option
            .as_ref()
            .map_or(scene_kernel || single_wave_lds || multi_wave_group, |v| v == "1");
        // A single guest wave can iterate an active-lane loop until both host
        // halves have finished. The precise latch and convergence conditions
        // for the driver compute route are checked by widen().
        let driver_loop = wide_option.is_none()
            && c.part(FourCC::DXIL).is_none()
            && c.part(FourCC::ACCC).is_some()
            && matches!(&original, Err(ShaderError::Unsupported { what, .. })
                if *what == lower::DIVERGENT_MASK_EXCHANGE);
        // exact first: each host wave on its own where that is exact (the idiom, every
        // form; no barriers), then the other exact forms; then approximate
        for exact in [true, false] {
            if !exact {
                // Before anything approximate, the exact forms that cost group barriers.
                // Every skip left out that the code after it cannot see
                // ([`ir::without_skips`], `deep`): most of the body is then code every
                // lane runs, where the two host waves can exchange.
                // (`OATHKEEP_WAVE_DEEP_SKIPS=0` and `OATHKEEP_WAVE_WIDE=0` turn the two off.)
                let deep_on = std::env::var_os("OATHKEEP_WAVE_DEEP_SKIPS").is_none_or(|v| v != "0");
                let deep = if deep_on { form("without skips, scalars followed", ir::without_skips(p, true)) } else { None };
                if let Some((p, st, io, res)) = &deep {
                    let again = emit_program(c, native, p, st, io, res);
                    report("without skips, scalars followed", &again);
                    if done(&again) {
                        return again.map(|(shader, _)| shader);
                    }
                }
                // Still stopped where not every lane runs: the per-lane branches around
                // those places taken as the console takes them, by the whole wave at once
                // ([`widen`]), at two group barriers per such branch.
                if wide_on || driver_loop {
                    let bases = [
                        deep.as_ref().map(|f| &f.0),
                        forms[1].1.as_ref().map(|f| &f.0),
                        forms[0].1.as_ref().map(|f| &f.0),
                        Some(p),
                    ];
                    if let Some(base) = bases.into_iter().flatten().next() {
                        let again = widen(c, native, base, why, !wide_on && driver_loop);
                        report("wave-wide branches", &again);
                        if done(&again) {
                            return again.map(|(shader, _)| shader);
                        }
                    }
                }
                if strict {
                    break;
                }
            }
            if !local_on || exact_only {
                continue;
            }
            for (name, f) in &forms {
                if let Some((p, st, io, res)) = f
                    && idiom(p, st) == exact
                {
                    let local = lower::with_local_waves(|| emit_program(c, native, p, st, io, res));
                    report(&format!("local waves ({}), {name}", if exact { "idiom" } else { "APPROXIMATE" }), &local);
                    if done(&local) {
                        return local.map(|(shader, _)| shader);
                    }
                }
            }
            if idiom(p, st) == exact {
                let local = lower::with_local_waves(|| emit_program(c, native, p, st, io, res));
                report(if exact { "local waves (idiom)" } else { "local waves (APPROXIMATE)" }, &local);
                if done(&local) {
                    return local.map(|(shader, _)| shader);
                }
            }
        }
        if local_on && !exact_only {
            // Last: every mask and lane read per host wave.
            if !strict {
                for (name, f) in &forms {
                    if let Some((p, st, io, res)) = f {
                        let local = lower::with_local_waves_any(|| emit_program(c, native, p, st, io, res));
                        report(&format!("local waves (APPROXIMATE, any mask), {name}"), &local);
                        if done(&local) {
                            return local.map(|(shader, _)| shader);
                        }
                    }
                }
                let local = lower::with_local_waves_any(|| emit_program(c, native, p, st, io, res));
                report("local waves (APPROXIMATE, any mask)", &local);
                if done(&local) {
                    return local.map(|(shader, _)| shader);
                }
            }
        }
    }
    // SCC of a mask operation joined with a scalar test behind a per-lane skip (the 32-lane
    // trace programs): right once the wave takes that skip together ([`widen`]; the
    // lowering has named the skipped blocks).
    let original = match original {
        Err(ShaderError::Unsupported { what, .. })
            if native.stage == HwStage::Cs
                && what == lower::LANE_MASK
                && std::env::var_os("OATHKEEP_WAVE_WIDE").is_none_or(|v| v != "0") =>
        {
            let again = widen(c, native, p, std::env::var_os("OATHKEEP_WAVE_WHY").is_some(), false);
            match again {
                Ok(_) => again,
                Err(_) => emit_program(c, native, p, st, io, res),
            }
        }
        other => other,
    };
    let supported = |(shader, wave): (HostShader, Option<u32>)| {
        if wave == Some(64) && lower::host_max_wave() == 32 {
            Err(ShaderError::Unsupported {
                offset: 0,
                what: "no wave32 lowering for this wave64 program",
            })
        } else {
            Ok(shader)
        }
    };
    let original = original.and_then(supported);
    // Retry unsupported programs only. Threading can separate an unknown
    // register value from its apparent use on a mutually exclusive path.
    if !matches!(original, Err(ShaderError::Unsupported { .. })) {
        return original;
    }
    let edges = threading::constant_edges(p, st);
    if edges.is_empty() {
        return original;
    }
    let p = ir::thread_edges(p, &edges)?;
    let st = structure::structure(&p)?;
    let io = io::raise(&p, native)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    emit_program(c, native, &p, &st, &io, &res).and_then(supported)
}

