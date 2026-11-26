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
use super::super::record::{At, Heap, Ours, Recorder, Res, lock, not_recorded_calls, read};
use super::super::states::{COMMON, COPY_SOURCE, Change, States};
use super::super::wire::{
    self, ALL, Cmd, Content, Delta, Loc, MISSING, NONE, Place, Slot, Va, ZERO, slot,
};

use super::{Fault, PAGE};

/// What a submission reads of buffers, by id: parts of them (copied from),
/// or all (read some other way).
#[derive(Default)]
pub(super) struct Reads {
    all: HashSet<u32>,
    parts: HashMap<u32, Vec<(u64, u64)>>,
}

impl Reads {
    pub(super) fn note(&mut self, cmds: &[Cmd]) {
        let (all, parts) = (&mut self.all, &mut self.parts);
        let mut whole = |va: &Va| {
            if let Va::At { res, .. } = va {
                all.insert(*res);
            }
        };
        for c in cmds {
            match c {
                Cmd::RootView { va, .. }
                | Cmd::IndexBuffer {
                    view: Some((va, _, _)),
                } => whole(va),
                Cmd::VertexBuffers {
                    views: Some(views), ..
                } => views.iter().for_each(|(va, _, _)| whole(va)),
                Cmd::Indirect { args, count, .. } => {
                    for res in [*args, *count] {
                        whole(&Va::At { res, offset: 0 });
                    }
                }
                Cmd::CopyResource { src, .. } => whole(&Va::At {
                    res: *src,
                    offset: 0,
                }),
                Cmd::CopyBuffer {
                    src, src_at, len, ..
                } => parts.entry(*src).or_default().push((*src_at, src_at + len)),
                Cmd::CopyTexture {
                    src:
                        Loc::Placed {
                            res,
                            offset,
                            size,
                            pitch,
                            ..
                        },
                    ..
                } => {
                    // At the most: compressed formats have fewer rows.
                    let len = u64::from(*pitch) * u64::from(size[1]) * u64::from(size[2].max(1));
                    parts.entry(*res).or_default().push((*offset, offset + len));
                }
                _ => {}
            }
        }
    }

    /// The whole pages read of a buffer of `size` bytes; `viewed` by a
    /// descriptor, all of it is.
    pub(super) fn pages(&self, id: u32, size: usize, viewed: bool) -> Vec<(usize, usize)> {
        if viewed || self.all.contains(&id) {
            return vec![(0, size)];
        }
        let Some(parts) = self.parts.get(&id) else {
            return Vec::new();
        };
        let mut pages: Vec<(usize, usize)> = parts
            .iter()
            .map(|&(from, to)| {
                (
                    from as usize / PAGE * PAGE,
                    (to as usize).next_multiple_of(PAGE).min(size),
                )
            })
            .filter(|(from, to)| from < to)
            .collect();
        pages.sort_unstable();
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (from, to) in pages {
            match out.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => out.push((from, to)),
            }
        }
        out
    }
}

/// A buffer the CPU sees.
pub(super) struct CpuBuffer {
    pub(super) id: u32,
    /// Held by the recorder, not by this.
    pub(super) res: std::mem::ManuallyDrop<ID3D12Resource>,
    /// Where it is read: its mapping, or the read-back copy of it.
    pub(super) live: *const u8,
    /// The read-back buffer and the offset in it, for one that is slow to
    /// read.
    pub(super) through: Option<(usize, u64)>,
    pub(super) size: usize,
    /// What it held at the submission before (zeros before the first).
    pub(super) kept: Vec<u8>,
}

// SAFETY: the mapping is of a buffer the struct holds.
unsafe impl Send for CpuBuffer {}
// SAFETY: as above; the mapping is only read.
unsafe impl Sync for CpuBuffer {}

