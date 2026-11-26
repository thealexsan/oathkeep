use std::sync::Mutex;

use oathkeep_shader::emit::host::{self, Host};
use oathkeep_shader::emit::{self, validator::Validator};
use oathkeep_shader::{HwStage, NativeProgram, ir};

use super::lock;
use super::tally::{Tally, counter};

/// Emitted shaders by label, shared between threads.
pub(super) type Kept = std::sync::Arc<Mutex<Vec<(String, emit::HostShader)>>>;

/// The optional checks after emission, one set per thread.
pub(super) struct Checks {
    /// Shaders named in `OATHKEEP_PAIRS` (`vs+ps,...`, label substrings),
    /// kept to build those pipelines after the walk.
    pub(super) keep: Option<Kept>,
    pub(super) validator: Option<Validator>,
    pub(super) host: Option<(Host, emit::HostShader)>,
}

/// `OATHKEEP_SHADER_CENSUS=<file>`: one line per emitted program, appended:
/// its label, the console program's instructions by encoding class (a
/// static count of the whole program, not of one thread's path), and the
/// host module's instructions by kind (`emit::census::KINDS`).
pub(super) fn census_line(label: &str, native: &NativeProgram, p: &ir::Program, shader: &emit::HostShader) {
    use std::io::Write;
    use oathkeep_shader::gfx10::Fields;
    static OUT: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let Some(out) = OUT.get_or_init(|| {
        let path = std::env::var_os("OATHKEEP_SHADER_CENSUS")?;
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        Some(Mutex::new(file))
    }) else {
        return;
    };
    let Some(census) = emit::census::take(&shader.dxil) else {
        return;
    };
    // total, vector, scalar, descriptor loads, buffer and memory, image, LDS, control, other
    let mut console = [0u32; 9];
    for inst in p.blocks.iter().flat_map(|b| &b.insts) {
        console[0] += 1;
        let k = match &inst.fields {
            Fields::Valu(_) => 1,
            Fields::Sop { .. } => 2,
            Fields::Smem { .. } => 3,
            Fields::Buffer { .. } | Fields::Flat { .. } => 4,
            Fields::Image { .. } => 5,
            Fields::Ds { .. } => 6,
            Fields::Sopp { .. } => 7,
            _ => 8,
        };
        console[k] += 1;
    }
    let mut line = format!("{label}\twave64={}", u8::from(native.wave64));
    for (name, n) in ["c_total", "c_valu", "c_sop", "c_smem", "c_buffer", "c_image", "c_ds", "c_sopp", "c_other"].iter().zip(console) {
        line.push_str(&format!("\t{name}={n}"));
    }
    for (name, n) in emit::census::KINDS.iter().zip(census.counts) {
        line.push_str(&format!("\t{name}={n}"));
    }
    line.push('\n');
    let _ = lock(out).write_all(line.as_bytes());
}

/// Validates an emitted shader if asked to.
pub(super) fn check(
    mut shader: emit::HostShader,
    t: &mut Tally,
    checks: &mut Checks,
    label: impl Fn() -> String,
) {
    if let Some(keep) = &checks.keep {
        lock(keep).push((label(), shader.clone()));
    }
    // With `--list`: a hash of the module and of what the host binds for it,
    // to show which shaders a change to the translator touches.
    if t.list {
        let hash = blake3::hash(format!("{shader:?}").as_bytes()).to_hex();
        t.lines.push(format!("{}  emitted {}", label(), &hash[..16]));
    }
    // `OATHKEEP_EMIT_DUMP` with `--list`: every emitted shader as a file.
    if t.list
        && let Ok(dir) = std::env::var("OATHKEEP_EMIT_DUMP")
    {
        let name: String = label()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let _ = std::fs::write(
            format!("{dir}/{}.dxil", &name[name.len().saturating_sub(100)..]),
            &shader.dxil,
        );
    }
    if let Some((h, stub_ps)) = &checks.host {
        let made = match shader.stage {
            HwStage::Vs => h.pipeline(&shader, stub_ps).map(drop),
            HwStage::Cs => h.compute_pipeline(&shader).map(drop),
            HwStage::Ms => h.mesh_pipeline(&shader, stub_ps).map(drop),
            _ => host::stub_vs(&shader).and_then(|vs| h.pipeline(&vs, &shader).map(drop)),
        };
        match made {
            Ok(()) => {
                *counter(t, shader.stage, 3) += 1;
                if t.list {
                    t.lines.push(format!("{}  pipeline created", label()));
                }
            }
            Err(e) => t.fail(format!("pipeline: {e}"), &label),
        }
    }
    let Some(v) = checks.validator.as_mut() else {
        if t.list && checks.host.is_none() {
            t.lines.push(format!("{}  emitted", label()));
        }
        return;
    };
    match v.validate(&mut shader.dxil) {
        Ok(()) => {
            *counter(t, shader.stage, 2) += 1;
            if t.list {
                t.lines.push(format!("{}  validated", label()));
            }
        }
        Err(text) => {
            let first = text
                .lines()
                .find(|l| l.contains("error"))
                .unwrap_or(text.lines().next().unwrap_or(""))
                .to_owned();
            if let Ok(dir) = std::env::var("OATHKEEP_EMIT_DUMP") {
                let n = t
                    .failed
                    .get(&format!("validator: {first}"))
                    .map_or(0, |f| f.0);
                if n < 3 {
                    let name = format!(
                        "{dir}/{}_{n}",
                        first
                            .chars()
                            .filter(char::is_ascii_alphanumeric)
                            .take(40)
                            .collect::<String>()
                    );
                    let _ = std::fs::write(format!("{name}.dxil"), &shader.dxil);
                    let _ = std::fs::write(format!("{name}.txt"), format!("{}\n{text}", label()));
                }
            }
            t.fail(format!("validator: {first}"), label);
        }
    }
}
