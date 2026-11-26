//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

#[macro_use]
mod staged;

mod buffer_views;
mod diag;
mod flips;
mod heap_flips;
mod heap_init;
mod heap_mirror;
mod heap_raw;
mod heap_slots;
mod heap_textures;
mod kin;
mod listed;
mod make;
mod null_views;
mod queue;
mod sets;
mod spans;

#[cfg(test)]
mod tests;

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;

pub(crate) use buffer_views::{TWINS, move_raw};
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) use diag::{describe_slots, shown_bytes, slot_story};
pub(crate) use diag::{
    OBJECT_TEXTURE_SLOTS, PLACED_TEXTURE_SLOTS, REFUSED_VIEWS, device_removed, is_texture_view,
    near_heaps,
};
pub(crate) use flips::{
    FLIPPED, PendingListFlip, flip_now, levels_changed, levels_changed_after, levels_waiting,
    read_all, read_short, show_after,
};
#[cfg(feature = "diagnostics")]
pub(crate) use kin::pop_causes;
pub(crate) use kin::{KEPT_VIEWS, placed_texture};
pub(crate) use listed::{Listed, list_views, listed_view, note_listed};
pub(crate) use queue::TABLED_WRITES;
pub(crate) use spans::{
    PAGE_WRITES, STALE, first_viewed, next_start, page_written, remapped,
};
pub(crate) use staged::{buffer_copy, buffer_extent, buffer_range, view_address};

use queue::TextureQueue;
use sets::{Made, Slots};
use spans::Spans;

pub(crate) struct HostHeap {
    pub(crate) buffer_sweep: Mutex<crate::draw::BufferSweep>,
    pub(crate) buffer_frame: std::sync::atomic::AtomicU64,
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Slot count the title originally requested (exceeds `count` when capped
    /// by the host's 1,000,000-descriptor limit).
    title_count: u32,
    /// Dynamic slots past `count`: [`DYNAMIC`] in view heaps, none in
    /// sampler heaps. (Upstream falls back to 2^16 on a capped host heap;
    /// ours keeps 2^18 there: 2^16 and 2^17 both ran out in this title.)
    dynamic: u32,
    /// Peak concurrent dynamic slots allocated since heap creation.
    peak_dynamic: std::sync::atomic::AtomicU32,
    /// Highest direct-mapped title slot (`< count`) mirrored so far.
    highest_direct: std::sync::atomic::AtomicU32,
    /// Highest title slot (`>= count`) seen by `mirror` so far (0 if none).
    highest_capped: std::sync::atomic::AtomicU32,
    /// Counts of `mirror` calls for title slots `>= count`:
    /// `[in_dynamic_shadow_zone, at_or_past_null_limit, for_draw_past_count]`.
    capped_mirror: [std::sync::atomic::AtomicU64; 3],
    /// Changed pages of a placed heap that overlapped the capped tail `>= count`
    /// (diagnostic accounting for raw byte writes into per-draw table rings).
    capped_raw_pages: std::sync::atomic::AtomicU64,
    sampler: bool,
    /// Where the raw twins of the title's slots start
    /// (`oathkeep_shader::emit::raw_views`; 0: there are none), and which of
    /// them are not null.
    raw: u32,
    twins: Vec<std::sync::atomic::AtomicBool>,
    /// When each slot was written last (with `crate::book`; else empty).
    #[cfg(feature = "diagnostics")]
    written: Vec<std::sync::atomic::AtomicU32>,
    /// The record (view index and generation) each slot was last made from.
    made: Made,
    /// The same for a slot whose descriptor is made and stays (else 0): what
    /// [`HostHeap::mirror`] asks first, without the lock. Changed under it.
    quick: Vec<std::sync::atomic::AtomicU64>,
    /// The records of the title's heap as the pass over records copied in
    /// as bytes last saw them ([`kin::Copied::changes`]). Made on first
    /// use.
    copied: Mutex<Vec<u8>>,
    /// New ones of them that wait for a draw ([`HostHeap::copied_wait`]),
    /// by the frame they were found in.
    waiting: Mutex<std::collections::VecDeque<(u64, Vec<u32>)>>,
    /// The slots made from a texture whose memory was found away, with what
    /// they were made from and when that was
    /// ([`HostHeap::texture_slot_gone`]).
    away: Mutex<std::collections::HashMap<u32, (u64, u64)>>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    ///
    /// A lock for each slot: one for the heap was what the recording threads
    /// waited for most (5.6 % of their time, profile waits-1: each descriptor
    /// made for a draw takes it, and lets go of what the slot held under it).
    kept: Box<[Mutex<Option<ID3D12Resource>>]>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
    /// How many dynamic slots are free (changed under `next`'s lock): with
    /// fewer than a table needs, the search is not made. Once the world
    /// had taken them all, every later table of the frame searched every
    /// slot before it was refused.
    free: std::sync::atomic::AtomicU32,
    /// The shortest table that found no run of free slots since slots were
    /// last given back (changed under `next`'s lock; `u32::MAX`: none).
    /// Until something is given back the free runs only shrink, so a table
    /// of that length or longer is refused without the search. Enough
    /// slots were free, but in pieces, and every such table walked the
    /// whole ring before it was refused: 0.33 ms each with 131,072 slots,
    /// 1.0 s of the recording threads in a frame that refused 3,143 (run
    /// 45), under the lock all of them need. (A free run that lies across
    /// the place the search starts from is seen by neither.)
    too_long: std::sync::atomic::AtomicU32,
    /// View heaps: the bytes per element of the buffer view at each title
    /// slot, for shaders that pick a buffer by an index they compute
    /// (`oathkeep_shader::emit::SLOT_STRIDES`): an upload buffer of
    /// `SLOT_STRIDES_COUNT` dwords, its GPU address and its mapping.
    strides: Option<(ID3D12Resource, u64, usize)>,
    /// Title slots holding buffer views. Such a view gets its host slot
    /// (and its memory a host copy) only when something reads it: a draw
    /// through a table, or [`HostHeap::buffer_slots`] for shaders that pick
    /// buffers from the whole heap.
    buffers: Slots,
    /// Of those and of the slots that left the set: the ones written since
    /// [`HostHeap::buffer_changes`] last took them.
    changed: Slots,
    /// Title slots holding a view of a texture the title may fill itself (a
    /// placed texture, a texture object), with the frame from which they
    /// are due: such a texture is uploaded when a draw reads it through a
    /// table; one a shader picks by a computed index is brought up to date
    /// from here ([`HostHeap::texture_slots_due`]).
    textures: Mutex<TextureQueue>,
    /// Per title slot, the frame (plus one; 0: never) in which a draw last
    /// read it through a table, or made a table it lies in (with
    /// [`queue::TABLED_READ`] set if the draw read it). The draw that
    /// reads such a slot brings its texture up to date itself and the slot
    /// then leaves `textures`; a write of a slot that was a table's within
    /// the last [`queue::TABLED_FRAMES`] frames is due only [`queue::TABLED_GRACE`]
    /// frames later, by when its draw has read it.
    tabled: Vec<std::sync::atomic::AtomicU32>,
    /// The title memory each placed texture view of this heap describes,
    /// by slot and by address ([`Spans`]): which slots a newer view of
    /// other memory there makes stale, and which to look at again when the
    /// runtime maps, unmaps or writes memory ([`remapped`]).
    placed: std::sync::Arc<Mutex<Spans>>,
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// The size of every host view heap on a host that takes more than
/// [`PORTABLE_VIEWS`] (AMD): the title's heap (a little over 1,000,000
/// views in the retail game) plus [`DYNAMIC`] fits with room to spare, and
/// the last slot is the null slot every view index at or past it reads.
const UNCAPPED_VIEWS: u32 = 1 << 21;

/// Where the raw twins of the title's slots start in a heap that has them
/// ([`raw_twins`]): past every slot a title's heap and the dynamic ones take.
const RAW_TWINS: u32 = 1 << 21;

/// Whether view heaps keep a raw twin of every buffer view of the title's
/// slots, which shaders read that pick buffers by an index they compute
/// (`oathkeep_shader::emit::raw_views` says why; `OATHKEEP_RAW_VIEWS=0`: they
/// read the title's slots themselves, as before). Needs a host that takes
/// heaps of 2^22 views.
fn raw_twins() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_RAW_VIEWS").is_none_or(|v| v != "0"))
}

/// Picks, once, the null slot at the end of every host view heap and tells
/// the translator now (`set_view_heap_limit`) instead of when the title's
/// heap is made: a shader keeps the limit it was translated with, and the
/// title makes 25 pipelines (their shaders translated at once) before it
/// makes its view heap. Called before a shader is translated and before a
/// view heap is made.
pub(crate) fn probe_view_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(host) = host() else { return };
        let fits = |count: u32| {
            // SAFETY: plain heap creation; the heap is released at once.
            let made: windows::core::Result<ID3D12DescriptorHeap> = unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            };
            made.is_ok()
        };
        let twins = raw_twins() && fits(2 * RAW_TWINS);
        if twins {
            oathkeep_shader::emit::set_raw_views(RAW_TWINS);
        }
        // (The twins lie past the null slot.)
        let views = if twins {
            RAW_TWINS
        } else if fits(UNCAPPED_VIEWS) {
            UNCAPPED_VIEWS
        } else {
            PORTABLE_VIEWS
        };
        tracing::info!(
            views,
            raw_twins = twins,
            "host view heaps hold {views} views; view indices are clamped to the null slot at the end from the first shader on"
        );
        oathkeep_shader::emit::set_view_heap_limit(views - 1);
    });
}

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw).
pub(crate) const DYNAMIC: u32 = 1 << 18;

/// Dynamic-slot budget on a [`PORTABLE_VIEWS`] (1,000,000) capped heap.
pub(crate) const CAPPED_DYNAMIC: u32 = DYNAMIC;

fn dynamic_slots(count: u32, null: u32) -> u32 {
    static OVERRIDE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    let env_override = *OVERRIDE.get_or_init(|| {
        std::env::var("OATHKEEP_DYNAMIC_SLOTS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
    });
    dynamic_slots_for(count, null, env_override)
}

fn dynamic_slots_for(count: u32, null: u32, env_override: Option<u32>) -> u32 {
    const MIN_DYNAMIC: u32 = 1 << 16;
    if null == 0 {
        return env_override.unwrap_or(DYNAMIC).clamp(MIN_DYNAMIC, DYNAMIC << 2);
    }
    if null < PORTABLE_VIEWS {
        let max_dyn = (null / 2).max(1);
        let min_dyn = MIN_DYNAMIC.min(max_dyn);
        let default = if count.saturating_add(DYNAMIC) > null {
            CAPPED_DYNAMIC
        } else {
            DYNAMIC
        };
        return env_override.unwrap_or(default).clamp(min_dyn, max_dyn);
    }
    let most = env_override.unwrap_or(DYNAMIC << 2);
    null.saturating_sub(count)
        .min(most)
        .max(DYNAMIC)
        .min(null.saturating_sub(1))
}
