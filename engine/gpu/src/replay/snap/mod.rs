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
use super::record::{At, Heap, Ours, Recorder, Res, lock, not_recorded_calls, read};
use super::states::{COMMON, COPY_SOURCE, Change, States};
use super::wire::{
    self, ALL, Cmd, Content, Delta, Loc, MISSING, NONE, Place, Slot, Va, ZERO, slot,
};

pub(super) type Fault = Box<dyn std::error::Error + Send + Sync>;

/// A read-back buffer contents go through, and how many there are.
pub(super) const STAGING: u64 = 256 << 20;
pub(super) const STAGINGS: usize = 4;
/// The most of a buffer in one piece.
pub(super) const PIECE: u64 = 32 << 20;
/// What the CPU changes is kept in units of.
pub(super) const PAGE: usize = 4096;
/// What one thread compares at a time.
pub(super) const CHUNK: usize = 1 << 20;
/// Frames waited, at the most, for one whose lists are all recorded.
const PATIENCE: u64 = 100;

#[derive(Default)]
pub(super) enum Stage {
    #[default]
    Waiting,
    /// Lists are recorded, since this many presents.
    Armed(u64),
    Capturing(Box<Capture>),
    Done,
}

/// Where the recorder is.
#[derive(Default)]
pub(super) struct Phase {
    presents: u64,
    stage: Stage,
    /// Submissions of this frame so far, and lists among them that were not
    /// recorded.
    submits: u32,
    missing: u32,
}

impl Phase {
    pub(super) fn done(&self) -> bool {
        matches!(self.stage, Stage::Done)
    }
}

/// A submission to a queue other than the drawing one: left out.
pub(super) fn other_queue(r: &Recorder, lists: u32) {
    if r.watching.load(Ordering::SeqCst) {
        r.problem_times(
            "lists sent to a queue other than the drawing one are not in the capture",
            u64::from(lists),
        );
    }
}

/// A submission to the drawing queue, made by `go`.
pub(super) fn submit(
    r: &Recorder,
    queue: &ID3D12CommandQueue,
    lists: &[*mut std::ffi::c_void],
    go: impl FnOnce(),
) {
    let mut phase = lock(&r.submits);
    type Closed = (Option<Arc<Vec<u8>>>, Arc<Vec<Change>>);
    let closed: Vec<Option<Closed>> = lists
        .iter()
        .map(|&p| r.list(p).and_then(|l| l.closed()))
        .collect();
    phase.submits += 1;
    let whole = |c: &Option<Closed>| c.as_ref().is_some_and(|c| c.0.is_some());
    phase.missing += lists
        .iter()
        .zip(&closed)
        .filter(|(p, c)| !whole(c) && r.knows(**p))
        .count() as u32;
    if let Stage::Capturing(c) = &mut phase.stage {
        let commands: Vec<Option<Arc<Vec<u8>>>> = closed
            .iter()
            .map(|c| c.as_ref().and_then(|c| c.0.clone()))
            .collect();
        let _ours = Ours::new();
        if let Err(e) = c.submit(r, queue, &commands) {
            stop(r, &mut phase, &*e);
        }
    }
    go();
    let mut states = lock(&r.states);
    for change in closed.iter().flatten().flat_map(|c| c.1.iter()) {
        states.change(change);
    }
    states.decay();
}

/// The capture failed.
pub(super) fn stop(r: &Recorder, phase: &mut Phase, e: &(dyn std::error::Error + Send + Sync)) {
    tracing::error!("frame recorder: the capture failed: {e}");
    let _ = std::fs::write(r.folder.join("failed.txt"), e.to_string());
    phase.stage = Stage::Done;
    r.watching.store(false, Ordering::SeqCst);
    r.arm(false);
    let held = std::mem::take(&mut *lock(&r.held));
    drop(held);
}

pub(super) fn present(r: &Recorder, texture: &ID3D12Resource, state: u32) {
    let mut phase = lock(&r.submits);
    phase.presents += 1;
    let presents = phase.presents;
    let (submits, missing) = (
        std::mem::take(&mut phase.submits),
        std::mem::take(&mut phase.missing),
    );
    let mut finished = None;
    match &mut phase.stage {
        Stage::Waiting => {
            let due = match r.settings.at {
                At::Seconds(s) => r.began.elapsed().as_secs_f64() >= s,
                At::Frame(f) => presents >= f,
                At::Trigger => false,
            };
            if due || r.settings.folder.join("go").exists() {
                tracing::info!(presents, "frame recorder: lists are recorded from now on");
                r.arm(true);
                phase.stage = Stage::Armed(presents);
            }
        }
        Stage::Armed(since) => {
            let waited = presents - *since;
            let whole = submits > 0 && missing == 0;
            if waited >= 2 && (whole || waited > PATIENCE) {
                if !whole {
                    r.problem("no frame was found whose lists were all recorded");
                }
                tracing::info!(
                    presents,
                    waited,
                    "frame recorder: the next frame is captured"
                );
                match Capture::new(r) {
                    Ok(c) => phase.stage = Stage::Capturing(Box::new(c)),
                    Err(e) => stop(r, &mut phase, &*e),
                }
            }
        }
        Stage::Capturing(c) if c.started => {
            let _ours = Ours::new();
            let done = c.present(r, texture, state).and_then(|()| {
                if c.frame < r.settings.frames {
                    return Ok(false);
                }
                c.finish(r)?;
                Ok(true)
            });
            match done {
                Ok(false) => {}
                Ok(true) => {
                    r.watching.store(false, Ordering::SeqCst);
                    r.arm(false);
                    if let Stage::Capturing(c) = std::mem::replace(&mut phase.stage, Stage::Done) {
                        finished = Some((c, std::mem::take(&mut *lock(&r.held))));
                    }
                }
                Err(e) => stop(r, &mut phase, &*e),
            }
        }
        // A frame without a submission: the next one.
        Stage::Capturing(_) | Stage::Done => {}
    }
    drop(phase);
    // What was kept alive goes without a lock held.
    drop(finished);
}


mod capture;
use capture::Capture;
mod capture_write;
mod io;
mod reads;

pub(super) use io::read_sub;

#[cfg(test)]
mod tests;
