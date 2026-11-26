//! Host-memory and VRAM residency admission for speculative texture work,
//! LRU eviction targeting, and reusable upload storage.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
static PRESSURED: AtomicBool = AtomicBool::new(false);
static HEADROOM: AtomicU64 = AtomicU64::new(u64::MAX);
static VRAM_LEVEL: AtomicU8 = AtomicU8::new(0);
static VRAM_RECLAIM: AtomicU64 = AtomicU64::new(0);

/// Local VRAM residency pressure tier for texture admission and LRU eviction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum VramLevel {
    Normal = 0,
    High = 1,
    Critical = 2,
}

impl VramLevel {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::High,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }
}

/// Result of evaluating local VRAM usage against the adapter's budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VramPlan {
    pub(crate) level: VramLevel,
    /// Target bytes of cold title-filled textures to evict to restore headroom.
    pub(crate) reclaim_bytes: u64,
    pub(crate) effective_used: u64,
    pub(crate) budget: u64,
}

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn allocation_error(error: &windows::core::Error) {
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        RETRY_UPLOAD_AT.fetch_max(crate::host::coarse_ms().saturating_add(500), Ordering::Relaxed);
    }
}

thread_local! {
    static STAGING_DEFERRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_staging_deferred() {
    STAGING_DEFERRED.set(true);
}

pub(crate) fn take_staging_deferred() -> bool {
    STAGING_DEFERRED.replace(false)
}

pub(crate) fn upload_refused() -> bool {
    crate::host::coarse_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    // Keep a protective floor against Rust/game OOM aborts without starving
    // bindless world textures or one-shot ped decoration bakes when steady-state
    // commit headroom sits in the 1-4 GiB range on 32 GiB hosts.
    commit >= if bake { 256 * MIB } else { 512 * MIB }
}

fn has_growth_reserve(commit: u64, bake: bool) -> bool {
    // Growing a fresh 64 MiB upload block for speculative bindless prefetch
    // requires more headroom than reusing slices already committed inside the
    // staging pool.
    commit >= if bake { 256 * MIB } else { 1024 * MIB }
}

/// Leave commit for game allocations and submissions already in flight while
/// still letting bindless textures and one-shot material bakes upload.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_upload_reserve(sample_commit(now), bake)
}

/// Whether the staging allocator may commit a brand-new OS upload buffer
/// (reusing existing free ranges in already-committed pool blocks stays open
/// whenever `admit_texture_pass` is true).
pub(crate) fn allow_staging_growth(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_growth_reserve(sample_commit(now), bake)
}

pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 32 * GIB))
}

/// Physical and commit pressure thresholds scaled to total host RAM so
/// 16 GiB hosts do not trip permanently into pressure when Windows retains
/// ~2-3 GiB free physical RAM alongside ample pagefile commit headroom.
fn pressure_thresholds(total_phys: u64) -> (u64, u64, u64, u64) {
    if total_phys == 0 || total_phys > 20 * GIB {
        (3 * GIB, 5 * GIB, 4 * GIB, 6 * GIB)
    } else {
        let enter_phys = (total_phys / 10).clamp(1024 * MIB, 3 * GIB);
        let exit_phys = (total_phys / 6).clamp(1536 * MIB, 5 * GIB);
        (enter_phys, exit_phys, 4 * GIB, 6 * GIB)
    }
}

fn next_pressure_for(was: bool, total_phys: u64, physical: u64, commit: u64) -> bool {
    let (enter_phys, exit_phys, enter_commit, exit_commit) = pressure_thresholds(total_phys);
    if was {
        physical < exit_phys || commit < exit_commit
    } else {
        physical <= enter_phys || commit <= enter_commit
    }
}

#[cfg(test)]
fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    next_pressure_for(was, 32 * GIB, physical, commit)
}

/// Evaluates local VRAM residency pressure and computes how many bytes of cold
/// title-filled textures should be evicted to keep `resource::room()` open
/// before the 90% cutoff starves newly streamed textures.
pub(crate) fn vram_plan(
    budget: u64,
    used: u64,
    pool_free: u64,
    reserved_extra: u64,
    filled_textures: u64,
) -> VramPlan {
    if budget == 0 {
        return VramPlan { level: VramLevel::Normal, reclaim_bytes: 0, effective_used: 0, budget: 0 };
    }
    let effective_used = used.saturating_sub(pool_free).saturating_add(reserved_extra);
    let target_water = budget / 100 * 76;
    let high_water = budget / 100 * 84;
    let critical_water = budget / 100 * 90;
    // Keep a working-set floor of active textures so we never thrash the
    // visible scene on tight cards.
    let floor = (1024 * MIB).min(budget / 4);
    let evictable = filled_textures.saturating_sub(floor);
    if effective_used >= critical_water {
        let deficit = effective_used.saturating_sub(target_water);
        let min_batch = (256 * MIB).min(evictable);
        VramPlan {
            level: VramLevel::Critical,
            reclaim_bytes: deficit.max(min_batch).min(evictable),
            effective_used,
            budget,
        }
    } else if effective_used >= high_water {
        let deficit = effective_used.saturating_sub(target_water);
        VramPlan {
            level: VramLevel::High,
            reclaim_bytes: deficit.min(evictable),
            effective_used,
            budget,
        }
    } else {
        VramPlan { level: VramLevel::Normal, reclaim_bytes: 0, effective_used, budget }
    }
}

/// Samples current DXGI local VRAM usage and updates the cached VRAM pressure
/// tier and eviction target.
pub(crate) fn sample_vram() -> VramPlan {
    let video = crate::host::host().and_then(|h| h.video_memory());
    let plan = match video {
        Some([budget, used, ..]) => {
            let pool_free = crate::pool::texture_room();
            let reserved_extra =
                crate::shadow::card_room_kept().saturating_add(crate::resource::targets_room_kept());
            let filled_textures =
                crate::resource::KINDS[1].load(Ordering::Relaxed).saturating_add(crate::tiles::given_total());
            vram_plan(budget, used, pool_free, reserved_extra, filled_textures)
        }
        None => VramPlan { level: VramLevel::Normal, reclaim_bytes: 0, effective_used: 0, budget: 0 },
    };
    VRAM_LEVEL.store(plan.level as u8, Ordering::Relaxed);
    VRAM_RECLAIM.store(plan.reclaim_bytes, Ordering::Relaxed);
    plan
}

pub(crate) fn vram_level() -> VramLevel {
    VramLevel::from_u8(VRAM_LEVEL.load(Ordering::Relaxed))
}

/// Admission policy for speculative background uploads (`texture::ahead`),
/// keeping copy-queue staging and VRAM free for actively drawn textures.
pub(crate) fn admit_ahead(
    vram: VramLevel,
    in_flight_bytes: u64,
    job_bytes: u64,
    base_limit: u64,
    draw_active: bool,
    growth_ok: bool,
) -> bool {
    if !growth_ok {
        return false;
    }
    let effective_limit = match vram {
        VramLevel::Critical => {
            // Under critical VRAM pressure, still let compact mip-tail / low-mip
            // textures through on a small cap, but defer multi-megabyte
            // speculative uploads so visible draw-bound textures go first.
            if job_bytes > 512 * 1024 {
                return false;
            }
            base_limit.min(8 * MIB)
        }
        VramLevel::High if draw_active => base_limit.min(16 * MIB),
        VramLevel::High => (base_limit * 3) / 4,
        VramLevel::Normal if draw_active => (base_limit / 2).max(16 * MIB).min(base_limit),
        VramLevel::Normal => base_limit,
    };
    in_flight_bytes < effective_limit
}

/// Whether a speculative picked-by-index upload may allocate a brand-new
/// non-pooled upload buffer when draws are actively uploading or VRAM is full.
pub(crate) fn allow_speculative_staging(job_bytes: u64, draw_active: bool) -> bool {
    if !allow_staging_growth(false) {
        return false;
    }
    admit_speculative_staging(vram_level(), job_bytes, draw_active, critical_commit())
}

fn admit_speculative_staging(vram: VramLevel, job_bytes: u64, draw_active: bool, critical_commit: bool) -> bool {
    if critical_commit {
        return false;
    }
    if vram == VramLevel::Critical && draw_active && job_bytes > 2 * MIB {
        return false;
    }
    true
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    let _ = sample_vram();
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    HEADROOM.store(physical.min(commit), Ordering::Relaxed);
    let was = pressured();
    let pressure = next_pressure_for(was, status.ullTotalPhys, physical, commit);
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        if pressure {
            tracing::warn!(
                pid = std::process::id(),
                frame = crate::frame::current(),
                physical_free_mib = physical >> 20,
                available_commit_mib = commit >> 20,
                total_physical_mib = status.ullTotalPhys >> 20,
                buffer_mirror_span_mib = crate::shadow::live_span_bytes() >> 20,
                texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
                spare_scratch_mib = crate::host::ALIVE[4].load(Ordering::Relaxed) >> 20,
                game_filled_gpu_texture_mib = crate::resource::KINDS[1].load(Ordering::Relaxed) >> 20,
                "host memory pressure: limiting texture prefetch and idle upload storage"
            );
        } else {
            tracing::info!(physical_free_mib = physical >> 20, available_commit_mib = commit >> 20, "host memory pressure cleared");
        }
    }
}

pub(crate) fn headroom() -> Option<u64> {
    let bytes = HEADROOM.load(Ordering::Relaxed);
    (bytes != u64::MAX).then_some(bytes)
}

pub(crate) fn pressured() -> bool {
    PRESSURED.load(Ordering::Relaxed)
}

/// True when OS commit headroom is critically low (< 1.5 GiB) or an allocation
/// just failed with `E_OUTOFMEMORY`, requiring emergency throttling.
pub(crate) fn critical_commit() -> bool {
    let now = crate::host::coarse_ms();
    now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) || sample_commit(now) < 1536 * MIB
}

/// A soft per-frame admission limit. A single texture can cross it. The
/// queue retains unserved slots so they can be retried on later frames.
pub(crate) fn prefetch_limit(bytes: u64, pressure: bool, critical: bool, limited: bool, staging: u64) -> u64 {
    if critical {
        bytes.min(16 << 20)
    } else if pressure || (limited && staging >= GIB) {
        bytes.min(64 << 20)
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bake_and_lookahead_leave_room_for_live_game_allocations() {
        for (commit, bake, lookahead, growth) in [
            (128 * MIB, false, false, false),
            (256 * MIB, true, false, false),
            (512 * MIB, true, true, false),
            (768 * MIB, true, true, false),
            (1024 * MIB, true, true, true),
        ] {
            assert_eq!(has_upload_reserve(commit, true), bake);
            assert_eq!(has_upload_reserve(commit, false), lookahead);
            assert_eq!(has_growth_reserve(commit, false), growth);
        }
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        assert!(next_pressure(false, 12 * GIB, 4 * GIB));
        assert!(next_pressure(false, 3 * GIB, 16 * GIB));
        assert!(next_pressure(true, 4 * GIB, 7 * GIB));
        assert!(next_pressure(true, 6 * GIB, 5 * GIB));
        assert!(!next_pressure(true, 5 * GIB, 6 * GIB));
        assert!(!next_pressure(false, 4 * GIB, 5 * GIB));
    }

    #[test]
    fn prefetch_respects_existing_budgets_and_resumes_after_recovery() {
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB - 1), 512 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, false, 2 * GIB), 512 << 20);
        assert_eq!(prefetch_limit(0, true, true, false, 0), 0);
        assert_eq!(prefetch_limit(8 << 20, true, true, false, 0), 8 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, false, false, 0), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, true, false, 0), 16 << 20);
        assert_eq!(prefetch_limit(192 << 20, false, false, false, 0), 192 << 20);
    }

    #[test]
    fn pressure_scales_physical_threshold_on_16gib_hosts() {
        let host_16g = 16 * GIB;
        // 2577 MiB free physical RAM with 17 GiB commit headroom on a 16 GiB
        // host is normal Windows cache equilibrium, not memory pressure.
        assert!(!next_pressure_for(false, host_16g, 2577 * MIB, 17112 * MIB));
        // Genuine physical RAM exhaustion (< 1.6 GiB) still triggers pressure.
        assert!(next_pressure_for(false, host_16g, 1500 * MIB, 17112 * MIB));
        // Hysteresis keeps pressure active until physical >= 2.67 GiB and commit >= 6 GiB.
        assert!(next_pressure_for(true, host_16g, 2400 * MIB, 17112 * MIB));
        assert!(!next_pressure_for(true, host_16g, 2800 * MIB, 17112 * MIB));
        // Low commit headroom still triggers pressure even with plenty of physical RAM.
        assert!(next_pressure_for(false, host_16g, 4 * GIB, 3500 * MIB));
    }

    #[test]
    fn vram_plan_triggers_proactive_and_critical_eviction_on_8gib_cards() {
        let budget = 7249 * MIB;
        // Under 84% effective usage: no eviction needed.
        let ok = vram_plan(budget, 5000 * MIB, 0, 500 * MIB, 2500 * MIB);
        assert_eq!(ok.level, VramLevel::Normal);
        assert_eq!(ok.reclaim_bytes, 0);

        // At 86% effective usage (before 90% cutoff): proactive High eviction down to 76%.
        let high = vram_plan(budget, 6250 * MIB, 0, 0, 3300 * MIB);
        assert_eq!(high.level, VramLevel::High);
        assert!(high.reclaim_bytes > 500 * MIB);

        // Pool free room subtracts from effective usage.
        let pooled = vram_plan(budget, 6250 * MIB, 500 * MIB, 0, 3300 * MIB);
        assert_eq!(pooled.level, VramLevel::Normal);
        assert_eq!(pooled.reclaim_bytes, 0);

        // At >= 90% effective usage (card full): Critical eviction with working-set floor.
        let crit = vram_plan(budget, 6600 * MIB, 0, 200 * MIB, 3331 * MIB);
        assert_eq!(crit.level, VramLevel::Critical);
        assert!(crit.reclaim_bytes >= 1000 * MIB);

        // Working-set floor protects the last 1 GiB of active textures.
        let floored = vram_plan(budget, 6800 * MIB, 0, 0, 1100 * MIB);
        assert_eq!(floored.level, VramLevel::Critical);
        assert_eq!(floored.reclaim_bytes, 76 * MIB);

        // Zero budget (unknown adapter): Normal, zero reclaim.
        let unknown = vram_plan(0, 4000 * MIB, 0, 0, 2000 * MIB);
        assert_eq!(unknown.level, VramLevel::Normal);
        assert_eq!(unknown.reclaim_bytes, 0);
    }

    #[test]
    fn ahead_admission_prioritizes_draws_and_small_mips_under_vram_pressure() {
        // Under Critical VRAM pressure, big speculative uploads wait while small mip-tails pass.
        assert!(!admit_ahead(VramLevel::Critical, 0, 4 * MIB, 32 * MIB, false, true));
        assert!(admit_ahead(VramLevel::Critical, 0, 256 * 1024, 32 * MIB, false, true));
        assert!(!admit_ahead(VramLevel::Critical, 8 * MIB, 256 * 1024, 32 * MIB, false, true));
        // Under High VRAM pressure + active draws, speculative cap tightens to 16 MiB.
        assert!(admit_ahead(VramLevel::High, 15 * MIB, 1 * MIB, 32 * MIB, true, true));
        assert!(!admit_ahead(VramLevel::High, 16 * MIB, 1 * MIB, 32 * MIB, true, true));
        // Growth disabled blocks speculative ahead uploads.
        assert!(!admit_ahead(VramLevel::Normal, 0, 64 * 1024, 32 * MIB, false, false));
        // Speculative staging fallback yields to active draws under critical VRAM.
        assert!(!admit_speculative_staging(VramLevel::Critical, 4 * MIB, true, false));
        assert!(admit_speculative_staging(VramLevel::Critical, 256 * 1024, true, false));
        assert!(!admit_speculative_staging(VramLevel::Normal, 64 * 1024, false, true));
    }
}
