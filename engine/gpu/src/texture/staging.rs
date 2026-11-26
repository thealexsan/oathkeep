use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use super::{BAKING, Digest, PICKING, STAGED, STAGING, committed, draw_active_recently, once};
use crate::host::host;
use crate::object::This;

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    /// A buffer of its own, or the block `Staging::_piece` is part of.
    pub(super) buffer: ID3D12Resource,
    /// Where in `buffer` it begins (the footprints count from there).
    pub(super) base: u64,
    /// Back in the pool with the last list that names it.
    _piece: Option<crate::pool::StagingPiece>,
    /// Its bytes.
    #[cfg(feature = "diagnostics")]
    pub(super) size: u64,
    pub(super) data: *mut u8,
    pub(super) footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    pub(super) rows: Vec<u32>,
    pub(super) layout: oathkeep_formats::texture::Layout,
    /// Mips per array slice, of the title's texture.
    pub(super) mips: u32,
    /// The first of them that is read and uploaded
    /// (`crate::resource::held_only`), the one the host copy begins at, and
    /// the bytes of title memory read.
    pub(super) first: u32,
    pub(super) level0: u32,
    pub(super) read: usize,
    /// Which host copy it uploads into, and what that is told to hold.
    pub(super) serial: u64,
    pub(super) holds: Arc<crate::resource::Holds>,
    /// Made by the pass over textures picked by index, for one upload
    /// (`once`): read when the list is submitted, and never looked at again.
    pub(super) once: std::sync::atomic::AtomicBool,
    /// The staging buffer's bytes (counted in `STAGED` while it lives).
    pub(super) bytes: u64,
    pub(super) memory: *const u8,
    pub(super) last: Mutex<Last>,
    /// Whether the staging buffer was detiled since a submitted list last
    /// uploaded it, and whether one has uploaded it at all: a list records
    /// the upload only then.
    pub(super) dirty: std::sync::atomic::AtomicBool,
    pub(super) uploaded: std::sync::atomic::AtomicBool,
    /// When a list last recorded a use of it (`crate::host::moment`).
    pub(super) used: std::sync::atomic::AtomicU64,
    /// How many bytes of the texture's memory were mapped when a list last
    /// recorded an upload of it (`fill_unlocked`; 0: not asked).
    pub(super) expected: std::sync::atomic::AtomicU64,
    /// Levels on demand (`crate::tiles`): the texture's host copy was made
    /// without memory of its own when the staging buffer was made.
    pub(super) on_demand: bool,
    /// For such a texture: the levels that were the title's at the last
    /// detiling (a bit a level; the staging buffer holds zeros for the
    /// others), and what that was judged by (`Staging::judgement`).
    pub(super) detiled: std::sync::atomic::AtomicU32,
    pub(super) judged: [std::sync::atomic::AtomicU64; 2],
    /// For such a texture: when an upload last left a held level without
    /// tiles (`crate::host::moment`, never 0; 0: none did).
    pub(super) waiting: std::sync::atomic::AtomicU64,
    /// Recorded for a one-shot decoration bake (`BAKING`): submission-time
    /// refresh must wait for title memory to stop remapping.
    pub(super) exact: std::sync::atomic::AtomicBool,
    /// Levels on demand: whether high mips were temporarily clamped under
    /// VRAM pressure so the slot upgrades automatically once room returns.
    pub(super) clamped_mips: std::sync::atomic::AtomicBool,
    /// Title resource pointer (`This as usize`) for aliased-mutation tracking.
    pub(super) resource: usize,
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
pub(super) struct Last {
    pub(super) hash: Option<Digest>,
    pub(super) asked: u64,
    /// The frame a partly mapped texture was last read in.
    pub(super) partial: Option<u64>,
    /// Until this frame a guarded texture is hashed at every check.
    pub(super) hot_until: u64,
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
    }
}

/// Whether a staging buffer made for one upload of a texture picked by index
/// is filled without asking the write watch and the guards
/// (`OATHKEEP_TEXTURE_ONCE=0`: with, as before).
fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_ONCE").is_none_or(|v| v != "0"))
}

impl Staging {
    /// Levels on demand: what says, beside the mapping, which levels of
    /// the texture are the title's now.
    pub(super) fn judgement(&self) -> [u64; 2] {
        let start = self.memory as u64;
        let first = crate::descriptor::host::first_viewed(start).map_or(0, u64::from);
        let limit = crate::descriptor::host::next_start(start, start.saturating_add(self.layout.size))
            .map_or(u64::MAX, |at| at - start);
        [first, limit]
    }

    /// What [`Staging::judgement`] said at the last detiling.
    pub(super) fn judged(&self) -> [u64; 2] {
        use std::sync::atomic::Ordering::Relaxed;
        [self.judged[0].load(Relaxed), self.judged[1].load(Relaxed)]
    }

    /// Levels on demand: the levels that are the title's now, a bit a level.
    pub(super) fn held_levels(&self, [first, limit]: [u64; 2]) -> u32 {
        let mips = &self.layout.mips;
        if self.layout.slices != 1 || mips.is_empty() || mips.len() > 32 {
            return u32::MAX;
        }
        let tail = (self.layout.first_tail_mip as usize).min(mips.len());
        let there = |from: u64, to: u64| {
            to > from && to <= limit && committed(self.memory.wrapping_add(from as usize), (to - from) as usize)
        };
        let mut held = 0u32;
        for (level, m) in mips.iter().enumerate().take(tail) {
            let end = if level == 0 { self.layout.slice_size } else { mips[level - 1].offset };
            if level as u64 >= first && there(m.offset, end) {
                held |= 1 << level;
            }
        }
        if tail < mips.len() && there(0, self.layout.alignment.max(1).min(self.layout.slice_size)) {
            for level in tail..mips.len() {
                if level as u64 >= first {
                    held |= 1 << level;
                }
            }
        }
        held
    }

    /// Whether `refresh()` has populated valid title bytes into this staging
    /// buffer (false when pages were transiently unmapped during remap).
    pub(crate) fn has_data(&self) -> bool {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).hash.is_some()
    }

    /// Forgets what the staging buffer was last filled from: the next
    /// refresh detiles anew, whether the memory changed or not.
    pub(super) fn forget(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Last::default();
    }

    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.read)
    }

    /// The title's level and the array slice of the staging buffer's part
    /// `i`.
    pub(super) fn part(&self, i: usize) -> (u32, u32) {
        let mips = self.mips - self.first;
        (self.first + i as u32 % mips, i as u32 / mips)
    }

    /// The host copy holds what the staging buffer was filled from.
    pub(crate) fn note_held(&self) {
        let hash = self.last.lock().unwrap_or_else(|e| e.into_inner()).hash;
        let prev = {
            let mut slot = self.holds.lock().unwrap_or_else(|e| e.into_inner());
            let prev = *slot;
            *slot = hash.map(|hash| (self.first, hash));
            prev
        };
        let mutated = match (prev, hash) {
            (Some((_, old)), Some(new)) => old != new,
            _ => false,
        };
        if hash.is_some() && self.once.load(std::sync::atomic::Ordering::Relaxed) {
            let start = self.memory as usize;
            let end = start.saturating_add(self.read);
            let aliased = mutated || !crate::inplace::shown_elsewhere(start & !0xffff, end).is_empty();
            if aliased {
                let print = super::aliased::fingerprint(start as u64, self.read as u64);
                super::aliased::watch(
                    self.resource,
                    start as u64,
                    self.read as u64,
                    print,
                    Arc::downgrade(&self.holds),
                );
            }
        }
    }

    pub(super) fn movie_plane(&self) -> bool {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8_UNORM;
        let desc = &self.layout.desc;
        !self.on_demand
            && desc.dimension == oathkeep_formats::texture::Dimension::Tex2d
            && desc.swizzle.0 == 0
            && desc.depth == 1
            && desc.mips == 1
            && desc.samples == 1
            && self.footprints.len() == 1
            && self.footprints[0].Offset == 0
            && self.footprints[0].Footprint.Format == DXGI_FORMAT_R8_UNORM
    }
}

/// Clamps high mip levels for speculative on-demand textures under VRAM
/// pressure so visible surfaces receive mip-tail / lower mips instead of
/// starving on `neutral()`. Never drops every available mip.
pub(super) fn clamp_speculative_mips(
    held: u32,
    first_tail_mip: u32,
    mips_len: usize,
    speculative: bool,
    vram: crate::residency::VramLevel,
) -> u32 {
    if held == 0 || !speculative || mips_len <= 1 {
        return held;
    }
    match vram {
        crate::residency::VramLevel::Normal => held,
        crate::residency::VramLevel::High => {
            // Defer mip 0 (~75% of texture VRAM) when smaller mips are mapped.
            let without_top = held & !1u32;
            if without_top != 0 { without_top } else { held }
        }
        crate::residency::VramLevel::Critical => {
            // Keep the packed mip tail plus one level above it so surfaces stay
            // textured with minimal tile usage until VRAM recovers.
            let keep_from = first_tail_mip.saturating_sub(1).min(mips_len.saturating_sub(1) as u32).min(31);
            let mask = !((1u32 << keep_from) - 1);
            let clamped = held & mask;
            if clamped != 0 {
                clamped
            } else {
                // Fall back to the smallest mapped level so we never return 0.
                1u32 << (31 - held.leading_zeros())
            }
        }
    }
}

/// The staging state of a title texture, made on first use.
pub(crate) fn staging(resource: This, copy: &crate::resource::HostTexture, first: u32) -> Option<Arc<Staging>> {
    let texture = &copy.texture;
    // Asked before the table is locked: the sweep lets go of a staging
    // buffer with its texture's host side locked, in that order.
    let on_demand = crate::resource::host_tiled(resource).is_some();
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize))
        && s.first <= first.max(copy.base)
        && s.serial == copy.serial
    {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let level0 = copy.base;
    let mips = level0 + u32::from(d.MipLevels);
    let first = crate::resource::read_from(&layout, first).max(level0).min(mips - 1);
    let read = layout.bytes_from(first) as usize;
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        mips - first
    } else {
        (mips - first) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            first - level0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (piece, buffer, base, data) = match crate::pool::staging(host, total) {
        Some((piece, buffer, base, data)) => (Some(piece), buffer, base, data),
        None => {
            let speculative = PICKING.get().is_some() && !BAKING.get();
            if crate::residency::upload_refused()
                || crate::residency::is_staging_deferred()
                || (speculative
                    && !crate::residency::allow_speculative_staging(total, draw_active_recently()))
            {
                crate::residency::note_staging_deferred();
                return None;
            }
            let (buffer, data) = host.upload_buffer(total)?;
            crate::host::name(&buffer, || {
                format!(
                    "staging of {resource:?} {}x{}x{} f{}",
                    d.Width, d.Height, d.DepthOrArraySize, d.Format.0
                )
            });
            (None, buffer, 0, data)
        }
    };
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let s = Arc::new(Staging {
        buffer,
        base,
        _piece: piece,
        #[cfg(feature = "diagnostics")]
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips,
        first,
        level0,
        read,
        serial: copy.serial,
        holds: copy.holds.clone(),
        once: std::sync::atomic::AtomicBool::new(once() && !on_demand && PICKING.get().is_some()),
        bytes: total,
        memory,
        last: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(true),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        expected: std::sync::atomic::AtomicU64::new(0),
        on_demand,
        detiled: std::sync::atomic::AtomicU32::new(0),
        judged: [const { std::sync::atomic::AtomicU64::new(0) }; 2],
        waiting: std::sync::atomic::AtomicU64::new(0),
        exact: std::sync::atomic::AtomicBool::new(BAKING.get()),
        clamped_mips: std::sync::atomic::AtomicBool::new(false),
        resource: resource as usize,
    });
    map.insert(resource as usize, s.clone());
    Some(s)
}
