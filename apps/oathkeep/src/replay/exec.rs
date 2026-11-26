use anyhow::{Context, Result};
use oathkeep_hle::replay::play::{self, Options};

use super::{Buffers, Replay, Timing, pair, range};

pub(crate) fn run(r: Replay) -> Result<()> {
    if let Some(text) = &r.dump {
        for line in play::dump(&r.folder, text)? {
            println!("{line}");
        }
        return Ok(());
    }
    if let Some(text) = &r.listing {
        let lists = range(text).with_context(|| format!("--listing {text}"))?;
        return play::listing(&r.folder, lists, &mut |line| println!("{line}"));
    }
    if let Some(text) = &r.flow {
        let lists = range(text).with_context(|| format!("--flow {text}"))?;
        return play::look::flow(&r.folder, lists, r.flow_draws, r.flow_slots, &mut |line| println!("{line}"));
    }
    if let Some(into) = &r.retranslate {
        return retranslate(&r.folder, into, r.view_limit, r.raw_views);
    }
    let until = match &r.until {
        Some(text) => Some(
            text.split_once(':')
                .and_then(|(l, c)| Some((l.parse().ok()?, c.parse().ok()?)))
                .with_context(|| format!("--until {text}: not <list>:<command>"))?,
        ),
        None => None,
    };
    let only = match &r.only {
        Some(text) => Some(
            range(text)
                .with_context(|| format!("--only {text}: not a list or a range of lists (3..7)"))?,
        ),
        None => None,
    };
    let options = Options {
        loops: r.loops.max(1),
        restore: !r.no_restore,
        detailed: matches!(r.timing, Timing::Detailed),
        top: r.top,
        image: r.image,
        buffers: match r.buffers {
            Buffers::AsRecorded => play::Buffers::AsRecorded,
            Buffers::Vram => play::Buffers::Card,
            Buffers::Sysmem => play::Buffers::Pc,
        },
        only_buffers: r.only_buffers,
        no_barriers: r.no_barriers,
        merge_barriers: r.merge_barriers,
        skip_clamps: r.skip_clamps,
        batch_clamps: r.batch_clamps,
        skip_hash_passes: r.skip_hash_passes,
        one_submit: r.one_submit,
        merge: !r.no_merge,
        wait_all: r.wait_all,
        wait_lists: r.wait_lists,
        pace: r.pace,
        awake: r.awake,
        only,
        until,
        skip_shaders: r.skip_shader,
        shaders: r.shaders,
        shaders_b: r.shaders_b,
        compare: r.compare,
        debug: r.debug_layer,
        breadcrumbs: r.breadcrumbs,
    };
    if let Some(text) = &r.slots {
        let (list, command) = text
            .split_once(':')
            .and_then(|(l, c)| Some((l.parse().ok()?, c.parse().ok()?)))
            .with_context(|| format!("--slots {text}: not <list>:<command>"))?;
        let player = play::Player::load(&r.folder, &options)?;
        return player.slots(list, command, &mut |line| println!("{line}"));
    }
    if let Some(folder) = &r.images {
        let mut which = Vec::new();
        for text in &r.texture {
            let (id, sub) = text.split_once('.').unwrap_or((text, "0"));
            let bad = || format!("--texture {text}");
            which.push((id.parse().with_context(bad)?, sub.parse().with_context(bad)?));
        }
        let images = play::look::Images {
            which,
            least: r.least,
            folder: folder.clone(),
            width: r.width,
            raw: r.raw,
        };
        let mut player = play::Player::load(&r.folder, &options)?;
        return player.images(&options, &images, &mut |line| println!("{line}"));
    }
    if !r.pixel.is_empty() {
        let mut player = play::Player::load(&r.folder, &options)?;
        let mut pixels = play::pixel::Pixels {
            depth: r.depth,
            written: !r.targets_only,
            ..Default::default()
        };
        if let Some(text) = &r.lists {
            pixels.lists = Some(range(text).with_context(|| format!("--lists {text}"))?);
        }
        let of = match &r.of {
            Some(text) => Some(pair(text, 'x').with_context(|| format!("--of {text}"))?),
            None => None,
        };
        for text in &r.pixel {
            let bad = || format!("--pixel {text}");
            match text.split_once(':') {
                Some((what, at)) => {
                    let (id, sub) = what.split_once('.').unwrap_or((what, "0"));
                    let (x, y) = pair(at, ',').with_context(bad)?;
                    let id = id.parse().with_context(bad)?;
                    let sub = sub.parse().with_context(bad)?;
                    pixels.texels.push((id, sub, x, y));
                }
                None => {
                    let (mut x, mut y) = pair(text, ',').with_context(bad)?;
                    if let (Some((w, h)), Some((pw, ph))) = (of, player.picture_size()) {
                        x = x * pw / w.max(1);
                        y = y * ph / h.max(1);
                    }
                    pixels.points.push((x, y));
                }
            }
        }
        for text in &r.split {
            pixels
                .split
                .push(pair(text, ':').with_context(|| format!("--split {text}"))?);
        }
        return player.pixel(&options, &pixels, &mut |line| println!("{line}"));
    }
    if r.check_written {
        let mut player = play::Player::load(&r.folder, &options)?;
        return player.check_written(&mut |line| println!("{line}"));
    }
    if r.upcopies {
        let mut player = play::Player::load(&r.folder, &options)?;
        return player.upcopies(&options, &mut |line| println!("{line}"));
    }
    let mut watched = Vec::new();
    for text in &r.watch {
        let mut n = text
            .split(':')
            .map(|p| u64::from_str_radix(p.trim_start_matches("0x"), 16));
        let mut next = || {
            n.next()
                .and_then(Result::ok)
                .with_context(|| format!("--watch {text}"))
        };
        let id = text
            .split(':')
            .next()
            .and_then(|p| p.parse().ok())
            .with_context(|| format!("--watch {text}"))?;
        let _ = next();
        watched.push((id, next()?, next()?));
    }
    if let Some(runs) = r
        .diverge
        .or(r.diverge_picture)
        .or((!watched.is_empty()).then_some(2))
    {
        let lists = match &r.lists {
            Some(text) => Some(range(text).with_context(|| format!("--lists {text}"))?),
            None => None,
        };
        let search = play::diverge::Search {
            runs: runs.max(2),
            lists,
            roots: r.roots,
            wait_all: options.wait_all,
            options: options.clone(),
        };
        let mut player = play::Player::load(&r.folder, &options)?;
        if !watched.is_empty() {
            return player.watch(&search, &watched, &mut |line| println!("{line}"));
        }
        if let Some(text) = &r.draws {
            return player.diverge_draws(&search, text, &mut |line| println!("{line}"));
        }
        if r.diverge_picture.is_some() {
            let commands = match &r.commands {
                Some(text) => Some(range(text).with_context(|| format!("--commands {text}"))?),
                None => None,
            };
            return player
                .diverge_picture(&search, commands, r.step, &mut |line| println!("{line}"));
        }
        return player.diverge(&search, &mut |line| println!("{line}"));
    }
    let reports = play::replay(&r.folder, &options)?;
    for (what, report) in &reports {
        println!("{}", report.text(what));
    }
    if let [(_, before), (_, after)] = &reports[..] {
        println!("{:+.2} ms", after.median_ms() - before.median_ms());
    }
    Ok(())
}

/// Translates the capture's shaders into `into`.
fn retranslate(folder: &std::path::Path, into: &std::path::Path, view_limit: u32, raw_views: u32) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let wanted = play::title_shaders(folder)?;
    let kept = play::find(folder)?.with_file_name("containers");
    std::fs::create_dir_all(&kept)?;
    std::fs::create_dir_all(into)?;
    let mut missing: std::collections::HashSet<&str> = wanted
        .iter()
        .map(|w| w.1.as_str())
        .filter(|h| !kept.join(format!("{h}.cdxil")).exists())
        .collect();
    if !missing.is_empty() {
        println!("{} of {} containers are looked for in the title's files", missing.len(), wanted.len());
        for root in crate::default_paths(Vec::new())? {
            for entry in walkdir::WalkDir::new(root).into_iter().filter_map(std::result::Result::ok) {
                let size = entry.metadata().map_or(0, |m| m.len());
                if missing.is_empty() || !entry.file_type().is_file() || size > crate::shader::MAX_FILE {
                    continue;
                }
                let Ok(data) = std::fs::read(entry.path()) else { continue };
                for source in crate::shader::sources(&data) {
                    let Some(bytes) = source.bytes() else { continue };
                    let hash = blake3::hash(&bytes).to_hex()[..16].to_owned();
                    if missing.remove(hash.as_str()) {
                        std::fs::write(kept.join(format!("{hash}.cdxil")), &bytes[..])?;
                    }
                }
            }
        }
    }
    println!("{} containers not found", missing.len());
    oathkeep_shader::emit::set_host_max_wave(64);
    oathkeep_shader::emit::set_host_rdna(true);
    oathkeep_shader::emit::set_view_heap_limit(view_limit);
    oathkeep_shader::emit::set_raw_views(raw_views);
    let all: Vec<_> = wanted.iter().collect();
    let (next, done) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let started = std::time::Instant::now();
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().map_or(4, |n| n.get()) {
            s.spawn(|| {
                while let Some((stage, hash)) = all.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let Ok(bytes) = std::fs::read(kept.join(format!("{hash}.cdxil"))) else { continue };
                    match oathkeep_shader::emit::translate(&bytes) {
                        Ok(t) => {
                            if std::fs::write(into.join(format!("{hash}.dxil")), &t.dxil).is_ok() {
                                done.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => println!("{stage} {hash}: {e}"),
                    }
                }
            });
        }
    });
    println!("{} of {} shaders translated, {:.1} s", done.into_inner(), all.len(), started.elapsed().as_secs_f64());
    Ok(())
}
