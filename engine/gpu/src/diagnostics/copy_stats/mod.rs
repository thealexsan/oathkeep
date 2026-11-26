//! Diagnostics: where the submission copies of title memory go (which
//! ranges, how often, of what memory) and what limits their rate, logged
//! with the frame-rate line. `OATHKEEP_COPY_MODE=plain` copies without the
//! mapped-page guard, `none` skips the copies (both for timing only).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

mod alloc;
mod memory;
mod report;
#[cfg(test)]
mod tests;
mod timing;

pub(crate) use memory::process_memory;
pub(crate) use report::log;
pub(crate) use timing::{
    card_waited, command_took, compared, phase, present_begins, presented, range, recorded,
    submission_begins, submission_ends, whole,
};

/// Counters since the last line.
pub(crate) enum C {
    Submissions,
    Copies,
    CopyBytes,
    Refreshes,
    RefreshBytes,
    Skipped,
    SkippedBytes,
    Runs,
    RunUs,
    Queries,
    QueryUs,
    MoveUs,
    Unmapped,
    /// Write-watch askings and their time.
    Watches,
    WatchUs,
    /// Bytes of watched ranges checked, and of those found unchanged.
    Checked,
    Unchanged,
    /// Copies that met an unmapped page.
    Fallbacks,
    /// Writes taken by the guard on view memory.
    GuardFaults,
    /// Texture bytes detiled after a watched write; hashed; detiled after a
    /// hash change.
    TexWatched,
    TexHashed,
    TexChanged,
    /// Changes of guarded textures the hash found and the guard did not see.
    TexHot,
    /// Frames presented since the last line (what the line divides by).
    Frames,
    /// Calls of list methods that record into a host list, and the
    /// nanoseconds they took, summed over the title's recording threads.
    RecordCalls,
    RecordNs,
    /// Waiting for host pipelines being made (`pipeline::make`).
    PipelineWaitUs,
    /// Dynamic descriptor slots: tables asked for, refused (no free run),
    /// and the nanoseconds of the searches.
    DynamicAsked,
    DynamicRefused,
    DynamicNs,
    /// Marker texts for device-removed data (`HostList::note`).
    NoteNs,
    /// Ranges given to shaders (steps), their snapshots, and the bytes
    /// compared after the submission.
    GpuWrites,
    SnapshotUs,
    BackCompared,
    /// Reading around unmapped pages after a failed bulk read.
    SparseUs,
    /// Guarding view memory (part of the write-watch askings' time).
    GuardUs,
    /// Bytes copied whole because their memory is neither write-watched
    /// nor a view (`Shadow::update_free`).
    WholeBytes,
    /// Submissions with ranges shaders write (they need the GPU's results
    /// before they return).
    BackSubmissions,
    /// The title's frame waits (`frame::wait`) and the time slept in them,
    /// over all its threads.
    PaceWaits,
    PaceUs,
    /// After a present: until the title's first list call and until its
    /// first submission; and from its last submission to its next present.
    UntilRecordUs,
    UntilSubmitUs,
    TailUs,
    /// Resetting host lists (`HostList::reset`).
    ResetNs,
    /// What shaders wrote: finding the 64 KiB pieces that differ, how many
    /// do, and putting them into title memory.
    BackScanUs,
    BackDiffer,
    BackSyncUs,
    /// Texture uploads recorded, and left out as unchanged.
    TexUploads,
    TexSkipped,
    /// Held submissions a present ran although their waits were not met.
    Forced,
    /// Ranges named through unordered access views by shaders that cannot
    /// store to buffers, not given to shaders as written, and their bytes.
    GpuWritesLeftOut,
    GpuWritesLeftOutBytes,
    /// Waits for memory values looked at (`list::ValueWait::met`) and the
    /// nanoseconds the looks took, on every thread.
    WaitChecks,
    WaitCheckNs,
    /// Host lists made (`Host::new_list`: for the lists a queue holds and
    /// for the parts of lists cut at waits) and the nanoseconds that took.
    HostLists,
    HostListNs,
    /// Parts of held submissions that ran while the part behind them could
    /// have run with them (its waits were met already).
    PartsTogether,
    /// Microseconds of the copies counted in `WholeBytes`.
    WholeUs,
    /// Submissions by how long the card was waited for (under 2 ms, 2-8,
    /// 8-32, longer), and the microseconds waited in each class.
    CardUnder2,
    Card2To8,
    Card8To32,
    CardOver32,
    CardUnder2Us,
    Card2To8Us,
    Card8To32Us,
    CardOver32Us,
    /// Submissions that hand nothing back to the title (no range shaders
    /// write, no value written when done), and the microseconds the card
    /// was waited for them.
    NothingBack,
    NothingBackUs,
    /// Waits for every earlier shader write recorded into host lists: for
    /// the title's barrier groups, for its barriers on buffers, and the
    /// indirect commands bounded (each a dispatch of ours between two such
    /// waits).
    WaitsForGroups,
    WaitsForBuffers,
    IndirectBounded,
}

pub(super) const COUNTERS: usize = 80;

/// One of some sets of counters, each in lines of the processor's memory of
/// its own: a set for all threads had them wait for each other's lines at
/// every list call (what counts here is summed up for the log alone).
#[repr(align(64))]
pub(super) struct Bank<const N: usize>(pub(super) [AtomicU64; N]);

/// The sets.
pub(super) const BANKS: usize = 32;

static COUNT: [Bank<COUNTERS>; BANKS] = [const { Bank([const { AtomicU64::new(0) }; COUNTERS]) }; BANKS];

/// This thread's set.
#[inline]
pub(super) fn bank() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    thread_local!(static MINE: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) });
    // (Asked by the allocator too: nothing here allocates.)
    MINE.try_with(|mine| {
        if mine.get() == usize::MAX {
            mine.set(NEXT.fetch_add(1, Relaxed) % BANKS);
        }
        mine.get()
    })
    .unwrap_or(0)
}

pub(crate) fn add(c: C, n: u64) {
    COUNT[bank()].0[c as usize].fetch_add(n, Relaxed);
}

/// The counter, all sets together, as it is now (it goes back to 0 with
/// [`log`]).
pub(crate) fn peek(c: C) -> u64 {
    let i = c as usize;
    COUNT.iter().map(|b| b.0[i].load(Relaxed)).sum()
}

/// Counter `i`, all sets together, from now on 0.
pub(super) fn taken(i: usize) -> u64 {
    COUNT.iter().map(|b| b.0[i].swap(0, Relaxed)).sum()
}

/// How copies run: 0 guarded (as always), 1 plain, 2 not at all.
pub(crate) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("OATHKEEP_COPY_MODE").as_deref() {
        Ok("plain") => 1,
        Ok("none") => 2,
        _ => 0,
    })
}

/// Whether an experiment on barriers is on (`OATHKEEP_BARRIER_TEST`): bit 0
/// for `groups` (the title's barrier groups record no wait for earlier
/// shader writes), bit 1 for `indirect` (the pass that bounds an indirect
/// command runs without its two such waits; its state changes stay), both
/// bits for `both`. For timing only: without the waits a shader may read
/// what another has not finished writing.
pub(crate) fn barrier_test() -> u8 {
    static TEST: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *TEST.get_or_init(|| match std::env::var("OATHKEEP_BARRIER_TEST").as_deref() {
        Ok("groups") => 1,
        Ok("indirect") => 2,
        Ok("both") => 3,
        _ => 0,
    })
}
