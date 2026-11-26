//! `oathkeep launch`: zero-friction shader pre-translation, pipeline pre-warming,
//! and profile-tuned title launcher.
//!
//! Runs before entering the title so cold shader translation and pipeline compilation
//! stalls/missing-PSO artifacts are eliminated:
//! 1. Verifies runtime readiness (Agility SDK presence on Windows 10, clean-room cache dirs).
//! 2. Pre-translates title shaders into the host-keyed DXIL cache (`oathkeep shaders`),
//!    skipping instantly on repeat launches when the translator + GPU fingerprint matches.
//! 3. Pre-warms known D3D12 host pipelines (`oathkeep warm`) from `known-pipelines.bin`
//!    and reports `host-pipelines.bin` / `early-pipelines.txt` readiness.
//! 4. Enables `OATHKEEP_PIPELINE_BOOK=1` (recording any newly seen pipelines for future
//!    launches), applies the selected performance profile (`pareto` by default for 8 GB GPUs),
//!    and starts the title.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::ValueEnum;

use crate::{Console, Unimplemented};

/// Tuned runtime presets matching `scripts/play.ps1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    /// Verified 8 GB VRAM profile (levels-on-demand, async relay, hole reuse).
    Pareto,
    /// Same as `pareto` with an explicit 1920x1080 display mode hint.
    Nvidia8gb,
    /// Aggressive GPU scheduling preset.
    Fast,
    /// Minimal conservative preset for first-boot troubleshooting.
    Safe,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Profile::Pareto => "pareto",
            Profile::Nvidia8gb => "nvidia8gb",
            Profile::Fast => "fast",
            Profile::Safe => "safe",
        }
    }

    /// Environment key/value pairs for this preset.
    pub fn env_vars(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Profile::Safe => &[("OATHKEEP_SHADER_LOOP_LIMIT", "256")],
            Profile::Fast => &[
                ("OATHKEEP_SHADER_LOOP_LIMIT", "256"),
                ("OATHKEEP_LATE_WAIT", "3"),
                ("OATHKEEP_WIDE_BACK", "1"),
                ("OATHKEEP_WIDE_REFRESH", "1"),
                ("OATHKEEP_WIDE_MERGE", "1"),
                ("OATHKEEP_JOIN_COPIES", "1"),
                ("OATHKEEP_CARD_REHASH_RUNS", "1"),
                ("OATHKEEP_CARD_BUFFERS_MIN_MB", "1"),
                ("OATHKEEP_CARD_BUFFERS_MB", "1024"),
                ("OATHKEEP_TEXTURE_NO_WAIT", "1"),
                ("OATHKEEP_PAGE_WRITES", "1"),
                ("OATHKEEP_COMMITTED_QUERY", "1"),
                ("OATHKEEP_INDIRECT_WINDOW", "1"),
                ("OATHKEEP_INPLACE", "2"),
                ("OATHKEEP_INPLACE_MIN_MB", "1"),
                ("OATHKEEP_PIPELINE_WAIT_MS", "16"),
            ],
            Profile::Pareto => &[
                ("OATHKEEP_AMBIENT", "0.8"),
                ("OATHKEEP_AMBIENT_AIR", "1.5"),
                ("OATHKEEP_TEXTURE_NO_WAIT", "1"),
                ("OATHKEEP_RELAY", "2"),
                ("OATHKEEP_WIDE_BACK", "1"),
                ("OATHKEEP_CARD_REHASH_RUNS", "1"),
                ("OATHKEEP_JOIN_COPIES", "1"),
                ("OATHKEEP_INDIRECT_WINDOW", "1"),
                ("OATHKEEP_RELAY_COPY", "1"),
                ("OATHKEEP_CARD_STEPS", "2"),
                ("OATHKEEP_INDIRECT_BATCH", "2"),
                ("OATHKEEP_MAP_THREADS", "1"),
                ("OATHKEEP_CARD_AWAKE", "1"),
                ("OATHKEEP_SHOW_AFTER", "1"),
                ("OATHKEEP_THIN", "1"),
                ("OATHKEEP_JOIN_PARTS", "8"),
                ("OATHKEEP_UPLOAD_AHEAD", "1"),
                ("OATHKEEP_LEVELS_ON_DEMAND", "1"),
                ("OATHKEEP_CARD_BUFFERS_MIN_KB", "64"),
            ],
            Profile::Nvidia8gb => &[
                ("OATHKEEP_DISPLAY_MODE", "1920x1080"),
                ("OATHKEEP_AMBIENT", "0.8"),
                ("OATHKEEP_AMBIENT_AIR", "1.5"),
                ("OATHKEEP_TEXTURE_NO_WAIT", "1"),
                ("OATHKEEP_RELAY", "2"),
                ("OATHKEEP_WIDE_BACK", "1"),
                ("OATHKEEP_CARD_REHASH_RUNS", "1"),
                ("OATHKEEP_JOIN_COPIES", "1"),
                ("OATHKEEP_INDIRECT_WINDOW", "1"),
                ("OATHKEEP_RELAY_COPY", "1"),
                ("OATHKEEP_CARD_STEPS", "2"),
                ("OATHKEEP_INDIRECT_BATCH", "2"),
                ("OATHKEEP_MAP_THREADS", "1"),
                ("OATHKEEP_CARD_AWAKE", "1"),
                ("OATHKEEP_SHOW_AFTER", "1"),
                ("OATHKEEP_THIN", "1"),
                ("OATHKEEP_JOIN_PARTS", "8"),
                ("OATHKEEP_UPLOAD_AHEAD", "1"),
                ("OATHKEEP_LEVELS_ON_DEMAND", "1"),
                ("OATHKEEP_CARD_BUFFERS_MIN_KB", "64"),
            ],
        }
    }
}

pub struct LaunchOptions {
    pub exe: Option<PathBuf>,
    pub profile: Profile,
    pub console: Console,
    pub jobs: usize,
    pub skip_warm: bool,
    pub warm_all: bool,
    pub force_warm: bool,
    pub no_launch: bool,
    pub title_args: Vec<String>,
}

/// Resolves the target executable from `--exe` or, if omitted, the largest `.exe`
/// inside the configured `game_dir` (strictly read-only).
pub fn resolve_exe(explicit: Option<&Path>, game_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        anyhow::ensure!(
            path.is_file(),
            "target executable not found: {}",
            path.display()
        );
        return Ok(path.to_path_buf());
    }
    let dir = game_dir.context(
        "no executable given and no game_dir configured (pass an exe path or set OATHKEEP_GAME_DIR / oathkeep.toml)",
    )?;
    find_main_exe(dir)
}

/// Picks the largest `.exe` directly inside `dir` (read-only scan).
fn find_main_exe(dir: &Path) -> Result<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_exe = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
        if !is_exe {
            continue;
        }
        let len = entry.metadata().map_or(0, |m| m.len());
        if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
            best = Some((len, path));
        }
    }
    best.map(|(_, path)| path)
        .with_context(|| format!("no .exe found in {}", dir.display()))
}

/// Stamp file recording that shader pre-translation completed for `fingerprint`.
fn shader_stamp_path(shaders_dir: &Path, fingerprint: &str) -> PathBuf {
    shaders_dir.join(format!("precompiled-{fingerprint}.ok"))
}

/// Applies preset defaults without overwriting variables the user explicitly set.
fn apply_env_defaults(vars: &[(&str, &str)]) {
    for &(k, v) in vars {
        if std::env::var_os(k).is_none() {
            // SAFETY: invoked on the single-threaded CLI setup path before spawning title threads.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

#[cfg(windows)]
fn warn_if_agility_missing() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let core = dir.join("D3D12").join("D3D12Core.dll");
    if !core.is_file() {
        println!(
            "  note: {} not found (fine on Windows 11; on Windows 10 run scripts\\setup-agility-sdk.ps1)",
            core.display()
        );
    }
}

#[cfg(windows)]
pub fn run(opts: LaunchOptions) -> Result<()> {
    let cfg = oathkeep_core::Config::load()?;
    let exe = resolve_exe(opts.exe.as_deref(), cfg.game_dir.as_deref())?;
    let title_dir = exe
        .parent()
        .context("target executable has no parent directory")?
        .to_path_buf();
    let jobs = opts.jobs.max(1);

    // Ensure the shader & pipeline caches stay strictly outside the repository.
    let shaders_dir = cfg.cache_subdir("shaders")?;
    let pipelines_dir = cfg.cache_subdir("pipelines")?;

    println!("=== Oathkeep Launcher (Shader Warming & Preload) ===");
    println!("  profile   : {}", opts.profile.name());
    println!(
        "  console   : {}",
        match opts.console {
            Console::SeriesX => "series-x",
            Console::SeriesS => "series-s",
        }
    );
    println!("  cache_dir : {}", cfg.cache_dir.display());
    println!("  data_dir  : {}", cfg.data_dir.display());
    println!("  workers   : {jobs}");
    warn_if_agility_missing();

    // Apply profile defaults before shader warming so host wave/view settings match the run.
    apply_env_defaults(opts.profile.env_vars());
    apply_env_defaults(&[
        ("OATHKEEP_PIPELINE_BOOK", "1"),
        ("OATHKEEP_PIPELINE_WAIT_MS", "16"),
    ]);

    if !opts.skip_warm {
        let warm_start = Instant::now();
        oathkeep_hle::known_pipelines::use_translations(shaders_dir.clone());
        let fingerprint = oathkeep_hle::known_pipelines::fingerprint();

        // Step 1: Pre-translate console shader containers into host DXIL cache.
        let stamp = shader_stamp_path(&shaders_dir, &fingerprint);
        if !opts.force_warm && stamp.is_file() {
            println!("[1/3] Shader translation cache warm (fingerprint {fingerprint}) — skipping scan.");
        } else {
            println!("[1/3] Pre-translating title shaders into host DXIL cache ({jobs} threads)...");
            crate::translate::precompile(vec![title_dir.clone()], Some(jobs))?;
            let _ = std::fs::write(&stamp, format!("{fingerprint}\n"));
        }

        // Step 2: Pre-warm known D3D12 pipelines into the GPU driver cache.
        let known_count = oathkeep_hle::known_pipelines::count(&pipelines_dir);
        let early_count = std::fs::read_to_string(pipelines_dir.join("early-pipelines.txt"))
            .map(|s| s.lines().filter(|l| !l.is_empty()).count())
            .unwrap_or(0);
        let lib_mb = std::fs::metadata(pipelines_dir.join("host-pipelines.bin"))
            .map_or(0.0, |m| m.len() as f64 / (1024.0 * 1024.0));

        if known_count > 0 {
            let mode = if opts.warm_all { "all known" } else { "used/early" };
            println!(
                "[2/3] Warming {mode} D3D12 pipelines ({known_count} in book, {early_count} early, library {lib_mb:.1} MB, {jobs} processes)..."
            );
            crate::warm::run(Some(jobs), None, opts.force_warm, !opts.warm_all, None)?;
        } else {
            println!(
                "[2/3] Pipeline book empty (library {lib_mb:.1} MB, {early_count} early); recording enabled (OATHKEEP_PIPELINE_BOOK=1) for future warm-ups."
            );
        }

        println!(
            "[3/3] Preload ready in {:.1} s.",
            warm_start.elapsed().as_secs_f64()
        );
    } else {
        println!("[1/3..3/3] Warming skipped (--skip-warm).");
    }

    if opts.no_launch {
        println!("Preload complete (--no-launch set; not starting title).");
        return Ok(());
    }

    println!("Launching title...");
    oathkeep_hle::runtime::set_title_args(opts.title_args);
    crate::run::run(&exe, Unimplemented::Trap, false, true, opts.console)
}

#[cfg(not(windows))]
pub fn run(_: LaunchOptions) -> Result<()> {
    anyhow::bail!("launching titles requires Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pareto_and_nvidia8gb_profiles_set_core_flags() {
        for profile in [Profile::Pareto, Profile::Nvidia8gb] {
            let vars = profile.env_vars();
            assert!(vars.contains(&("OATHKEEP_LEVELS_ON_DEMAND", "1")));
            assert!(vars.contains(&("OATHKEEP_RELAY", "2")));
            assert!(vars.contains(&("OATHKEEP_TEXTURE_NO_WAIT", "1")));
            assert!(vars.contains(&("OATHKEEP_THIN", "1")));
        }
        assert!(
            Profile::Nvidia8gb
                .env_vars()
                .contains(&("OATHKEEP_DISPLAY_MODE", "1920x1080"))
        );
    }

    #[test]
    fn find_main_exe_picks_largest_executable() {
        let dir = std::env::temp_dir().join(format!("oathkeep-launch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("small.exe"), vec![0u8; 16]).unwrap();
        std::fs::write(dir.join("main.exe"), vec![0u8; 128]).unwrap();
        std::fs::write(dir.join("data.bin"), vec![0u8; 512]).unwrap();

        let found = resolve_exe(None, Some(&dir)).unwrap();
        assert_eq!(found.file_name().unwrap(), "main.exe");

        let explicit = resolve_exe(Some(&dir.join("small.exe")), Some(&dir)).unwrap();
        assert_eq!(explicit.file_name().unwrap(), "small.exe");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_path_uses_fingerprint_only() {
        let dir = Path::new("C:\\cache\\shaders");
        let stamp = shader_stamp_path(dir, "0123456789abcdef");
        assert_eq!(stamp.file_name().unwrap(), "precompiled-0123456789abcdef.ok");
    }
}
