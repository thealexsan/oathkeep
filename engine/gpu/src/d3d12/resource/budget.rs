use super::host_tex::HOST_TEXTURES;
use super::hosted::KINDS;

/// Unconditional host-texture floor for a given local VRAM budget.
pub(super) fn texture_floor_bytes(vram_budget: Option<u64>) -> u64 {
    const MAX_FLOOR: u64 = 2 << 30;
    match vram_budget {
        Some(budget) => MAX_FLOOR.min(budget / 4),
        None => MAX_FLOOR,
    }
}

/// Whether more host copies of textures the title fills may be made for
/// shaders that pick textures by index.
pub(crate) fn room(memory: Option<[u64; 4]>) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    const WITHOUT_BUDGET: u64 = 4 << 30;
    static LIMIT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        let mb = std::env::var("OATHKEEP_TEXTURE_BUDGET_MB").ok()?;
        mb.trim().parse::<u64>().ok().map(|mb| mb << 20)
    });
    let filled = KINDS[1].load(Relaxed) + crate::memory::tiles::given_total();
    if let Some(limit) = limit {
        return filled < limit;
    }
    let floor = texture_floor_bytes(memory.map(|[budget, ..]| budget));
    if filled < floor {
        return true;
    }
    match memory {
        Some([budget, used, ..]) => {
            let limit = budget / 100 * room_percent();
            if used >= limit {
                return false;
            }
            let pool_free = crate::memory::residency::pool_free_credit(
                budget,
                used,
                crate::memory::pool::texture_room().saturating_add(crate::memory::tiles::free_room()),
            );
            used.saturating_sub(pool_free) + crate::memory::shadow::card_room_kept() + targets_room_kept(Some(budget))
                < limit
        }
        None => filled < WITHOUT_BUDGET,
    }
}

/// Target VRAM reserve ceiling for render targets and other host-written
/// textures not yet created.
pub(super) fn targets_room_limit(vram_budget: Option<u64>, env_mb: Option<u64>) -> u64 {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const FULL_DEFAULT: u64 = 3072 * MIB;
    if let Some(mb) = env_mb {
        return mb.min(8192) * MIB;
    }
    match vram_budget {
        Some(budget) if budget < 11 * GIB => (budget / 5).clamp(512 * MIB, FULL_DEFAULT),
        _ => FULL_DEFAULT,
    }
}

/// The bytes of the card's memory the texture queue leaves free for
/// render targets and other host copies the host itself writes.
pub(crate) fn targets_room_kept(vram_budget: Option<u64>) -> u64 {
    use std::sync::atomic::Ordering::Relaxed;
    static ENV_MB: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let env_mb = *ENV_MB.get_or_init(|| {
        std::env::var("OATHKEEP_CARD_ROOM_FOR_TARGETS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
    });
    let most = targets_room_limit(vram_budget, env_mb);
    let targets = HOST_TEXTURES[1].load(Relaxed).saturating_sub(KINDS[1].load(Relaxed));
    most.saturating_sub(targets)
}

/// [`room`] as it was at most a tenth of a second ago.
pub(crate) fn room_lately() -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tenth = crate::backend::now_ms() / 100 + 1;
    let last = LAST.load(Relaxed);
    if last >> 1 == tenth {
        return last & 1 != 0;
    }
    let room = room(crate::backend::host().and_then(|h| h.video_memory()));
    LAST.store((tenth << 1) | u64::from(room), Relaxed);
    room
}

fn room_percent() -> u64 {
    static PERCENT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *PERCENT.get_or_init(|| {
        std::env::var("OATHKEEP_TEXTURE_ROOM_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(90, |percent| percent.clamp(50, 97))
    })
}
