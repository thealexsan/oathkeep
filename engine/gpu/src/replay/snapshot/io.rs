//! One frame written down: what its submissions run, what the card's
//! memory held at its start, and what the CPU put into memory both see
//! before each submission.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::os::windows::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::gpu::{self, Seen, Worker};
use super::super::recorder::{At, Heap, Ours, Recorder, Res, lock, not_recorded_calls, read};
use super::super::states::{COMMON, COPY_SOURCE, Change, States};
use super::super::wire::{
    self, ALL, Cmd, Content, Delta, Loc, MISSING, NONE, Place, Slot, Va, ZERO, slot,
};

use super::*;

/// A part of a resource in a read-back buffer, `at` bytes in.
pub(super) struct Piece {
    pub(super) res: Arc<Res>,
    pub(super) first: u64,
    pub(super) count: u32,
    pub(super) len: u64,
    pub(super) at: u64,
}

/// Records the copies of `pieces` into `to`. Textures are moved to a state
/// they can be copied from and back.
pub(super) fn copy_out(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    states: &States,
    pieces: &[Piece],
    to: &ID3D12Resource,
) {
    let (mut before, mut after) = (Vec::new(), Vec::new());
    let mut seen = HashSet::new();
    for p in pieces.iter().filter(|p| !p.res.is_buffer()) {
        if !seen.insert(p.res.id) {
            continue;
        }
        let from = states.get(p.res.id).unwrap_or(vec![COMMON]);
        // From the common state a texture is promoted by the copy.
        let to: Vec<u32> = from
            .iter()
            .map(|&s| {
                if s == COMMON || s & COPY_SOURCE != 0 {
                    s
                } else {
                    COPY_SOURCE
                }
            })
            .collect();
        gpu::moves(&p.res.object(), &from, &to, &mut before);
        gpu::moves(&p.res.object(), &to, &from, &mut after);
    }
    // SAFETY: an open list; live resources.
    unsafe {
        if !before.is_empty() {
            list.ResourceBarrier(&before);
        }
        for p in pieces {
            let object = p.res.object();
            if p.res.is_buffer() {
                list.CopyBufferRegion(to, p.at, &*object, p.first, p.len);
                continue;
            }
            let (all, _) =
                gpu::footprints(device, &gpu::to_desc(&p.res.desc), p.first as u32, p.count);
            for (i, mut f) in all.into_iter().enumerate() {
                f.Offset += p.at;
                list.CopyTextureRegion(
                    &gpu::placed_location(to, f),
                    0,
                    0,
                    0,
                    &gpu::sub_location(&object, p.first as u32 + i as u32),
                    None,
                );
            }
        }
        if !after.is_empty() {
            list.ResourceBarrier(&after);
        }
    }
}

/// Records the copy of a texture's subresource, in `state`, into `to`.
pub(in crate::replay) fn read_sub(
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    sub: u32,
    state: u32,
    to: &ID3D12Resource,
    f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    let there = state == COMMON || state & COPY_SOURCE != 0;
    // SAFETY: an open list; live resources.
    unsafe {
        if !there {
            list.ResourceBarrier(&[gpu::transition(texture, ALL, state, COPY_SOURCE)]);
        }
        list.CopyTextureRegion(
            &gpu::placed_location(to, f),
            0,
            0,
            0,
            &gpu::sub_location(texture, sub),
            None,
        );
        if !there {
            list.ResourceBarrier(&[gpu::transition(texture, ALL, COPY_SOURCE, state)]);
        }
    }
}

/// A slot as it goes into the file: a constant buffer view's address as
/// buffer and offset.
pub(super) fn resolved(r: &Recorder, mut s: Slot) -> Slot {
    let kind = i32::from_le_bytes([s.desc[4], s.desc[5], s.desc[6], s.desc[7]]);
    if s.kind == slot::SRV
        && s.described != 0
        && kind == D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE.0
    {
        r.problem("views of acceleration structures are null in the capture");
        return Slot {
            kind: slot::SRV,
            ..Slot::EMPTY
        };
    }
    if s.kind == slot::CBV && s.described != 0 {
        match r.va(s.address()) {
            Va::At { res, offset } => {
                s.res = res;
                s.set_address(offset);
            }
            Va::Null {} => {}
            Va::Unknown { .. } => {
                // Its buffer is gone: nothing may read it.
                s = Slot {
                    kind: slot::CBV,
                    ..Slot::EMPTY
                };
            }
        }
    }
    s
}

/// The slots as runs of equal ones, empty ones left out.
pub(super) fn runs_of(r: &Recorder, all: &[Slot]) -> Vec<(u32, u32, Slot)> {
    let mut runs: Vec<(u32, u32, Slot)> = Vec::new();
    for (i, s) in all.iter().enumerate() {
        if s.kind == slot::EMPTY {
            continue;
        }
        match runs.last_mut() {
            Some((first, n, same)) if *first + *n == i as u32 && same == s => *n += 1,
            _ => runs.push((i as u32, 1, *s)),
        }
    }
    for run in &mut runs {
        run.2 = resolved(r, run.2);
    }
    runs
}

/// Whether all bytes are zero.
pub(super) fn zero(bytes: &[u8]) -> bool {
    // SAFETY: any bytes are a number.
    let (a, b, c) = unsafe { bytes.align_to::<u128>() };
    a.iter().chain(c).all(|&x| x == 0) && b.iter().all(|&x| x == 0)
}

pub(super) fn write_at(file: &File, mut bytes: &[u8], mut at: u64) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let n = file.seek_write(bytes, at)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[n..];
        at += n as u64;
    }
    Ok(())
}

/// Compares `len` bytes at `live` with those at `kept`, page by page, brings
/// the changed ones in `kept` up to date, and answers their runs.
///
/// # Safety
/// Both are `len` bytes; `kept` is the caller's alone.
pub(super) unsafe fn changed_runs(live: *const u8, kept: *mut u8, len: usize) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut at = 0;
    while at < len {
        let n = PAGE.min(len - at);
        // SAFETY: the caller's.
        unsafe {
            let same = std::slice::from_raw_parts(live.add(at), n)
                == std::slice::from_raw_parts(kept.add(at), n);
            if !same {
                std::ptr::copy_nonoverlapping(live.add(at), kept.add(at), n);
                match runs.last_mut() {
                    Some(last) if last.1 == at => last.1 = at + n,
                    _ => runs.push((at, at + n)),
                }
            }
        }
        at += n;
    }
    runs
}

