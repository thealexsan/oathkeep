//! Recheck fingerprint and placed-texture mapping probes for [`super::State::heap_textures`].

use std::sync::atomic::Ordering;

use crate::descriptors::View;
use crate::d3d12::object::This;

use super::super::bake::recheck_soon;
use super::{
    HEAP_TEXTURES_AWAY, HEAP_TEXTURES_PARTLY, HEAP_TEXTURES_RECHECKED, RECHECK_FRAMES,
};

pub(super) struct SlotProbe {
    pub(super) again_in: u64,
    pub(super) partly: Option<u64>,
    pub(super) there: Option<u64>,
}

/// Checks whether a due slot is unchanged on recheck, unmapped/stalled in
/// placed memory, or a host-written texture object. Returns `None` when the
/// slot needs no upload in this pass.
pub(super) fn probe_slot(
    heap: &crate::descriptors::Heap,
    host: &crate::descriptors::host_heap::HostHeap,
    frame: u64,
    most: u64,
    index: u32,
    write: u64,
) -> Option<SlotProbe> {
    // A placed texture whose memory is not mapped (not streamed in
    // yet, or streamed out again: the heap keeps tens of thousands
    // of such views) gets no host texture; looked at again soon (a
    // title writes a texture's view before its pages are mapped,
    // and the pass runs early in a frame). One of which some is
    // mapped (a texture streaming in: its smaller mips lie at the
    // start of its memory and arrive first) is uploaded with what
    // is there, and again when more is. (It was uploaded again
    // every 120 frames, changed or not; and asked by its first
    // page alone, one whose first page is the one missing counted
    // as not there.)
    // A slot uploaded whole is looked at again a few times
    // (`RECHECK_FRAMES`): the title writes a texture's view, and
    // may fill the texture's memory itself only afterwards, with
    // no descriptor write and nothing from the runtime to tell of
    // it. The host copy then kept what the memory held at the
    // first look for the rest of the run. The look reads 48 KiB;
    // only a changed texture is uploaded again.
    // `OATHKEEP_HEAP_RECHECK=0`: uploaded once, as before.
    // One found changed by such a look is looked at in the next frame
    // again: the textures left that change unnoticed are ones the
    // title paints all the time through another view of their pages
    // (all their blocks are shown at other addresses too), and each
    // was shown four frames late, over and over.
    let mut again_in = RECHECK_FRAMES[0];
    if let Some((at, len, print, looks, uploaded)) = host.texture_slot_checked(index) {
        if crate::textures::texture::fingerprint(at, len) == print {
            HEAP_TEXTURES_RECHECKED[0].fetch_add(1, Ordering::Relaxed);
            match RECHECK_FRAMES.get(usize::from(looks) + 1) {
                Some(wait) => host.texture_slot_recheck(
                    index,
                    write,
                    frame + wait,
                    (at, len),
                    print,
                    looks + 1,
                    uploaded,
                ),
                None => host.texture_slot_done(index, write, None),
            }
            return None;
        }
        if recheck_soon() {
            again_in = 1;
        }
        let n = HEAP_TEXTURES_RECHECKED[1].fetch_add(1, Ordering::Relaxed);
        if n < 200 || n.is_multiple_of(1000) {
            // Whether its pages are the title's at other addresses
            // too: what is written there is told of there alone.
            let elsewhere =
                crate::memory::inplace::shown_elsewhere(at as usize & !0xffff, (at + len) as usize);
            let first =
                heap.handle(index)
                    .and_then(crate::descriptors::view)
                    .map_or(0, |view| match view {
                        View::Placed(false, _, desc) => crate::d3d12::resource::first_mip(&desc),
                        _ => 0,
                    });
            tracing::warn!(
                first_level = first,
                blocks_shown_elsewhere = elsewhere.len(),
                slot = index,
                address = format_args!("{at:#x}"),
                bytes = len,
                frames_after_upload = frame.saturating_sub(uploaded),
                looks,
                changed = n + 1,
                "a texture picked by index changed after its upload with nothing to tell of it: uploaded again"
            );
        }
    }
    let mut partly = None;
    let mut there = None;
    if let Some(View::Placed(false, resource, desc)) =
        heap.handle(index).and_then(crate::descriptors::view)
    {
        let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
        let first = crate::d3d12::resource::first_mip(&desc);
        let len = crate::d3d12::resource::placed_bytes_from(&resource, first).unwrap_or(1);
        let mapped = crate::textures::texture::mapped_bytes(address as *const u8, len as usize);
        if mapped == 0 {
            HEAP_TEXTURES_AWAY.fetch_add(1, Ordering::Relaxed);
            // Streamed out: what the slot showed is let go.
            host.texture_slot_gone(index);
            host.texture_slot_later(index, write, frame, most);
            return None;
        }
        host.texture_slot_here(index);
        there = Some(mapped);
        if mapped < len {
            if mapped <= host.texture_slot_mapped(index) {
                HEAP_TEXTURES_PARTLY[0].fetch_add(1, Ordering::Relaxed);
                host.texture_slot_later(index, write, frame, most);
                return None;
            }
            if !crate::textures::texture::committed(address as *const u8, 1) {
                HEAP_TEXTURES_PARTLY[2].fetch_add(1, Ordering::Relaxed);
            }
            partly = Some(mapped);
        }
    }
    // A texture object the host renders to or writes holds the
    // host's own contents: nothing to bring up to date, and this
    // pass is no use of it (its host copy is made when something
    // uses it, and its slots then: `descriptor::make_waiting`).
    if let Some(View::Shader(object, _)) = heap.handle(index).and_then(crate::descriptors::view)
        && crate::d3d12::resource::buffer_bytes(object as This).is_none()
        && !crate::d3d12::resource::title_fills(object as This)
    {
        host.texture_slot_done(index, write, None);
        return None;
    }
    Some(SlotProbe { again_in, partly, there })
}
