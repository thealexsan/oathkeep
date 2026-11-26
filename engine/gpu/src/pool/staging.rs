use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use super::ranges::{BLOCK, Piece};
use super::{STAGING, lock, now, on};

/// A mapped upload buffer's memory.
#[derive(Clone)]
pub(super) struct Upload(pub(super) ID3D12Resource, pub(super) *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// Cap on 64 MiB staging blocks on limited-RAM hosts (<= 32 GiB): speculative
/// prefetch stops growing at 5 blocks (320 MiB), bakes/draws at 6 (384 MiB),
/// preventing multi-GB PC commit spikes while continuing to reuse free slices.
pub(super) fn staging_block_cap(limited_host_ram: bool, speculative: bool, baking: bool) -> Option<usize> {
    if !limited_host_ram {
        return None;
    }
    if speculative {
        Some(5)
    } else if baking {
        Some(6)
    } else {
        Some(6)
    }
}

/// `bytes` of upload memory, all zeros: the buffer it lies in, where in
/// that buffer, and its mapping. `None`: not pooled.
pub(crate) fn staging(host: &crate::host::Host, bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !on() {
        return None;
    }
    // Copies from a buffer begin at multiples of 512.
    let bytes = bytes.next_multiple_of(512);
    let baking = crate::texture::BAKING.get();
    let speculative = crate::texture::PICKING.get().is_some() && !baking;
    let cap = staging_block_cap(crate::residency::limited_host_ram(), speculative, baking);
    let (piece, memory, again) = {
        let mut pool = lock(&STAGING);
        match pool.take_capped(bytes, 512, now(), cap, || {
            // Only gate speculative bindless prefetch on staging-pool growth
            // headroom; table-bound textures and decoration bakes are sampled
            // immediately and would otherwise read uninitialized/reused VRAM.
            if speculative && !crate::residency::allow_staging_growth(false) {
                return None;
            }
            let (buffer, data) = host.upload_buffer(BLOCK)?;
            crate::host::name(&buffer, || "staging of textures (a block)".into());
            Some(Upload(buffer, data))
        }) {
            Some(taken) => taken,
            None => {
                if speculative && bytes <= BLOCK / 2 && cap.is_some_and(|c| pool.blocks() >= c) {
                    crate::residency::note_staging_deferred();
                }
                return None;
            }
        }
    };
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (an unmapped texture) uploads zeros,
        // as from a buffer of its own.
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    }
    Some((owned, memory.0, piece.start, data))
}
