//! Bindless texture queue pass, budgets, and per-frame accounting.

mod accounting;
mod probe;
mod slot;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::HostList;

use super::PASS_BUDGET;
use super::bake::{BAKE_AHEAD, BAKE_SERVED, BakeSync};
use super::state::State;

use slot::{PassState, SlotStep, process_slot};

/// Textures brought up to date for shaders that pick them by index
/// ([`State::heap_textures`]), the bytes of their memory, and the slots
/// still waiting.
pub(crate) static HEAP_TEXTURES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_WAITING: AtomicU64 = AtomicU64::new(0);
/// Passes that made no texture because the card had no room
/// (`crate::d3d12::resource::room`), for the memory line.
pub(crate) static HEAP_TEXTURES_FULL: AtomicU64 = AtomicU64::new(0);

/// Frames after a whole upload of a texture picked by index at which its
/// memory is looked at again, each counted from the look before.
pub(super) const RECHECK_FRAMES: [u64; 5] = [4, 12, 40, 120, 400];

/// Looks at uploaded textures picked by index that found the memory as it
/// was, and that found it changed (uploaded again), since the start.
pub(crate) static HEAP_TEXTURES_RECHECKED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Uploads of textures already on the card made while it had no room for
/// new ones, since the start.
pub(crate) static HEAP_TEXTURES_REUPLOADED_FULL: AtomicU64 = AtomicU64::new(0);

/// Whether a pass over the textures picked by index on a full card holds
/// back the slots that need a new host copy (`OATHKEEP_FULL_CARD_PASS=0`:
/// no, as before: they stay due, each pass looks at the same ones again
/// and stops at the 257th). Held back, such a slot is looked at again
/// after about four seconds or as soon as the card has room, and the pass
/// goes on to the slots behind it: the ones whose host copy exists and
/// only wants bringing up to date, and the ones whose memory went (letting
/// their host copies go is what makes room). What is uploaded while the
/// card has room is the same either way.
fn full_card_pass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_FULL_CARD_PASS").is_none_or(|v| v != "0"))
}

/// Whether a texture that has its host copy is still brought up to date
/// while the card has no room for new ones (`OATHKEEP_REUPLOAD_WHEN_FULL=0`:
/// no).
fn reupload_when_full() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_REUPLOAD_WHEN_FULL").is_none_or(|v| v != "0"))
}

/// Whether textures picked by index are looked at again after their upload
/// (`OATHKEEP_HEAP_RECHECK=0`: no).
fn heap_recheck() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_HEAP_RECHECK").is_none_or(|v| v != "0"))
}

/// Times a placed texture's slot was passed over because its memory is not
/// mapped.
pub(crate) static HEAP_TEXTURES_SAME: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_AWAY: AtomicU64 = AtomicU64::new(0);
/// Slots the pass found up to date already: their texture is uploaded by
/// the draws that read it through a table (it has a staging buffer).
pub(crate) static HEAP_TEXTURES_TABLED: AtomicU64 = AtomicU64::new(0);
/// The queue of textures picked by index at the last pass
/// (`TextureQueue::census`): slots in it, due, waiting for their memory,
/// uploaded in part, and the frames the oldest due one has waited.
pub(crate) static HEAP_TEXTURES_QUEUE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Since the start: looks at a slot uploaded in part that found no more of
/// its memory mapped; uploads of textures not all mapped; and of those,
/// the ones whose first page was not there (asked by the first page alone,
/// as before, they counted as not there at all).
pub(crate) static HEAP_TEXTURES_PARTLY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Minimum free bytes in `tiles::POOL` (`1 MiB` = 16 tiles) to retry waiting
/// `OATHKEEP_LEVELS_ON_DEMAND=1` textures and admit tiled mip tails when `!room`.
pub(super) const TILE_REUSE_FLOOR: u64 = 1 << 20;

/// Minimum already-committed free holes in `pool::texture_room()` (`64 MiB`)
/// needed to keep the full per-frame reuse staging budget when `!room` (filling
/// existing pool holes via `with_no_grow` allocates zero additional VRAM).
pub(super) const POOL_REUSE_HEADROOM: u64 = 64 << 20;

/// Bytes staged by `heap_textures` in the current frame.
static SPENT: AtomicU64 = AtomicU64::new(0);

pub(super) fn clamp_pass_bytes_when_full(
    bytes: u64,
    room: bool,
    required: bool,
    baking: bool,
    pool_room: u64,
) -> u64 {
    if !room && !required && !baking && pool_room < POOL_REUSE_HEADROOM {
        bytes.min(48 << 20)
    } else {
        bytes
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn heap_texture_pass_budget(
    per_frame: u64,
    span: std::time::Duration,
    on_time: usize,
    catchup: bool,
    room: bool,
    limited_ram: bool,
    required: bool,
    baking: bool,
    pool_room: u64,
) -> (u64, std::time::Duration) {
    let (per_frame, span) = if catchup && on_time >= 1024 && room && !limited_ram {
        (per_frame * 3, span * 3)
    } else {
        (per_frame, span)
    };
    (
        clamp_pass_bytes_when_full(per_frame, room, required, baking, pool_room),
        span,
    )
}

/// Per-frame texture staging byte budget scaled from the adapter's local VRAM
/// budget (`None` keeps the 192 MiB 12 GiB baseline).
pub(super) fn staging_budget_per_frame(vram_budget: Option<u64>) -> u64 {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    match vram_budget {
        Some(budget) if budget <= 6 * GIB => 96 * MIB,
        Some(budget) if budget >= 20 * GIB => 384 * MIB,
        Some(budget) if budget > 12 * GIB => 256 * MIB,
        _ => 192 * MIB,
    }
}

impl State {
    /// The texture side of [`State::heap_buffers`]: textures the title fills
    /// itself (streamed pages, layout-library copies) whose views sit in the
    /// view heap are created (placed textures have no host texture until
    /// something reads them) and uploaded, as a table read would do, for
    /// shaders that pick them by a computed index. Within a budget of bytes
    /// per frame and in all; the rest waits for later lists and frames.
    /// The most recently written slots first (`TextureQueue::due`); slots
    /// draws read through tables are not in the queue.
    /// shortcut: uploaded once, and again only when the title rewrites the
    /// slot. A placed texture not all mapped yet is uploaded with what is
    /// there and looked at again (1, 2, 4, ... frames later, at most about
    /// a second): uploaded again when more of it is mapped. Pages mapped
    /// in place of others, and contents changed with no descriptor write,
    /// are missed (hook the page-mapping events for those).
    /// `OATHKEEP_HEAP_TEXTURES=0` turns it off.
    pub(super) fn heap_textures(
        &self,
        list: &HostList,
        heap: &crate::descriptors::Heap,
        host: &crate::descriptors::host_heap::HostHeap,
        mut sync: Option<&mut BakeSync>,
    ) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_HEAP_TEXTURES").is_none_or(|v| v != "0"))
        {
            return true;
        }
        let memory = crate::backend::host().and_then(|h| h.video_memory());
        // `OATHKEEP_TEXTURE_STAGE_MB` sets it: a backlog of tens of thousands of
        // textures (a scene's first minutes) then drains in fewer frames.
        static PER_FRAME_MB: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let asked = *PER_FRAME_MB.get_or_init(|| {
            std::env::var("OATHKEEP_TEXTURE_STAGE_MB")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        });
        let per_frame = match asked {
            Some(mb) => mb << 20,
            None => {
                let base = staging_budget_per_frame(memory.map(|[budget, ..]| budget));
                if crate::memory::residency::limited_host_ram() {
                    base.min(128 << 20)
                } else {
                    base
                }
            }
        };
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        static FRAME_NS: AtomicU64 = AtomicU64::new(0);
        let frame = crate::d3d12::frame::current();
        if FRAME.swap(frame, Ordering::Relaxed) != frame {
            SPENT.store(0, Ordering::Relaxed);
            FRAME_NS.store(0, Ordering::Relaxed);
        }
        // No more of them are made while the card has no room
        // (`resource::room`): by what is alive now, not by what was ever
        // staged (4 GB in all, after which later textures stayed empty for
        // the run). The ones nothing names any more are let go, which
        // makes room again: so the slots are still looked at (the pass
        // returned here, and a slot whose texture had been streamed out
        // kept its host copy for as long as the card was full).
        let room = crate::d3d12::resource::room(memory);
        if !room {
            HEAP_TEXTURES_FULL.fetch_add(1, Ordering::Relaxed);
            once!("textures picked by index: the card has no room for more; they wait");
        }
        // Room again: the slots held back while the card was full are due
        // now (`full_card_pass`).
        if room {
            host.texture_slots_room(frame);
        }
        // Slots whose texture's memory the runtime mapped, unmapped or
        // wrote since the last pass are due again.
        host.texture_slots_remapped(frame);
        let baking = crate::textures::texture::BAKING.get();
        if baking {
            host.texture_slots_bake_written(frame);
        }
        host.texture_slots_short(frame);
        // The slots of textures whose first level with memory (levels on
        // demand), or whose host copy for one with more levels, changed since
        // the last pass are made again from it; those whose new view's upload
        // has run are made from that.
        host.texture_views_remade();
        host.texture_slots_flip();
        // And with room (or at least 1 MiB = 16 free 64 KiB tiles committed in
        // `tiles::POOL`), placed textures whose upload left a held level without
        // tiles (`OATHKEEP_LEVELS_ON_DEMAND=1`) are retried instead of stranding
        // 14-15 MiB of free tiles behind a 16 MiB gate.
        if room || crate::memory::tiles::free_room() >= TILE_REUSE_FLOOR {
            host.texture_slots_waiting(frame);
        }
        let required = PASS_BUDGET.get().is_some_and(|(_, _, bake)| bake);
        let (mut due, on_time, census) = if baking {
            host.texture_slots_due_prioritized(frame, 1 << 16, 1 << 16, 512, 256)
        } else {
            host.texture_slots_due_prioritized(frame, 512, 1024, 64, 64)
        };
        if baking {
            let known: std::collections::HashSet<u32> =
                due.iter().map(|&(index, _)| index).collect();
            static AHEAD: AtomicU64 = AtomicU64::new(u64::MAX);
            let again = AHEAD.swap(frame, Ordering::Relaxed) == frame;
            let (ahead, _) =
                host.texture_slots_due(frame + BAKE_AHEAD[usize::from(again)], 1 << 16);
            due.extend(ahead.into_iter().filter(|(index, _)| !known.contains(index)));
        }
        HEAP_TEXTURES_WAITING.store(census[0], Ordering::Relaxed);
        for (count, now) in HEAP_TEXTURES_QUEUE.iter().zip(census) {
            count.store(now, Ordering::Relaxed);
        }
        // Where the card's memory is, and what the due slots would need
        // (`OATHKEEP_CARD_ACCOUNTING=1`; a line when the card first has no
        // room, and every five seconds after it).
        Self::card_accounting(heap, host, frame, room);
        // The frames in about four seconds, between 16 and 256: how long a
        // slot with nothing to upload waits at most for its next look. In
        // frames alone it was 120: four minutes at the opening cutscene's
        // half a frame per second, while the title streams by the clock.
        // (About a second before the runtime told of the memory it maps:
        // the looks are now only for what that does not tell.)
        static PASSED: AtomicU64 = AtomicU64::new(0);
        let now = crate::backend::now_ms();
        let took = now.saturating_sub(PASSED.swap(now, Ordering::Relaxed)).max(1);
        let most = (4000 / took).clamp(16, 256);
        // Cap synchronous recording-thread wall time strictly in steady state
        // (`5 ms` default, overridable via `OATHKEEP_TEXTURE_STAGE_MS`), with a
        // modest `15 ms` allowance during cold boot when VRAM has room and a
        // large fresh backlog is draining.
        static SPAN_MS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let asked_ms = *SPAN_MS.get_or_init(|| {
            std::env::var("OATHKEEP_TEXTURE_STAGE_MS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
        });
        // A backlog (a scene's first minutes: 22,000 slots due, 2-3 minutes of black
        // surfaces at 192 MB a frame) gets three times the bytes until it drains;
        // `OATHKEEP_TEXTURE_CATCHUP=0` keeps the plain budget.
        static CATCHUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let catchup = *CATCHUP
            .get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_CATCHUP").is_none_or(|v| v != "0"));
        let default_ms = if catchup
            && on_time >= 512
            && room
            && !crate::memory::residency::limited_host_ram()
            && frame < 600
        {
            15
        } else {
            5
        };
        let span = std::time::Duration::from_millis(asked_ms.unwrap_or(default_ms));
        let pool_room = crate::memory::pool::texture_room();
        let (per_frame, _) = heap_texture_pass_budget(
            per_frame,
            span,
            on_time,
            catchup,
            room,
            crate::memory::residency::limited_host_ram(),
            required,
            baking,
            pool_room,
        );
        // A run made early (`State::early_textures`) has a budget of its
        // own: its bytes on top of what the frame has spent so far.
        // (Not `most`: that is the longest wait between looks, used below.)
        let (pass_budget, span, bytes_at_most) = match PASS_BUDGET.get() {
            Some((ms, bytes, _)) => {
                let bytes = clamp_pass_bytes_when_full(bytes, room, required, baking, pool_room);
                (
                    true,
                    std::time::Duration::from_millis(ms),
                    SPENT.load(Ordering::Relaxed).saturating_add(bytes),
                )
            }
            None => (false, span, per_frame),
        };
        // Apply the shared frame ceiling after early-pass and catch-up budgets.
        // A bake samples its inputs immediately and cannot retry that draw.
        let bytes_at_most = if required || asked.is_some() {
            bytes_at_most
        } else {
            crate::memory::residency::prefetch_limit(
                bytes_at_most,
                crate::memory::residency::pressured(),
                crate::memory::residency::critical_commit(),
                crate::memory::residency::limited_host_ram(),
                crate::textures::texture::live_staging_bytes(),
            )
        };

        let remaining_span = if pass_budget {
            span
        } else {
            let already = std::time::Duration::from_nanos(FRAME_NS.load(Ordering::Relaxed));
            span.saturating_sub(already)
        };
        if remaining_span.is_zero() || due.is_empty() {
            if sync.is_none() && baking && due.is_empty() {
                BAKE_SERVED.store(host.texture_news().0 + 1, Ordering::Relaxed);
            }
            return due.is_empty();
        }
        let began = std::time::Instant::now();
        let deadline = began + remaining_span;
        struct FrameTimeGuard<'a>(&'a std::time::Instant, bool);
        impl Drop for FrameTimeGuard<'_> {
            fn drop(&mut self) {
                if !self.1 {
                    FRAME_NS.fetch_add(self.0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                }
            }
        }
        let _frame_time_guard = FrameTimeGuard(&began, pass_budget);
        if sync.is_none() {
            BAKE_SERVED.store(0, Ordering::Relaxed);
        }
        let mut pass = PassState {
            frame,
            most,
            baking,
            required,
            room,
            since_asked: 0,
            refused: 0,
        };
        let mut served = true;
        for (iter, (index, write)) in due.into_iter().enumerate() {
            if SPENT.load(Ordering::Relaxed) >= bytes_at_most
                || ((iter & 7) == 0 && std::time::Instant::now() >= deadline)
            {
                served = false;
                break;
            }
            match process_slot(list, heap, host, &mut pass, sync.as_deref_mut(), index, write) {
                SlotStep::Continue => {}
                SlotStep::Break => break,
                SlotStep::StopUnserved => {
                    served = false;
                    break;
                }
                SlotStep::DeviceRemoved => return false,
            }
        }
        if sync.is_none() && baking && served {
            BAKE_SERVED.store(host.texture_news().0 + 1, Ordering::Relaxed);
        }
        served
    }
}
