use super::This;
#[cfg(windows)]
use super::{ensure_state, host_list, read_back::read_back, with_state};
#[cfg(windows)]
pub(super) use super::read_back::resolve;
#[cfg(not(windows))]
pub(super) extern "system" fn resolve(
    this: This,
    dst: This,
    dst_sub: u32,
    src: This,
    src_sub: u32,
    format: u32,
) {
    let _ = (this, dst, dst_sub, src, src_sub, format);
}

#[cfg(windows)]
pub(super) const COPY_DEST: u32 = 0x400;
#[cfg(windows)]
pub(super) const COPY_SOURCE: u32 = 0x800;

/// `len` bytes at `offset` of a title buffer, if they are inside it.
#[cfg(windows)]
pub(super) fn buffer_range(buffer: This, offset: u64, len: u64) -> Option<*mut u8> {
    let (data, size) = crate::resource::buffer_bytes(buffer)?;
    let end = offset.checked_add(len)?;
    if end > size {
        tracing::warn!(offset, len, size, "copy outside its buffer");
        return None;
    }
    // SAFETY: inside the buffer, checked above.
    Some(unsafe { data.add(offset as usize) })
}

/// A copy of title memory that `list` records: on the card at its place in
/// the list ([`crate::host::HostList::place_copy`]), or at submission.
#[cfg(windows)]
fn buffer_copy(this: This, list: &crate::host::HostList, to: *mut u8, from: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    if list.place_copy(to as usize, from as usize, len) {
        with_state(this, |_, state| state.copied());
    } else {
        list.defer_copy(to, from, len);
    }
}

/// Buffer to buffer: both are host memory, copied at submission.
pub(super) extern "system" fn copy_buffer_region(
    this: This,
    dst: This,
    dst_offset: u64,
    src: This,
    src_offset: u64,
    len: u64,
) {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && let Some(to) = buffer_range(dst, dst_offset, len)
        && let Some(from) = buffer_range(src, src_offset, len)
    {
        buffer_copy(this, list, to, from, len as usize);
    }
    #[cfg(not(windows))]
    let _ = (this, dst, dst_offset, src, src_offset, len);
}

/// Whether one of two resources is a target in a stand-in format and the
/// other is not ([`crate::resource::target_format`]): their texels differ
/// in size, so the host cannot copy between them (logged once).
#[cfg(windows)]
pub(super) fn unlike(dst: This, src: This) -> bool {
    let unlike = crate::resource::stands_in(dst) != crate::resource::stands_in(src);
    if unlike {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(?dst, ?src, "copy between a stand-in target and title-format texels skipped")
        });
    }
    unlike
}

/// Copies left out because both sides were one host texture
/// ([`copy_resource`], [`copy_texture_region`]), since the start.
#[cfg(windows)]
pub(crate) static SELF_COPIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) extern "system" fn copy_resource(this: This, dst: This, src: This) {
    #[cfg(windows)]
    {
        let Some(list) = host_list(this) else {
            return;
        };
        if let (Some(to), Some(from)) = (
            crate::resource::host_resource(dst),
            crate::resource::host_resource(src),
        ) {
            if unlike(dst, src) {
                return;
            }
            // Two title textures over the same memory have one host
            // texture between them: the copy would be of that texture onto
            // itself, which the host does not allow, and it would change
            // nothing. It was recorded all the same, with the texture left
            // in the source state as the copy's destination (run 205's
            // debug layer: "CopyResource with its destination in the
            // COPY_SOURCE state", 37 times, a 2560x1440 depth texture).
            // Only the two state changes and the copy are left out: what
            // is noted of the textures stays as it was. After
            // triippywhale 6e502b3b.
            let same = windows::core::Interface::as_raw(to) == windows::core::Interface::as_raw(from);
            // A source the title fills itself is uploaded first (upstream
            // PR #671): a copy read whatever the host texture held.
            if crate::texture::title_written(from) {
                crate::texture::upload(list, src, from);
            }
            // Depth textures shaders also write: what they wrote, first.
            list.twin_use(dst, false, true);
            list.twin_use(src, false, false);
            // A destination made level by level gets its levels' memory
            // ahead of this list (`crate::tiles`).
            crate::texture::card_writes(list, dst, to, None);
            crate::texture::host_written(dst);
            if same {
                SELF_COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::info!(
                        ?dst,
                        ?src,
                        "a copy between two title textures that share one host texture is left out: the first one"
                    );
                });
                return;
            }
            ensure_state(list, dst, COPY_DEST);
            ensure_state(list, src, COPY_SOURCE);
            #[cfg(feature = "diagnostics")]
            list.note_op(|| format!("copy resource {src:?} to {dst:?}"));
            // SAFETY: an open list and live textures.
            unsafe { list.list.CopyResource(to, from) };
        } else if let (Some((to, a)), Some((from, b))) = (
            crate::resource::buffer_bytes(dst),
            crate::resource::buffer_bytes(src),
        ) {
            buffer_copy(this, list, to, from, a.min(b) as usize);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, dst, src);
}

pub(super) extern "system" fn copy_resource_x(this: This, dst: This, src: This, _flags: u32) {
    copy_resource(this, dst, src);
}

/// A copy location as titles pass it, laid out as the host's: a resource,
/// then a subresource index (type 0) or a placed footprint (type 1).
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct CopyLocation {
    pub(super) resource: This,
    pub(super) kind: u32,
    pub(super) footprint: crate::resource::PlacedFootprint,
}

/// Texture to texture on the GPU; buffer to texture through an upload
/// buffer filled at submission. Texture to buffer (read back) is not done
/// yet.
pub(super) extern "system" fn copy_texture_region(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::{D3D12_BOX, D3D12_TEXTURE_COPY_LOCATION};
        use windows::core::Interface;
        const FOOTPRINT: u32 = 1;
        // SAFETY: the title passes both locations (null is tolerated).
        let (Some(list), Some(&dst), Some(&src)) =
            (host_list(this), unsafe { dst.as_ref() }, unsafe {
                src.as_ref()
            })
        else {
            return;
        };
        let Some(to) = crate::resource::host_resource(dst.resource) else {
            // Texture to buffer: a read back. The list copies the texels
            // into a host buffer of ours; once it has run they go to the
            // title's buffer (`HostList::defer_read_back`).
            // The title reads two tiny float targets back every frame (5x1
            // and 1x1 RGBA32F, 80 and 16 bytes: run 90), and read a buffer
            // nobody wrote. `OATHKEEP_READ_BACK=0` leaves them unmade, as
            // before.
            static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let made = read_back(list, dst, x, y, z, src, source_box);
            if n <= 16 || n.is_multiple_of(4096) || made.is_err() {
                static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if made.is_err() && REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 16 {
                    return;
                }
                let source = crate::resource::host_resource(src.resource).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{}x{} f{} mips {}", d.Width, d.Height, d.Format.0, d.MipLevels)
                });
                tracing::info!(
                    copies = n,
                    made = ?made,
                    source = ?source,
                    source_kind = src.kind,
                    destination_kind = dst.kind,
                    destination_offset = dst.footprint.offset,
                    destination_bytes = ?dst.footprint.bytes(),
                    "a texture to buffer copy (a read back)"
                );
            }
            return;
        };
        if unlike(dst.resource, src.resource) {
            return;
        }
        let mut from = src;
        // Both sides are the same subresource of one host texture (set below).
        let mut same = false;
        let _upload; // keeps the source alive while this call records it
        #[cfg(feature = "diagnostics")]
        let mut read = None;
        if src.kind == FOOTPRINT {
            let (Some(host), Some(bytes)) = (crate::host::host(), src.footprint.bytes()) else {
                return;
            };
            let Some(data) = buffer_range(src.resource, src.footprint.offset, bytes) else {
                return;
            };
            let buffer = match list.place_source(data as usize, bytes as usize) {
                // What shaders wrote to it, at this place of the list.
                Some(buffer) => buffer,
                None => {
                    let Some((buffer, staging)) = list.upload(host, bytes) else {
                        return;
                    };
                    // The list owns this mapped D3D12 upload buffer. Wine can
                    // report its address as MEM_FREE, so use the host-upload
                    // path.
                    list.defer_fill_buffer(&buffer, staging, data, bytes as usize);
                    buffer
                }
            };
            from.resource = buffer.as_raw().cast_const().cast();
            from.footprint.offset = 0;
            #[cfg(feature = "diagnostics")]
            {
                // SAFETY: a live buffer.
                read = Some((unsafe { buffer.GetGPUVirtualAddress() }, bytes));
            }
            _upload = buffer;
        } else {
            let Some(texture) = crate::resource::host_resource(src.resource) else {
                return;
            };
            // The same subresource of one host texture on both sides (two
            // title textures over one memory): not a copy the host allows.
            // The states and the copy are left out below; the rest is
            // noted as before.
            same = texture.as_raw() == to.as_raw()
                && src.kind != FOOTPRINT
                && dst.kind != FOOTPRINT
                && src.footprint.offset as u32 == dst.footprint.offset as u32;
            list.twin_use(src.resource, false, false);
            if crate::texture::title_written(texture) {
                crate::texture::upload(list, src.resource, texture);
            }
            if !same {
                ensure_state(list, src.resource, COPY_SOURCE);
            }
            from.resource = texture.as_raw().cast_const().cast();
        }
        // A depth texture shaders also write: what they wrote, first (the
        // source above too).
        list.twin_use(dst.resource, false, true);
        // The first copy into a texture of the title's: what it does not
        // write is zeros, as in a texture that was an allocation of its own
        // (`OATHKEEP_COPY_OVER_TITLE=title`: what title memory holds, which
        // may be another texture's: specks on the waitress's forehead were
        // left in run n04). (The host
        // copy is cut from a block and held whatever lay there, and no
        // upload is made once a copy wrote it: the atlas the title copies
        // its baked decorations into, a 2048x2048 BC3 texture of which a
        // bake writes 256x256 at most, was drawn over a character's face
        // with other textures' bytes wherever nothing was baked, runs n01 and
        // textures-1.)
        static OVER: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let over = *OVER.get_or_init(|| match std::env::var("OATHKEEP_COPY_OVER_TITLE").as_deref() {
            Ok("0") => 0,
            Ok("title") => 1,
            _ => 2,
        });
        if over != 0 && crate::texture::title_written(to) && !crate::texture::host_wrote(dst.resource as usize) {
            // One a list uploaded, or a draw sampled, holds the title's.
            if over == 2 && !list.used(dst.resource as usize) && !crate::texture::is_staged(dst.resource) {
                if let Some(host) = crate::host::host() {
                    ensure_state(list, dst.resource, COPY_DEST);
                    let _ = list.first_use(dst.resource as usize);
                    crate::texture::zeros(list, host, to);
                }
            } else {
                crate::texture::upload(list, dst.resource, to);
            }
            // (Done before the title's copy writes over it.)
            if list.used(dst.resource as usize) {
                ensure_state(list, dst.resource, COPY_SOURCE);
            }
        }
        // A destination made level by level gets the level's memory ahead
        // of this list (`crate::tiles`): the title's bake copies each level
        // of its result into a block-compressed texture of that kind, and
        // a copy into a level without memory is dropped.
        crate::texture::card_writes(list, dst.resource, to, Some(dst.footprint.offset as u32));
        crate::texture::host_written(dst.resource);
        if same {
            SELF_COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        ensure_state(list, dst.resource, COPY_DEST);
        let mut to_location = dst;
        to_location.resource = to.as_raw().cast_const().cast();
        let host_box = (!source_box.is_null()).then(|| source_box.cast::<D3D12_BOX>());
        #[cfg(feature = "diagnostics")]
        {
            let text = || {
                // SAFETY: a live texture; the box is the title's or null.
                let (d, b) = unsafe { (to.GetDesc(), host_box.map(|b| *b)) };
                format!(
                    "copy to {:?} {}x{} f{} sub/kind {} at {x},{y},{z} from {:?} kind {} footprint {:?} box {b:?}",
                    dst.resource,
                    d.Width,
                    d.Height,
                    d.Format.0,
                    dst.kind,
                    src.resource,
                    src.kind,
                    (src.kind == FOOTPRINT).then_some(src.footprint)
                )
            };
            match read {
                Some(range) => list.note_read(text, range),
                None => list.note_op(text),
            }
        }
        const _: () =
            assert!(size_of::<CopyLocation>() == size_of::<D3D12_TEXTURE_COPY_LOCATION>());
        // SAFETY: an open list; both locations in the host's layout, naming
        // live resources; the box is the title's (same layout).
        unsafe {
            list.list.CopyTextureRegion(
                (&raw const to_location).cast(),
                x,
                y,
                z,
                (&raw const from).cast(),
                host_box,
            )
        };
    }
    #[cfg(not(windows))]
    let _ = (this, dst, x, y, z, src, source_box);
}

#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(super) extern "system" fn copy_texture_region_x(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
    _flags: u32,
) {
    copy_texture_region(this, dst, x, y, z, src, source_box);
}

/// `CopyMemoryToMemoryX(destination, source, size, flags)`: GPU addresses
/// are title memory, copied at submission like buffer copies (if mapped).
pub(super) extern "system" fn copy_memory(this: This, dst: u64, src: u64, len: u64, _flags: u32) {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && dst != 0
        && src != 0
        && len != 0
        && dst != src
    {
        let len = len as usize;
        if list.place_copy(dst as usize, src as usize, len) {
            with_state(this, |_, state| state.copied());
            return;
        }
        let non_overlapping = (dst as usize).saturating_add(len) <= src as usize
            || (src as usize).saturating_add(len) <= dst as usize;
        if non_overlapping {
            // Use `Step::Copy` so `card_steps` can offload it and CPU fallback
            // preserves `watch::ask_once` via range-scoped `watch::wrote`
            // instead of nuking the submission journal via `Step::Run`.
            list.defer_copy(dst as *mut u8, src as *const u8, len);
            return;
        }
        list.defer(move || {
            if crate::texture::committed(src as *const u8, len)
                && crate::texture::committed(dst as *const u8, len)
            {
                // SAFETY: both ranges are committed title memory (overlapping `memmove`).
                unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
            }
        });
    }
    #[cfg(not(windows))]
    let _ = (this, dst, src, len);
}
