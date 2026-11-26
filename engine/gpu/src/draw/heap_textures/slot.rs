//! Per-slot upload, recheck, reuse, and staging step for [`super::State::heap_textures`].

use std::sync::atomic::{AtomicU64, Ordering};

use oathkeep_shader::ResourceType;

use crate::descriptor::View;
use crate::host::HostList;
use crate::object::This;

use super::super::bake::BakeSync;
use super::super::readable::readable;
use super::probe::{SlotProbe, probe_slot};
use super::{
    HEAP_TEXTURE_BYTES, HEAP_TEXTURES, HEAP_TEXTURES_PARTLY, HEAP_TEXTURES_REUPLOADED_FULL,
    HEAP_TEXTURES_SAME, HEAP_TEXTURES_TABLED, SPENT, TILE_REUSE_FLOOR, full_card_pass,
    heap_recheck, reupload_when_full,
};

pub(super) struct PassState {
    pub(super) frame: u64,
    pub(super) most: u64,
    pub(super) baking: bool,
    pub(super) required: bool,
    pub(super) room: bool,
    /// Bytes uploaded by this pass since the card was last asked
    /// whether it has room: asked again every 32 MiB (a pass may upload
    /// 192 MB and more, and one asking a pass let the queue run some
    /// 400 MB past its line while the world loaded: run 193).
    pub(super) since_asked: u64,
    pub(super) refused: usize,
}

pub(super) enum SlotStep {
    Continue,
    Break,
    StopUnserved,
    DeviceRemoved,
}

pub(super) fn process_slot(
    list: &HostList,
    heap: &crate::descriptor::Heap,
    host: &crate::descriptor::host::HostHeap,
    pass: &mut PassState,
    mut sync: Option<&mut BakeSync>,
    index: u32,
    write: u64,
) -> SlotStep {
    let frame = pass.frame;
    let most = pass.most;
    let baking = pass.baking;
    let required = pass.required;
    let Some(SlotProbe { again_in, partly, there }) =
        probe_slot(heap, host, frame, most, index, write)
    else {
        return SlotStep::Continue;
    };
    // From here on a host texture is made: not without room. The
    // slot stays due.
    // Not so a slot already made from this very view: its host
    // texture exists, and uploading it again takes none of the
    // card's memory. It was refused all the same, so a texture the
    // title filled after its first upload kept its empty host copy
    // from the moment the card reached the line, which is the first
    // world frame in eight launches of nine: the same slots were
    // found changed again at every pass and never uploaded (run
    // 179: 33,000 such findings), and the main character's face
    // had its colour only in the launches that entered the world
    // under the line. `OATHKEEP_REUPLOAD_WHEN_FULL=0`: refused, as
    // before.
    let mut reuse_only = false;
    if !pass.room {
        let has_its_texture = reupload_when_full()
            && heap
                .handle(index)
                .is_some_and(|handle| host.texture_slot_made_from(index, handle));
        if !has_its_texture {
            let can_reuse = crate::pool::texture_room() >= (32 << 20)
                || crate::tiles::free_room() >= TILE_REUSE_FLOOR;
            if !can_reuse {
                pass.refused += 1;
                if full_card_pass() {
                    // Held back for the frames of about four seconds
                    // (`most`), or until the card has room; the pass
                    // goes on, within a count that bounds one pass.
                    host.texture_slot_held(index, write, frame + most);
                    if pass.refused > 1024 {
                        return SlotStep::Break;
                    }
                } else if pass.refused > 256 {
                    return SlotStep::Break;
                }
                return SlotStep::Continue;
            }
            reuse_only = true;
        } else {
            let n = HEAP_TEXTURES_REUPLOADED_FULL.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n.is_multiple_of(500) {
                tracing::info!(
                    so_far = n,
                    slot = index,
                    "a texture already on the card is brought up to date although the card has no room for new ones"
                );
            }
        }
    }
    // Gate new host texture creation / staging on commit reserve after
    // `mapped == 0` reclamation (`texture_slot_gone`) has run, and
    // before mirroring a slot to an unuploaded texture.
    if !crate::residency::admit_texture_pass(required) {
        return SlotStep::StopUnserved;
    }
    // A slot that shows the texture by an earlier view of it (one
    // from another level) goes on doing so until the upload of what
    // the new one shows has run.
    let sync_bake = sync.is_some();
    let shows_other = !baking
        && !sync_bake
        && crate::resource::held_only()
        && heap
            .handle(index)
            .is_some_and(|handle| host.texture_slot_shows_other(index, handle));
    // A slot that shows nothing of it yet, or something else, shows it
    // when the upload has run too.
    let late = !shows_other
        && !baking
        && !sync_bake
        && crate::descriptor::host::show_after()
        && heap
            .handle(index)
            .is_some_and(|handle| !host.texture_slot_made_from(index, handle));
    let defer_mirror = reuse_only && !sync_bake && !shows_other && !late;
    let (view, bake_key) = if sync_bake {
        match heap
            .handle(index)
            .and_then(|handle| host.prepare_bake_slot(index, handle))
        {
            Some((view, key)) => (Some(view), Some(key)),
            None => (None, None),
        }
    } else {
        let view = heap.handle(index).and_then(|handle| {
            if shows_other || late || defer_mirror {
                crate::descriptor::view(handle)
            } else {
                host.mirror(index, handle, true)
            }
        });
        (view, None)
    };
    let at_once = || {
        if (late || defer_mirror || bake_key.is_some())
            && let Some(handle) = heap.handle(index)
        {
            let _ = host.mirror(index, handle, true);
        }
    };
    let placed = view.as_ref().and_then(crate::descriptor::host::placed_texture);
    if crate::residency::upload_refused() {
        return SlotStep::StopUnserved;
    }
    let resource = match placed.as_ref().or(view.as_ref()) {
        Some(&View::Shader(resource, _)) => resource as This,
        _ => {
            at_once();
            host.texture_slot_done(index, write, None);
            return SlotStep::Continue;
        }
    };
    let texture = if reuse_only {
        crate::pool::with_no_grow(|| crate::resource::host_resource(resource))
    } else {
        crate::resource::host_resource(resource)
    };
    if crate::residency::upload_refused() {
        return SlotStep::StopUnserved;
    }
    if reuse_only && texture.is_none() {
        pass.refused += 1;
        if full_card_pass() {
            host.texture_slot_held(index, write, frame + most);
            if pass.refused > 1024 {
                return SlotStep::Break;
            }
        } else if pass.refused > 256 {
            return SlotStep::Break;
        }
        return SlotStep::Continue;
    }
    if reuse_only {
        static REUSED: AtomicU64 = AtomicU64::new(0);
        let n = REUSED.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n.is_multiple_of(2000) {
            tracing::info!(
                so_far = n,
                slot = index,
                baking,
                pool_room_mb = crate::pool::texture_room() >> 20,
                tiles_free_mb = crate::tiles::free_room() >> 20,
                "new texture admitted into existing pool holes without growing VRAM"
            );
        }
    }
    if defer_mirror && let Some(handle) = heap.handle(index) {
        let _ = host.mirror(index, handle, true);
    }
    let title_fills = texture.is_some_and(crate::texture::title_written);
    if !title_fills {
        at_once();
        // a render target, a depth or unordered access texture: the host's own
        host.texture_slot_done(index, write, None);
        return SlotStep::Continue;
    }
    // A texture that has a staging buffer now is one draws read
    // through a table: they upload it, and check it at every
    // submission that samples it.
    let tabled = crate::texture::is_staged(resource);
    // The host copy holds these very bytes: nothing to upload.
    let first = match (&placed, &view) {
        (Some(_), Some(View::Placed(_, _, desc))) => crate::resource::first_mip(desc),
        _ => 0,
    };
    let same = !tabled && crate::texture::unchanged(resource, first);
    if same {
        HEAP_TEXTURES_SAME.fetch_add(1, Ordering::Relaxed);
        if let (Some(sync), Some(key), Some(handle)) =
            (sync.as_deref_mut(), bake_key, heap.handle(index))
        {
            if let Some(copy) = crate::resource::host_copy(resource) {
                sync.keep.push(copy);
            }
            sync.publish.push((index, handle, key));
        } else {
            if shows_other && let Some(handle) = heap.handle(index) {
                let _ = host.mirror(index, handle, true);
            }
            // Its bytes are in, or on the way there in a list before.
            if late
                && texture.is_some_and(crate::pool::fresh)
                && !crate::texture::ahead::there(resource)
            {
                if let Some(handle) = heap.handle(index) {
                    host.texture_slot_show_after(list, index, handle);
                }
            } else {
                at_once();
            }
        }
        let again = partly.map(|mapped| (frame + 1, mapped));
        let memory = if again.is_none() && heap_recheck() {
            crate::texture::title_span(resource, first)
        } else {
            None
        };
        match memory {
            Some((at, len)) => {
                let print = crate::texture::fingerprint(at, len);
                host.texture_slot_recheck(index, write, frame + again_in, (at, len), print, 0, frame);
            }
            None => host.texture_slot_done(index, write, again),
        }
        return SlotStep::Continue;
    }
    // The first upload of a texture no list has used: on a queue of
    // its own, and the slot shows it as soon as that has run.
    let ahead = if late && !tabled && texture.is_some_and(crate::pool::fresh) {
        crate::texture::ahead::take(
            resource,
            first,
            there.filter(|_| partly.is_none()).unwrap_or(u64::MAX),
        )
    } else {
        None
    };
    if let Some(ahead) = &ahead {
        use crate::texture::ahead::Taken;
        let (Taken::New(job) | Taken::Known(job)) = ahead;
        if let Some(handle) = heap.handle(index) {
            host.texture_slot_show_ahead(job, index, handle);
        }
        if let Taken::New(job) = ahead {
            crate::texture::ahead::send(job);
        }
    }
    if ahead.is_none() && crate::residency::upload_refused() {
        return SlotStep::StopUnserved;
    }
    let late = late && ahead.is_none();
    let _ = crate::residency::take_staging_deferred();
    if let Some(view) = &view
        && ahead.is_none()
    {
        // (What was found mapped of it a moment ago is not asked
        // again, unless the host copy may need more.)
        crate::texture::PICKING.set(Some(there.filter(|_| partly.is_none()).unwrap_or(u64::MAX)));
        readable(list, ResourceType::Srv, view);
        crate::texture::PICKING.set(None);
    }
    // A failed or deferred staging allocation leaves this slot due.
    // Publishing it or marking it served would lose the only retry for
    // some materials.
    if ahead.is_none()
        && (crate::residency::take_staging_deferred() || crate::residency::upload_refused())
    {
        return SlotStep::StopUnserved;
    }
    if let (Some(sync), Some(key), Some(handle)) =
        (sync.as_deref_mut(), bake_key, heap.handle(index))
    {
        if let Some(copy) = crate::resource::host_copy(resource) {
            sync.keep.push(copy);
        }
        sync.publish.push((index, handle, key));
    }
    if shows_other && let Some(handle) = heap.handle(index) {
        host.texture_slot_flip(list, index, handle);
    }
    if late && let Some(handle) = heap.handle(index) {
        host.texture_slot_show_after(list, index, handle);
    }
    #[cfg(feature = "diagnostics")]
    if crate::book::on()
        && let Some(texture) = texture.filter(|t| crate::pool::fresh(t))
    {
        use windows::core::Interface;
        let raw = texture.as_raw() as usize;
        let asked = crate::host::submissions();
        list.defer_host(move || {
            let (started, finished) = crate::host::submissions();
            crate::book::note(raw, || {
                format!(
                    "; upload for slot {index} recorded at submissions {asked:?}, sent as submission {started} (finished {finished}) at {:.3} s",
                    crate::host::now_ms() as f64 / 1e3
                )
            });
        });
    }
    // A texture view the host refuses removes the device: name it.
    static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !GONE.load(Ordering::Relaxed)
        && let Some(why) = crate::descriptor::host::device_removed()
    {
        GONE.store(true, Ordering::Relaxed);
        let text: String = format!("{view:?}").chars().take(300).collect();
        tracing::error!(
            why,
            slot = index,
            "the host device went while textures picked by index were made ready; last: {text}"
        );
        crate::host::log_removal_context();
        return SlotStep::DeviceRemoved;
    }
    let (bytes, whole) = crate::texture::staged(resource, there.is_none()).unwrap_or((0, true));
    // Not all of it there: looked at again next frame, and from
    // then on as a slot with nothing new (a placed texture: until
    // more of it is mapped; a texture object at an address the
    // title mapped: again in 120 frames, as before).
    let again = match partly {
        Some(mapped) => Some((frame + 1, mapped)),
        None => (!whole).then_some((frame + 120, 0)),
    };
    if again.is_some() {
        HEAP_TEXTURES_PARTLY[1].fetch_add(1, Ordering::Relaxed);
    }
    if tabled {
        // Nothing was staged here, so nothing is counted or spent,
        // and the staging buffer stays: taking it away made the
        // next draw that reads the texture through a table stage,
        // detile and upload all of it once more.
        HEAP_TEXTURES_TABLED.fetch_add(1, Ordering::Relaxed);
        host.texture_slot_done(index, write, again);
        return SlotStep::Continue;
    }
    // Uploaded: the list holds the staging buffer until it has run;
    // a table read, or the next look at a texture not all mapped
    // yet, makes it anew. (Kept for those until now: every partly
    // streamed texture of the heap held a staging buffer of its full
    // size for good.)
    let memory = if again.is_none() && heap_recheck() {
        crate::texture::staged_memory(resource)
    } else {
        None
    };
    crate::texture::unstage(resource);
    SPENT.fetch_add(bytes, Ordering::Relaxed);
    pass.since_asked += bytes;
    if pass.room && pass.since_asked >= 32 << 20 {
        pass.since_asked = 0;
        pass.room = crate::resource::room(crate::host::host().and_then(|h| h.video_memory()));
    }
    HEAP_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
    HEAP_TEXTURES.fetch_add(1, Ordering::Relaxed);
    match memory {
        // Whole and uploaded: looked at again (see above).
        Some((at, len)) => {
            let print = crate::texture::fingerprint(at, len);
            host.texture_slot_recheck(index, write, frame + again_in, (at, len), print, 0, frame);
        }
        None => host.texture_slot_done(index, write, again),
    }
    SlotStep::Continue
}
