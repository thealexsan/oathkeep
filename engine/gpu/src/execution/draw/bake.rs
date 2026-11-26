//! Early and synchronous decoration-bake bindless texture uploads.

use std::sync::atomic::{AtomicU64, Ordering};

use oathkeep_shader::{ResourceType, RootType, UsedResource};

use crate::descriptors::View;
use crate::backend::HostList;
use crate::d3d12::object::This;
use crate::d3d12::pipeline::host::HostPipeline;

use super::PASS_BUDGET;
use super::heap_textures::{HEAP_TEXTURES, HEAP_TEXTURES_QUEUE};
use super::readable::readable;
use super::state::State;

/// The milliseconds of every frame the pass that uploads textures picked
/// by index takes before a shader that picks by index has asked for it
/// (`OATHKEEP_EARLY_TEXTURES_MS`, 12; 0: never, as before), and the
/// milliseconds it may take just before a draw of the title's decoration
/// bake (`OATHKEEP_BAKE_TEXTURES_MS`, 1500; 0: nothing special).
///
/// The pass ran only from the first world frame on, where it met some
/// 24,000 slots at once and needed 60-80 frames for them. The title bakes
/// its characters' skin and decorations into textures of its own while it
/// loads, 159-1,165 frames before that (runs 156-174), with a pixel shader
/// that picks its sources by index: whatever was not on the card then was
/// baked as nothing, once, for the run. On the console a texture is there
/// when its bytes are.
fn early_budgets() -> (u64, u64) {
    static MS: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default)
        };
        (
            ms("OATHKEEP_EARLY_TEXTURES_MS", 12),
            ms("OATHKEEP_BAKE_TEXTURES_MS", 1500),
        )
    })
}

/// Whether a draw of the title's bake finds the textures it picks by index
/// as title memory has them at that moment (`OATHKEEP_BAKE_EXACT=0`: as the
/// pass leaves them, as before). The pass looks at a slot whose memory was
/// written or mapped a frame or two later, by when the rest of it has
/// landed, and at one uploaded whole a few frames later again, for a title
/// that fills memory unnoticed: a bake in between took the bytes from before
/// (another texture's), and what it bakes stays: coloured specks all over
/// the waitress's face for the whole diner scene (run n01), none in other
/// runs. So the pass ahead of a bake takes those slots too, and its uploads
/// wait for title memory that is being remapped (`crate::textures::texture` `BAKING`).
fn bake_exact() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_BAKE_EXACT").is_none_or(|v| v != "0"))
}

/// Whether a texture found changed with nothing to tell of it is looked at
/// again in the next frame (`OATHKEEP_RECHECK_SOON=0`: after four, as
/// before).
pub(super) fn recheck_soon() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_RECHECK_SOON").is_none_or(|v| v != "0"))
}

/// Frames ahead the pass before a bake takes slots from that are due only
/// then ([`bake_exact`]): those of notices (1 and 2), and the first looks
/// again ([`super::heap_textures::RECHECK_FRAMES`]), these at the frame's first bake alone (a
/// look reads 48 KiB, and a frame bakes up to sixteen times).
pub(super) const BAKE_AHEAD: [u64; 2] = [12, 2];

/// What [`crate::descriptors::host_heap::HostHeap::texture_news`] said when the
/// pass before a bake last served all there was, plus one (0: it did not):
/// until something is new, the next bakes of the frame need no pass.
pub(super) static BAKE_SERVED: AtomicU64 = AtomicU64::new(0);

/// Free-threaded wrapper around a reusable upload-only [`HostList`] protected
/// by [`BAKE_UPLOAD`]. D3D12 command allocators, lists, and retained COM
/// resources are free-threaded when externally synchronized.
struct BakeList(HostList);
// SAFETY: accessed exclusively under `BAKE_UPLOAD`'s mutex.
unsafe impl Send for BakeList {}

/// Serializes synchronous one-shot ped/decoration bake uploads across
/// concurrent `OATHKEEP_THIN=1` recording threads and owns the reusable
/// upload-only host command list.
static BAKE_UPLOAD: std::sync::Mutex<Option<BakeList>> = std::sync::Mutex::new(None);

/// Pending descriptor publications for a synchronous bake upload pass: slots
/// stay on `neutral()` (or their previous compatible view) until the dedicated
/// upload list has finished executing on the host GPU.
pub(super) struct BakeSync {
    pub(super) publish: Vec<(u32, u64, u64)>,
    pub(super) keep: Vec<std::sync::Arc<crate::d3d12::resource::HostTexture>>,
}

/// The pixel shaders of the title's decoration bake (the title:
/// `PS_BakeDecorations_*`), by the first 16 hex digits of their hash as the
/// frame listings print it. They draw into a 256x256 corner of 2048x2048
/// targets and name no texture in a table.
pub(crate) const BAKE_PIXEL_SHADERS: [&str; 7] = [
    "c0a7fe77b1862e10",
    "0d1575a396dfe513",
    "b7cfd8dda91ebb6b",
    "280967ad158cee0b",
    "fa455c8a3c32577b",
    "d126bcae5d5d8d26",
    "fcee612943c008e0",
];

/// Geometry stages shared by every ped texture compositor / decoration bake
/// permutation (`vertex 089d6271be6650ef` fullscreen-triangle ped baker and
/// `mesh d4111a6e6ea49d91` decoration projection mesh), catching pixel-shader
/// permutations beyond `BAKE_PIXEL_SHADERS`.
const BAKE_GEOMETRY_SHADERS: [&str; 2] = ["089d6271be6650ef", "d4111a6e6ea49d91"];

/// Since the start: early runs of the texture pass, and those of them
/// made for a bake draw.
pub(crate) static EARLY_TEXTURE_PASSES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

impl State {
    /// Ensures textures claimed by background copy-queue uploads
    /// (`OATHKEEP_UPLOAD_AHEAD=1`) or deferred behind the `neutral()` gray
    /// stand-in (`OATHKEEP_SHOW_AFTER=1` / `shows_other`) have finished
    /// uploading and flipped to their real descriptors before a one-shot
    /// decoration bake draw samples them.
    fn flush_for_bake(
        &self,
        list: &HostList,
        host: &crate::descriptors::host_heap::HostHeap,
        mut sync: Option<&mut BakeSync>,
    ) {
        static RESCUED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
        let drained = crate::textures::texture::prefetch::drain();
        let mut settled = false;
        let mut flipped = 0usize;
        for _ in 0..4 {
            let need_settle = host.needs_gpu_settle_for_bake();
            if need_settle {
                settled = true;
                crate::backend::settle_for_fence();
            }
            flipped += host.texture_views_remade();
            flipped += host.texture_slots_flip();
            if !need_settle || !host.needs_gpu_settle_for_bake() {
                break;
            }
        }
        let pending = host.take_pending_list_flips();
        let mut promoted = 0usize;
        if !pending.is_empty() {
            let this_list = (std::ptr::from_ref(list) as usize, list.epoch());
            let sampled = UsedResource {
                kind: ResourceType::Srv,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 8,
                user_data: 0,
                slot: 0,
            };
            for item in pending {
                let (origin, index, handle, key) =
                    (item.origin, item.index(), item.handle(), item.key());
                if !host.slot_still_at(handle, key) {
                    continue;
                }
                let Some(view) = crate::descriptors::view(handle) else {
                    continue;
                };
                if let View::Placed(_, resource, desc) = &view {
                    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                    let len = crate::d3d12::resource::placed_bytes(resource).unwrap_or(1);
                    if crate::textures::texture::mapped_bytes(address as *const u8, len as usize) == 0 {
                        host.restore_pending_list_flip(item);
                        continue;
                    }
                }
                if sync.is_some() || origin != this_list {
                    let placed = crate::descriptors::host_heap::placed_texture(&view);
                    if let Some(&View::Shader(resource, _)) = placed.as_ref().or(Some(&view)) {
                        let resource = resource as This;
                        let texture = crate::d3d12::resource::host_resource(resource);
                        let title_fills = texture.is_some_and(crate::textures::texture::title_written);
                        let first = match (&placed, &view) {
                            (Some(_), View::Placed(_, _, desc)) => crate::d3d12::resource::first_mip(desc),
                            _ => 0,
                        };
                        if let (Some(sync), Some(copy)) =
                            (sync.as_deref_mut(), crate::d3d12::resource::host_copy(resource))
                        {
                            sync.keep.push(copy);
                        }
                        if title_fills
                            && !crate::textures::texture::prefetch::there(resource)
                            && (sync.is_some() || !crate::textures::texture::unchanged(resource, first))
                        {
                            let was_staged = crate::textures::texture::is_staged(resource);
                            let _ = crate::memory::residency::take_staging_deferred();
                            readable(list, sampled.kind, &view);
                            let failed = crate::memory::residency::take_staging_deferred()
                                || crate::memory::residency::upload_refused();
                            if !was_staged {
                                crate::textures::texture::unstage(resource);
                            }
                            if failed {
                                host.restore_pending_list_flip(item);
                                continue;
                            }
                        }
                    }
                }
                if let Some(sync) = sync.as_deref_mut() {
                    sync.publish.push((index, handle, key));
                } else {
                    let _ = host.mirror(index, handle, true);
                }
                promoted += 1;
            }
        }
        if drained > 0 || settled || flipped > 0 || promoted > 0 {
            let n = RESCUED[0].fetch_add(1, Ordering::Relaxed) + 1;
            let all_drained =
                RESCUED[1].fetch_add(drained as u64, Ordering::Relaxed) + drained as u64;
            let all_flipped =
                RESCUED[2].fetch_add(flipped as u64, Ordering::Relaxed) + flipped as u64;
            let all_promoted =
                RESCUED[3].fetch_add(promoted as u64, Ordering::Relaxed) + promoted as u64;
            if n.is_power_of_two() {
                tracing::info!(
                    frame = crate::d3d12::frame::current(),
                    flushes = n,
                    drained_ahead = all_drained,
                    flipped = all_flipped,
                    promoted_from_lists = all_promoted,
                    settled,
                    "flushed deferred texture stand-ins before decoration bake"
                );
            }
        }
    }

    /// Whether a graphics draw is a one-shot texture compositor / decoration
    /// bake whose bindless texture inputs must be flushed and up to date
    /// before recording the draw. Matches known bake pixel/geometry shaders
    /// and structurally detects depth-less 4+ MRT square atlas bakes
    /// (`>= 512x512`) so unseen NPC/head/clothing shader permutations never
    /// bake `neutral()` or `null` stand-ins into permanent ped textures.
    fn is_bake_draw(&self, p: &HostPipeline) -> bool {
        if p.stages.iter().any(|s| {
            let h = s.shader.hash.as_str();
            BAKE_PIXEL_SHADERS.contains(&h) || BAKE_GEOMETRY_SHADERS.contains(&h)
        }) {
            return true;
        }
        self.depth == 0
            && self.targets.len() >= 4
            && self
                .targets
                .first()
                .and_then(|&t| crate::d3d12::resource::texture_kind(t as This))
                .is_some_and(|(w, h, _)| w >= 512 && w == u64::from(h))
    }

    /// Runs the pass that uploads textures picked by index
    /// ([`State::heap_textures`]) where nothing has asked for it yet
    /// ([`early_budgets`]): once a frame for a few milliseconds, at the
    /// frame's first draw with a view heap set, and with a larger budget
    /// of its own before every draw whose pixel shader is one of the
    /// title's bake programs. For decoration bakes, uploads run synchronously
    /// on a dedicated internal command list and descriptors are published only
    /// after the GPU copy finishes so concurrent `OATHKEEP_THIN=1` lists can
    /// never submit a bake draw ahead of its texture uploads.
    pub(super) fn early_textures(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        let (early_ms, bake_ms) = early_budgets();
        if compute || (early_ms == 0 && bake_ms == 0) {
            return;
        }
        let is_bake = bake_ms > 0 && self.is_bake_draw(p);
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        // The milliseconds this frame's bake draws have taken so far: a
        // frame has several, and together they get `bake_ms`.
        static BAKED_MS: AtomicU64 = AtomicU64::new(0);
        let frame = crate::d3d12::frame::current();
        let first = FRAME.swap(frame, Ordering::Relaxed) != frame;
        if first {
            BAKED_MS.store(0, Ordering::Relaxed);
        }
        if !is_bake && !(first && early_ms > 0) {
            return;
        }
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptors::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return;
        };
        let Some(host) = heap.host() else {
            return;
        };
        if is_bake {
            let began = std::time::Instant::now();
            let mut bake_slot = BAKE_UPLOAD.lock().unwrap_or_else(|e| e.into_inner());
            let gpu = crate::backend::host();
            if bake_slot.is_none()
                && let Some(gpu) = gpu
            {
                match gpu.new_list() {
                    Ok(upload_list) => *bake_slot = Some(BakeList(upload_list)),
                    Err(e) => {
                        tracing::warn!(error = %e, "no host list for synchronous bake uploads")
                    }
                }
            }
            let exact = bake_exact();
            if let (Some(gpu), Some(BakeList(bake_list))) = (gpu, bake_slot.as_ref()) {
                let mut sync = BakeSync { publish: Vec::new(), keep: Vec::new() };
                crate::textures::texture::BAKING.set(exact);
                self.flush_for_bake(bake_list, host, Some(&mut sync));
                let left = bake_ms.saturating_sub(BAKED_MS.load(Ordering::Relaxed));
                let run_pass = left > 0
                    && (first
                        || !exact
                        || {
                            let (writes, noted) = host.texture_news();
                            noted || BAKE_SERVED.load(Ordering::Relaxed) != writes + 1
                        });
                let mut served = false;
                if run_pass {
                    let n = EARLY_TEXTURE_PASSES[1].fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_power_of_two() {
                        let [slots, due, ..] =
                            HEAP_TEXTURES_QUEUE.each_ref().map(|c| c.load(Ordering::Relaxed));
                        tracing::info!(
                            frame,
                            bakes = n,
                            queued_at_the_last_pass = slots,
                            due_at_the_last_pass = due,
                            uploaded_so_far = HEAP_TEXTURES.load(Ordering::Relaxed),
                            "a draw of the title's decoration bake: textures picked by index are uploaded synchronously first (OATHKEEP_BAKE_TEXTURES_MS=0: not)"
                        );
                    }
                    BAKE_SERVED.store(0, Ordering::Relaxed);
                    PASS_BUDGET.set(Some((left, 2 << 30, true)));
                    served = self.heap_textures(bake_list, heap, host, Some(&mut sync));
                    PASS_BUDGET.set(None);
                }
                crate::textures::texture::BAKING.set(false);
                if bake_list.has_recorded_work() {
                    bake_list.close();
                    gpu.execute_sync_internal(bake_list);
                    bake_list.reset();
                } else if !sync.publish.is_empty() {
                    crate::backend::settle_for_fence();
                }
                for (index, handle, key) in sync.publish {
                    let _ = host.publish_uploaded_slot(index, handle, key);
                }
                host.texture_views_remade();
                host.texture_slots_flip();
                drop(sync.keep);
                if run_pass && served && exact {
                    BAKE_SERVED.store(host.texture_news().0 + 1, Ordering::Relaxed);
                }
                BAKED_MS.fetch_add(began.elapsed().as_millis() as u64, Ordering::Relaxed);
                return;
            }
            // Fallback when no host GPU list can be created (e.g. headless tests).
            crate::textures::texture::BAKING.set(exact);
            self.flush_for_bake(list, host, None);
            crate::textures::texture::BAKING.set(false);
            let left = bake_ms.saturating_sub(BAKED_MS.load(Ordering::Relaxed));
            if left == 0 {
                return;
            }
            if !first && exact {
                let (writes, noted) = host.texture_news();
                if !noted && BAKE_SERVED.load(Ordering::Relaxed) == writes + 1 {
                    return;
                }
            }
            EARLY_TEXTURE_PASSES[1].fetch_add(1, Ordering::Relaxed);
            PASS_BUDGET.set(Some((left, 2 << 30, true)));
            crate::textures::texture::BAKING.set(exact);
            let _ = self.heap_textures(list, heap, host, None);
            crate::textures::texture::BAKING.set(false);
            PASS_BUDGET.set(None);
            BAKED_MS.fetch_add(began.elapsed().as_millis() as u64, Ordering::Relaxed);
            return;
        }
        let budget = if first && early_ms > 0 {
            (early_ms, 64 << 20)
        } else {
            return;
        };
        EARLY_TEXTURE_PASSES[0].fetch_add(1, Ordering::Relaxed);
        PASS_BUDGET.set(Some((budget.0, budget.1, false)));
        let _ = self.heap_textures(list, heap, host, None);
        PASS_BUDGET.set(None);
    }
}
