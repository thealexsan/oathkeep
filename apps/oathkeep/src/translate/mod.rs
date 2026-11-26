//! The offline batch: every shader found goes through the translator, and
//! what it cannot handle yet is tallied (decision 0003).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use oathkeep_shader::emit::host::{self, Host};
use oathkeep_shader::emit::{self, validator::Validator};
use oathkeep_shader::structure;
use oathkeep_shader::{
    Container, FourCC, HwStage, NativeProgram, ShaderError, native_programs,
};
use oathkeep_shader::{io, resource};

use crate::shader::{MAX_FILE, Source, sources};


mod tally;
mod validate;
mod warm;

pub use warm::{precompile, warm};

use tally::{Tally, counter, tally_io, tally_resources, tally_structured};
use validate::{Checks, census_line, check};

pub fn run(
    paths: Vec<PathBuf>,
    validate: bool,
    pipelines: bool,
    list: bool,
    jobs: Option<usize>,
) -> Result<()> {
    // Dumped shaders and validator reports are game-derived (rule #1).
    if let Some(dir) = std::env::var_os("OATHKEEP_EMIT_DUMP") {
        oathkeep_core::ensure_outside_repo(Path::new(&dir))?;
    }
    let paths = crate::default_paths(paths)?;
    // `OATHKEEP_HOST_MAX_WAVE=32`: translate as for a host whose waves have
    // at most 32 lanes (as the runtime does on such a card).
    if let Some(n) = std::env::var("OATHKEEP_HOST_MAX_WAVE").ok().and_then(|v| v.parse().ok()) {
        oathkeep_shader::emit::set_host_max_wave(n);
    }
    // `OATHKEEP_HOST_RDNA=1`: as for an AMD RDNA card (`OATHKEEP_RDNA_FLOAT`).
    oathkeep_shader::emit::set_host_rdna(std::env::var_os("OATHKEEP_HOST_RDNA").is_some_and(|v| v == "1"));
    let dll = validate.then(Validator::default_path).flatten();
    if validate {
        Validator::load(dll.as_deref()).map_err(anyhow::Error::msg)?;
        println!(
            "validator    {}",
            dll.as_ref()
                .map_or("dxil.dll".into(), |p| p.display().to_string())
        );
    }
    if pipelines {
        Host::new(false).map_err(anyhow::Error::msg)?;
    }
    let pairs: Vec<(String, String)> = std::env::var("OATHKEEP_PAIRS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| p.split_once('+'))
        .map(|(v, p)| (v.trim().to_owned(), p.trim().to_owned()))
        .collect();
    let kept = std::sync::Arc::new(Mutex::new(Vec::new()));
    let threads =
        jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let seen = Mutex::new(HashSet::new());
    let total = Mutex::new(Tally::default());
    for root in &paths {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() || entry.metadata().map_or(0, |m| m.len()) > MAX_FILE {
                continue;
            }
            let Ok(data) = std::fs::read(entry.path()) else {
                continue;
            };
            let jobs = sources(&data);
            let next = AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..threads.min(jobs.len()) {
                    s.spawn(|| {
                        let mut t = Tally {
                            list,
                            ..Tally::default()
                        };
                        let mut checks = Checks {
                            keep: (!pairs.is_empty()).then(|| kept.clone()),
                            validator: validate
                                .then(|| Validator::load(dll.as_deref()).ok())
                                .flatten(),
                            host: pipelines
                                .then(|| Host::new(false).ok().map(|h| (h, host::stub_ps())))
                                .flatten(),
                        };
                        while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                            translate(job, entry.path(), &seen, &mut t, &mut checks);
                        }
                        lock(&total).merge(t);
                    });
                }
            });
        }
    }
    let t = total.into_inner().unwrap_or_else(|e| e.into_inner());
    println!(
        "containers   {} ({} duplicates skipped)",
        t.containers, t.duplicates
    );
    println!(
        "programs     {} translated of {} ({} IR instructions)",
        t.translated, t.programs, t.instructions
    );
    println!(
        "structured   {} of {} ({} vector instructions, {} guarded, {} on opaque masks; \
         branches {} per lane, {} uniform)",
        t.structured,
        t.translated,
        t.vector,
        t.guarded,
        t.opaque,
        t.lane_branches,
        t.uniform_branches
    );
    println!(
        "resources    {} of {} tracked (descriptors {} loaded, {} built, {} constant; \
         {} scalar accesses; {} with a computed term)",
        t.tracked, t.translated, t.loaded, t.built, t.constant, t.scalar, t.dynamic
    );
    println!(
        "  used-resource entries {}: {} not traced {:?}; {} traced parameters not listed",
        t.used,
        t.untraced_used.values().sum::<usize>(),
        t.untraced_used,
        t.unlisted
    );
    println!(
        "I/O          {} of {} vertex and pixel programs raised ({} interpolations, \
         {} vertex values; exports {:?}, {} still f16 pairs)",
        t.io_raised, t.io_programs, t.interpolated, t.vertex_values, t.exports, t.compressed
    );
    println!(
        "DXIL         {} emitted of {} vertex and pixel programs; {} validated, {} pipelines",
        t.emitted, t.emit_programs, t.validated, t.pipelines
    );
    println!(
        "             {} emitted of {} compute programs; {} validated, {} pipelines",
        t.cs_emitted, t.cs_programs, t.cs_validated, t.cs_pipelines
    );
    println!(
        "             {} emitted of {} mesh programs; {} validated, {} pipelines",
        t.ms_emitted, t.ms_programs, t.ms_validated, t.ms_pipelines
    );
    if !pairs.is_empty() {
        let host = Host::new(false).map_err(anyhow::Error::msg)?;
        let kept = lock(&kept);
        let find = |name: &str| kept.iter().find(|(l, _)| l.contains(name));
        for (v, p) in &pairs {
            let made = match (find(v), find(p)) {
                (Some((_, vs)), Some((_, ps))) => if vs.stage == HwStage::Ms {
                    host.mesh_pipeline(vs, ps)
                } else {
                    host.pipeline(vs, ps)
                }
                .map(|_| "pipeline created".to_owned())
                .unwrap_or_else(|e| e),
                _ => "not emitted".to_owned(),
            };
            println!("pair {v} + {p}: {made}");
        }
    }
    let mut lines = t.lines;
    lines.sort();
    for l in lines {
        println!("{l}");
    }
    let mut failed: Vec<_> = t.failed.into_iter().collect();
    failed.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    for (reason, (n, example)) in failed {
        println!("  {n:>7}  {reason}  (e.g. {example})");
    }
    Ok(())
}

fn translate(
    source: &Source,
    path: &Path,
    seen: &Mutex<HashSet<blake3::Hash>>,
    t: &mut Tally,
    checks: &mut Checks,
) {
    let example = || format!("{}{}", path.display(), source.label());
    let Some(bytes) = source.bytes() else {
        return t.fail("corrupt record".into(), example);
    };
    // The hash a run's log names the shader by (`hash=` in "first bind",
    // "shader does not translate", "host pipeline refused").
    let hash = blake3::hash(&bytes).to_hex()[..16].to_owned();
    // Diagnostics: `OATHKEEP_TRANSLATE_ONLY=<hash prefix>,...` translates
    // those containers alone.
    static ONLY: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let only = ONLY.get_or_init(|| {
        std::env::var("OATHKEEP_TRANSLATE_ONLY")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty())
            .collect()
    });
    if !only.is_empty() && !only.iter().any(|o| hash.starts_with(o.as_str())) {
        return;
    }
    let example = || format!("{}{} #{hash}", path.display(), source.label());
    t.containers += 1;
    let Ok(container) = Container::parse(&bytes) else {
        return t.fail("no DXIL part (SM5 or older)".into(), example);
    };
    let Some(dxil) = container.part(FourCC::DXIL).or(container.part(FourCC::ACCC)).map(|p| p.data) else {
        return t.fail("no DXIL part (SM5 or older)".into(), example);
    };
    // `~`: the hash of the code alone; containers that differ only around
    // it are translated once, the others listed as duplicates of it.
    let code = blake3::hash(dxil);
    let code8 = code.to_hex()[..8].to_owned();
    let example = || format!("{} ~{code8}", example());
    // `OATHKEEP_DUMP_MATCH=<dir>;<text>|<text>...`: the containers whose
    // line holds one of the texts (a name, `#hash`), as the title has them.
    if let Ok(spec) = std::env::var("OATHKEEP_DUMP_MATCH")
        && let Some((dir, texts)) = spec.split_once(';')
    {
        let line = example();
        if texts.split('|').any(|text| !text.is_empty() && line.contains(text)) {
            let name: String = source
                .label()
                .chars()
                .skip_while(|c| *c != ' ')
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(70)
                .collect();
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(std::path::Path::new(dir).join(format!("{name}.{hash}.cdxil")), &bytes[..]);
        }
    }
    if !lock(seen).insert(code) {
        t.duplicates += 1;
        if t.list {
            t.lines.push(format!("{}  duplicate", example()));
        }
        return;
    }
    let programs = match native_programs(dxil) {
        Ok(p) => p,
        Err(e) => return t.fail(e.to_string(), example),
    };
    // `OATHKEEP_FAIL_DUMP=<dir>`: the first 3 containers of each failure reason, as the title
    // has them (`.cdxil`), to work on a translator gap offline
    let dump = std::env::var_os("OATHKEEP_FAIL_DUMP");
    let before: Option<BTreeMap<String, usize>> = dump.as_ref().map(|_| t.failed.iter().map(|(k, v)| (k.clone(), v.0)).collect());
    translate_programs(&container, programs, &example, t, checks);
    if let (Some(dir), Some(before)) = (dump, before) {
        for (reason, (n, _)) in &t.failed {
            let was = before.get(reason).copied().unwrap_or(0);
            if *n > was && was < 3 {
                let stem: String = reason.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).take(60).collect();
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{stem}_{was}.cdxil")), &bytes[..]);
            }
        }
    }
}

fn translate_programs(container: &Container, programs: Vec<NativeProgram>, example: &dyn Fn() -> String, t: &mut Tally, checks: &mut Checks) {
    for p in programs {
        t.programs += 1;
        match oathkeep_shader::spill::build(&p) {
            Ok(program) => {
                t.translated += 1;
                t.instructions += program.blocks.iter().map(|b| b.insts.len()).sum::<usize>();
                // Per-lane descriptors are traced on the per-lane form; its
                // failure is listed after the resources' and I/O's.
                let structured = structure::structure(&program);
                let lanes = structured.as_ref().ok();
                let res = match resource::track(&program, &p.layout, p.stage, lanes) {
                    Ok(r) => {
                        tally_resources(&p, &r, t);
                        Some(r)
                    }
                    Err(ShaderError::Unsupported { offset, what }) => {
                        t.fail(format!("resources: {what}"), || {
                            format!("{} {:?} at {offset:#x}", example(), p.stage)
                        });
                        None
                    }
                    Err(e) => {
                        t.fail(e.to_string(), || format!("{} {:?}", example(), p.stage));
                        None
                    }
                };
                let graphics = matches!(p.stage, HwStage::Vs | HwStage::Ps);
                let compute = p.stage == HwStage::Cs;
                let mut raised = None;
                if graphics || compute || p.stage == HwStage::Ms {
                    t.io_programs += usize::from(graphics);
                    match io::raise(&program, &p) {
                        Ok(r) => {
                            if graphics {
                                tally_io(&r, t);
                            }
                            raised = Some(r);
                        }
                        Err(ShaderError::Unsupported { offset, what }) => t
                            .fail(format!("I/O: {what}"), || {
                                format!("{} {:?} at {offset:#x}", example(), p.stage)
                            }),
                        Err(e) => t.fail(e.to_string(), || format!("{} {:?}", example(), p.stage)),
                    }
                }
                let structured = match structured {
                    Ok(s) => {
                        tally_structured(&program, &s, t);
                        Some(s)
                    }
                    Err(e) => {
                        let reason = match e {
                            ShaderError::Unsupported { what, .. } => format!("structure: {what}"),
                            ShaderError::Isa { reason, .. } => format!("structure: {reason}"),
                            e => e.to_string(),
                        };
                        t.fail(reason, || format!("{} {:?}", example(), p.stage));
                        None
                    }
                };
                if let (Some(r), Some(i), Some(s)) = (res, raised, structured) {
                    let label = || format!("{} {:?}", example(), p.stage);
                    *counter(t, p.stage, 0) += 1;
                    match emit::emit(container, &p, &program, &s, &i, &r) {
                        Ok(shader) => {
                            census_line(&label(), &p, &program, &shader);
                            *counter(t, p.stage, 1) += 1;
                            check(shader, t, checks, label);
                        }
                        Err(ShaderError::Unsupported { offset, what }) => t
                            .fail(format!("emit: {what}"), || {
                                format!("{} at {offset:#x}", label())
                            }),
                        Err(e) => t.fail(format!("emit: {e}"), label),
                    }
                }
            }
            Err(e) => {
                let reason = match e {
                    ShaderError::Unsupported { what, .. } => format!("unsupported: {what}"),
                    e => e.to_string(),
                };
                t.fail(reason, || format!("{} {:?}", example(), p.stage));
            }
        }
    }
}

pub(super) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    /// Shader dumps are game-derived, so a dump directory in the repo is refused.
    #[test]
    fn emit_dump_inside_repo_is_refused() {
        // SAFETY: no other test in this binary touches the environment.
        unsafe { std::env::set_var("OATHKEEP_EMIT_DUMP", env!("CARGO_MANIFEST_DIR")) };
        let missing = std::env::temp_dir().join("victory-translate-no-such-dir");
        let err = super::run(vec![missing], false, false, false, Some(1)).unwrap_err();
        // SAFETY: as above; unset so a later test's `run` is not refused.
        unsafe { std::env::remove_var("OATHKEEP_EMIT_DUMP") };
        assert!(
            matches!(
                err.downcast_ref(),
                Some(oathkeep_core::PolicyError::InsideRepo { .. })
            ),
            "{err}"
        );
    }
}
