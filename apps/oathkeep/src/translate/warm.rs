use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use crate::shader::{MAX_FILE, sources};
use super::lock;

/// `translate --warm <file>`: the containers whose hash the file names are
/// translated into the run's shader cache, on `jobs` threads; nothing is
/// made on the GPU. A run of the title then finds them there instead of
/// translating each at its first use.
pub fn warm(paths: Vec<PathBuf>, file: &Path, jobs: Option<usize>) -> Result<()> {
    // `--warm all`: every container found, whatever its hash.
    let all = file.as_os_str() == "all";
    let mut wanted: HashSet<String> = HashSet::new();
    if !all {
        let text = std::fs::read_to_string(file)?;
        for line in text.lines() {
            for word in line.split(|c: char| !c.is_ascii_hexdigit()) {
                if word.len() == 16 {
                    wanted.insert(word.to_ascii_lowercase());
                }
            }
        }
        anyhow::ensure!(!wanted.is_empty(), "no 16-digit shader hash in {}", file.display());
    }
    // `OATHKEEP_WARM_SECONDS=<n>`: stop taking new shaders after n seconds
    // (what is done stays in the cache; the next warm-up goes on from there).
    let limit = std::env::var("OATHKEEP_WARM_SECONDS").ok().and_then(|v| v.trim().parse::<u64>().ok());
    let cfg = oathkeep_core::Config::load()?;
    oathkeep_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let paths = crate::default_paths(paths)?;
    let threads = jobs.unwrap_or(4).max(1);
    let began = std::time::Instant::now();
    let done = Mutex::new(HashSet::new());
    let counts = [const { AtomicUsize::new(0) }; 5];
    for root in &paths {
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
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
                        while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                            let Some(bytes) = job.bytes() else {
                                continue;
                            };
                            let hash = blake3::hash(&bytes).to_hex()[..16].to_owned();
                            if limit.is_some_and(|n| began.elapsed().as_secs() >= n) {
                                counts[3].fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                            if (!all && !wanted.contains(&hash)) || !lock(&done).insert(hash) {
                                continue;
                            }
                            let n = counts[4].fetch_add(1, Ordering::Relaxed) + 1;
                            if n.is_multiple_of(1000) {
                                println!("warm-up      {n} shaders so far, {:.0} s", began.elapsed().as_secs_f64());
                            }
                            let which = match oathkeep_hle::modules::d3d12x::warm_shader(&bytes) {
                                Ok(true) => 0,
                                Ok(false) => 1,
                                Err(_) => 2,
                            };
                            counts[which].fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
            });
        }
    }
    let [translated, there, failed] = [0, 1, 2].map(|i| counts[i].load(Ordering::Relaxed));
    let asked = if all { "all".to_owned() } else { wanted.len().to_string() };
    let stopped = if counts[3].load(Ordering::Relaxed) > 0 { " (stopped at the time limit)" } else { "" };
    println!(
        "warm-up      {asked} hashes asked, {} found: {translated} translated now, {there} in the cache already, {failed} do not translate; {:.1} s on {threads} threads{stopped}",
        translated + there + failed,
        began.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Every shader found into the cache runs translate from.
pub fn precompile(paths: Vec<PathBuf>, jobs: Option<usize>) -> Result<()> {
    let cfg = oathkeep_core::Config::load()?;
    oathkeep_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let paths = crate::default_paths(paths)?;
    let threads =
        jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let started = std::time::Instant::now();
    let seen = Mutex::new(HashSet::new());
    let done = AtomicUsize::new(0);
    let failed = Mutex::new(BTreeMap::<String, (usize, String)>::new());
    for root in &paths {
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
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
                        while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                            let Some(bytes) = job.bytes() else { continue };
                            let hash = blake3::hash(&bytes);
                            if !lock(&seen).insert(hash) {
                                continue;
                            }
                            match oathkeep_hle::modules::d3d12x::precompile(&bytes) {
                                Ok(_) => {
                                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                                    if n.is_multiple_of(1000) {
                                        println!("{n} shaders in the cache, {:.0} s", started.elapsed().as_secs_f64());
                                    }
                                }
                                Err(why) => {
                                    let mut failed = lock(&failed);
                                    failed.entry(why).or_insert_with(|| (0, hash.to_hex()[..16].to_owned())).0 += 1;
                                }
                            }
                        }
                    });
                }
            });
        }
    }
    let failed = failed.into_inner().unwrap_or_else(|e| e.into_inner());
    println!(
        "{} shaders in the cache, {} do not translate, {:.0} s",
        done.load(Ordering::Relaxed),
        failed.values().map(|f| f.0).sum::<usize>(),
        started.elapsed().as_secs_f64()
    );
    for (why, (n, hash)) in failed {
        println!("  {n:>5}  {why}  (e.g. {hash})");
    }
    Ok(())
}
