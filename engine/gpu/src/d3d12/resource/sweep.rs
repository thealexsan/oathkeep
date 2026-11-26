use super::budget::targets_room_kept;
use super::format::footprint;
use super::hosted::{KINDS, REPLACED, fills, textures};
use super::placed::placed_textures_map;
use super::{BUFFER, Resource, resource};
use crate::d3d12::object::This;

/// For the card accounting line: the title texture made for a view of memory at `address`.
pub(crate) fn placed_known(address: u64, desc: &[u32; 14]) -> Option<This> {
    let mut key = *desc;
    key[1] = 0;
    key[13] = 0;
    let map = placed_textures_map();
    map.get(&(address, key)).or_else(|| map.get(&(address, *desc))).map(|&r| r as This)
}

/// For the card accounting line: whether title texture `this` has a host copy now.
pub(crate) fn copy_state(this: This) -> Option<(u8, u64, u64)> {
    use std::sync::atomic::Ordering::Relaxed;
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    if r.desc.dimension == BUFFER {
        return None;
    }
    let memory = r.data() as u64;
    let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
    Some(match made.as_ref() {
        Some((host, _)) => (2, host.vram_bytes(), memory),
        None => (u8::from(r.host.gone.load(Relaxed)), footprint(&r.desc).map_or(0, |f| f.size), memory),
    })
}

/// Lets go of the host copies of textures the title fills that nothing names any more.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    {
        let mut replaced = REPLACED.lock().unwrap_or_else(|e| e.into_inner());
        let (gone, keep): (Vec<_>, Vec<_>) =
            replaced.drain(..).partition(|(at, _)| crate::backend::long_ago(*at, 1000, IDLE_FRAMES));
        *replaced = keep;
        drop(replaced);
        for (_, made) in gone {
            crate::backend::retire(made);
        }
    }
    let memory = crate::backend::host().and_then(|h| h.video_memory());
    static KEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let keep = *KEEP
        .get_or_init(|| !matches!(std::env::var("OATHKEEP_TEXTURE_KEEP").as_deref(), Ok("0") | Ok("1")));
    let (idle_ms, most) = match memory {
        Some([budget, used, ..]) if keep => {
            let pool_free = crate::memory::residency::pool_free_credit(
                budget,
                used,
                crate::memory::pool::texture_room().saturating_add(crate::memory::tiles::free_room()),
            );
            let used = used.saturating_sub(pool_free)
                + crate::memory::shadow::card_room_kept()
                + targets_room_kept(Some(budget));
            crate::backend::vram_keep(budget, used)
        }
        _ => (crate::backend::vram_idle_ms(memory), u64::MAX),
    };
    let (mut went, mut bytes, mut first) = (0u64, 0u64, None);
    let all = textures();
    let mut by_age: Vec<(u64, usize)> = Vec::new();
    if most != u64::MAX {
        for &at in all.iter() {
            // SAFETY: a texture leaves the table before it is freed.
            let r = unsafe { &*(at as *const Resource) };
            let used = r.host.used.load(Relaxed);
            if used != 0 && fills(&r.desc) && crate::backend::long_ago(used, idle_ms, IDLE_FRAMES) {
                by_age.push((used & 0xffff_ffff, at));
            }
        }
        by_age.sort_unstable();
    }
    const PART: usize = 8192;
    static FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static WHOLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let whole = *WHOLE.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_SWEEP").is_some_and(|v| v == "whole"));
    let part = if most != u64::MAX {
        0
    } else if whole || idle_ms < 60_000 {
        usize::MAX
    } else {
        PART
    };
    let from = if part == PART { FROM.load(Relaxed) } else { 0 };
    let mut looked = 0;
    let began = std::time::Instant::now();
    for at in all.range(from..).take(part).copied().chain(by_age.iter().map(|&(_, at)| at)) {
        if bytes >= most {
            break;
        }
        looked += 1;
        if part == PART {
            FROM.store(if looked == PART { at + 1 } else { 0 }, Relaxed);
        }
        // SAFETY: a texture leaves the table before it is freed, under the lock held here.
        let r = unsafe { &*(at as *const Resource) };
        let used = r.host.used.load(Relaxed);
        if used == 0 || !fills(&r.desc) || !crate::backend::long_ago(used, idle_ms, IDLE_FRAMES) || crate::textures::texture::host_wrote(at) {
            continue;
        }
        let gone = {
            let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
            let idle = made.as_ref().is_some_and(|(host, _)| {
                crate::backend::long_ago(r.host.used.load(Relaxed), idle_ms, IDLE_FRAMES)
                    && std::sync::Arc::strong_count(host) == 1
                    && crate::backend::references(&host.texture) == 1
            });
            if idle {
                crate::textures::texture::unstage(at as This);
                r.host.wanted.store(u32::MAX, Relaxed);
                made.take()
            } else {
                None
            }
        };
        if let Some((host, _)) = gone {
            let reclaimed = host.vram_bytes();
            r.host.gone.store(true, Relaxed);
            KINDS[2].fetch_add(1, Relaxed);
            KINDS[3].fetch_add(reclaimed, Relaxed);
            went += 1;
            bytes += reclaimed;
            if first.is_none() {
                first = Some(at);
            }
            crate::backend::retire(host);
        }
    }
    SWEPT_US[0].fetch_add(1, Relaxed);
    SWEPT_US[1].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    SWEPT_US[2].fetch_max(began.elapsed().as_micros() as u64, Relaxed);
    if went > 0 {
        crate::backend::trail(|| {
            format!(
                "sweep: {went} host copies of textures the title fills let go ({} KiB), each held by its title texture alone (no list, no slot); the first: title texture {:#x}",
                bytes >> 10,
                first.unwrap_or(0)
            )
        });
    }
}

/// Since the start: looks of [`let_go_idle`], their microseconds, and the longest one's.
pub(crate) static SWEPT_US: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];
