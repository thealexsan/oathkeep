use super::*;

/// Whether a queue's submissions go to the card without a wait for each
/// (`OATHKEEP_LATE_WAIT=1`; off by default, and off in the timing run, which
/// waits for every list on its own).
///
/// Every submission was prepared, sent, waited for and copied back before
/// the next one was looked at, so the card stood idle while we prepared
/// and we stood idle while it ran: in run 86 a frame of 446 ms is 134 ms of
/// preparing, 194 ms of waiting and 78 ms of copying back, one after the
/// other, on the one thread that submits.
///
/// With the switch a queue's submission is sent and joins a line of
/// "flights" ([`Flight`]); the thread goes on to the next one while the
/// card runs. The flights are settled together ([`Host::settle`]: one wait
/// for the card, what their shaders wrote copied back once per byte, then
/// their bottom-of-pipe writes and the fence signals behind them, in
/// order) at the next point where the title, or our own preparing, can
/// observe what they do. Those points, exactly:
///
/// 1. a synchronous submission of ours ([`Host::execute`]): a present, a
///    read-back or map of a texture, a clear or upload run at once;
/// 2. the next submission reads title memory that a flight's shaders may
///    write: a title buffer copy, a fill of upload memory, a texture's
///    memory, any step that runs code of ours on title memory
///    ([`late_blocked`]);
/// 3. the next submission refreshes a range of a host copy that a flight's
///    shaders may write, and the title wrote a page of it since it was last
///    copied ([`stale_under_flights`]): the refresh leaves such a range
///    alone until the flight's writes are back, as it always did within a
///    submission;
/// 4. a queue stands at a wait for a memory value that no flight's
///    bottom-of-pipe write is known to satisfy, at an address a flight may
///    write ([`late_met`]);
/// 5. the title blocks on a fence without an event;
/// 6. the card has run every flight (a thread of ours waits for that:
///    [`late_helper`]), or the flights hold [`late_most`] host lists.
///
/// What the title is told (bottom-of-pipe writes, fence values) is told
/// when the card has really run the work, later than before and never
/// earlier; a fence signalled on a queue while flights are in the air is
/// set behind them. A wait for a memory value that a flight's
/// bottom-of-pipe write satisfies counts as met: the waiting part goes to
/// the card behind that flight, which is the order the wait asks for.
///
/// What it assumes beyond today: memory the title rewrites between two of
/// its submissions, without waiting for the first, may be seen by the
/// first's shaders (as on the console, where the card reads title memory
/// when it runs, not when the list is submitted).
pub(crate) fn late() -> bool {
    late_stage() > 0
}

/// `OATHKEEP_LATE_WAIT`: 0 (off), 1, or 2 ([`late_exact`]).
pub(super) fn late_stage() -> u8 {
    static STAGE: OnceLock<u8> = OnceLock::new();
    *STAGE.get_or_init(|| {
        if std::env::var_os("OATHKEEP_GPU_TIMING").is_some() {
            return 0;
        }
        match std::env::var("OATHKEEP_LATE_WAIT").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            Ok("3") => 3,
            // The relay alone: on top of stage 3.
            Err(_) if crate::memory::shadow::relay() => 3,
            _ => 0,
        }
    })
}

/// Stage 2 of [`late`] (`OATHKEEP_LATE_WAIT=2`): point 3 of its list is no
/// longer a point. Where the title wrote a page that lies in a range a
/// flight's shaders may write, the bytes the title changed are found (the
/// page as it is now against what the copy held when the flight was
/// prepared) and put into the copy, and only those: what the flight's
/// shaders write beside them stays (`Shadow::merge_under_writes`). The
/// flights are settled for it only where that cannot be done: memory that
/// is not write-watched, or a copy in the card's memory with more changed
/// runs than a submission should send up one by one.
///
/// In run 92 (stage 1) this point settled the flights five times a frame
/// of twelve, each time with the card's work still to wait for: 172 ms a
/// frame of waiting were left of 200.
///
/// What differs from settling first: a byte both the title and a flight's
/// shader wrote is the title's if the shader wrote it first, the shader's
/// if it wrote it afterwards; settled first it was always the shader's.
/// The title writing a byte a shader of its own is writing is a race on
/// the console too.
pub(super) fn late_exact() -> bool {
    late_stage() > 1
}

/// Stage 3 of [`late`] (`OATHKEEP_LATE_WAIT=3`: stage 2, and the present is
/// no longer a point where flights are settled). The copy to the back
/// buffer is sent as a flight of its own (`Host::execute_own`, a list of
/// its own for each present in the air) and the swap chain is told to
/// present behind it; the queue's call returns, and the card finishes the
/// frame while the title records the next one. What the title is told
/// about the frame (bottom-of-pipe writes, fences) is told when the card
/// has run it, by the settling thread.
///
/// What is left out with it: the look whether the presented frame is black
/// (diagnostics; it reads the frame back at once). A frame that is saved
/// or listed is presented the old way. Once every second and a half a
/// submission settles the flights all the same, for the sweep that lets
/// idle host objects go (it needs nothing in the air).
pub(crate) fn late_free_present() -> bool {
    late_stage() > 2
}

/// The relay's first stage (`OATHKEEP_RELAY=1`; off by default; it turns
/// [`late`] on at stage 3 unless `OATHKEEP_LATE_WAIT` says another stage,
/// and is off with it, in the timing run and with kept snapshots).
///
/// With [`late`] alone every settle does three things on the thread that
/// holds the submission lock: it waits for everything in the air, copies
/// back with the card standing, and only then goes on preparing. The frame
/// stays a relay: the card works about 242 ms of it and one thread
/// prepares, waits and copies back for about 220.
///
/// With the switch a thread of its own ([`relay_worker`]) lands flights as
/// soon as the card has run them, in the order they were sent and without
/// the submission lock: it fetches and merges what their shaders wrote to
/// copies in the card's memory, compares and merges the other copies,
/// frees the flights' ranges, and then does their read-backs, their
/// bottom-of-pipe writes and the fence signals behind them, in the line's
/// order. The submitting thread meanwhile prepares and sends the next
/// submissions. [`Host::settle`] no longer settles anything: it waits until
/// that thread has landed everything in the air, at the same points as
/// before (the list in [`late`]), so whoever can observe title memory, a
/// fence or a present still does so only after the flights before it are
/// merged.
///
/// What makes that safe is in `crate::memory::shadow::relay` (ranges counted per
/// flight; a merge that never stores into memory the card may be writing;
/// sends to card copies numbered) and here: flights land in the order of
/// [`LINE`] by one thread, so a later flight's results never reach title
/// memory before an earlier flight's; the compare on the card stores the
/// hash it made, so each flight's compare flags exactly what changed since
/// the compare before it, however late the pieces are fetched; the fetch
/// goes to the queue under [`QUEUE_ORDER`], which says which sends of
/// title bytes the card had received when it ran.
pub(super) fn late_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_LATE_WAIT_LISTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(256).max(1)
    })
}

/// Whether a submission with bottom-of-pipe writes may fly
/// (`OATHKEEP_LATE_WAIT_WRITES=0`: it is settled as soon as it is sent, so
/// the title is told at once, as before; for telling a title that polls
/// such a value from one that does not).
pub(super) fn late_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LATE_WAIT_WRITES").is_none_or(|v| v != "0"))
}

/// Per host copy, the ranges a submission's shaders may write, as declared
/// (address, end).
pub(super) type Declared = (std::sync::Arc<crate::memory::shadow::Shadow>, Vec<(usize, usize)>);

/// What [`Host::submit`] hands back for a submission it sent and did not
/// wait for.
pub(super) struct Sent {
    /// Its number among the submissions started.
    pub(super) number: u64,
    pub(super) card: Option<CardPlan>,
    pub(super) back: Vec<Declared>,
    /// What was taken off it to be done on the card.
    pub(super) own: Option<gpu_steps::Taken>,
    /// Title-memory ranges written when its shader textures are copied back.
    pub(super) texture_back_ranges: Vec<(usize, usize)>,
    /// Its bottom-of-pipe writes.
    pub(super) writes: Vec<(usize, crate::d3d12::command_queue::Written, usize)>,
}

/// A submission on the card that has not been waited for ([`late`]): its
/// host lists, taken from the title's (which it may reset meanwhile), and
/// what is to be done once it has run.
pub(super) struct Flight {
    pub(super) number: u64,
    /// The value signalled on the host's fence behind it.
    pub(super) fence: u64,
    pub(super) lists: Vec<crate::d3d12::command_list::Detached>,
    pub(super) order: Vec<usize>,
    pub(super) card: Option<CardPlan>,
    pub(super) back: Vec<Declared>,
    /// The list of ours that ran ahead of them ([`card_steps`]).
    pub(super) own: Option<gpu_steps::Taken>,
    pub(super) texture_back_ranges: Vec<(usize, usize)>,
}

/// One thing in the line of what waits for the card, in order.
pub(super) enum Late {
    Run(Flight),
    /// A bottom-of-pipe write of the flight before it: address, what, bytes.
    Write(usize, crate::d3d12::command_queue::Written, usize),
    /// A fence (its address) signalled behind the flights before it.
    Signal(usize, u64),
}

pub(super) struct Line {
    pub(super) line: Vec<Late>,
    /// The title memory the flights' shaders and texture readbacks may write
    /// (address, end), in order and joined.
    pub(super) written: Vec<(usize, usize)>,
    /// Host lists in the flights.
    pub(super) lists: usize,
    /// The value signalled on the host's fence behind the last flight.
    pub(super) target: u64,
    /// A settle has taken the line and is not done: a signal still goes
    /// behind it.
    pub(super) settling: bool,
    /// Something was settled away from a queue's own calls: the helper
    /// thread looks at what queues hold.
    pub(super) poke: bool,
    /// Host lists of settled flights, reset and given back outside the
    /// submission lock ([`land`]: a settle may run under a caller's own
    /// locks).
    pub(super) landed: Vec<(Vec<crate::d3d12::command_list::Detached>, Vec<usize>)>,
    /// With [`relay`]: the bottom-of-pipe writes of the flights being
    /// landed (taken out of `line`, not done yet), for [`late_met`].
    pub(super) in_hand: Vec<(usize, crate::d3d12::command_queue::Written, usize)>,
    /// With [`card_steps`]: what the flights being landed write.
    pub(super) landing: Vec<gpu_steps::Writer>,
}

// SAFETY: D3D12 objects (free-threaded) and host lists nothing records
// into; touched under the lock.
unsafe impl Send for Line {}

pub(super) static LINE: Mutex<Line> = Mutex::new(Line {
    line: Vec::new(),
    written: Vec::new(),
    lists: 0,
    target: 0,
    settling: false,
    poke: false,
    landed: Vec::new(),
    in_hand: Vec::new(),
    landing: Vec::new(),
});
/// Told when a flight joins [`LINE`] or it is poked.
pub(super) static LINE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

pub(super) fn line() -> std::sync::MutexGuard<'static, Line> {
    LINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why flights were settled ([`LATE_STATS`]' places from 2 on).
pub(super) const WHY_SYNC: usize = 2;
pub(super) const WHY_CARD_DONE: usize = 3;
pub(super) const WHY_VALUE: usize = 4;
pub(super) const WHY_STALE: usize = 5;
pub(super) const WHY_READ: usize = 6;
pub(super) const WHY_RUN: usize = 7;
pub(super) const WHY_MOST: usize = 8;
pub(super) const WHY_WRITES: usize = 9;
pub(super) const WHY_FENCE: usize = 10;

/// Since the last frame-rate line: submissions sent without a wait;
/// settles; settles by reason ([`WHY_SYNC`]..); flights settled;
/// microseconds settles waited for the card; value waits met by a flight's
/// bottom-of-pipe write; fence signals set behind flights.
/// Then: the settles for title memory read, by what read it (a buffer
/// copy, a fill of upload memory, a texture).
pub(super) static LATE_STATS: [std::sync::atomic::AtomicU64; 18] = [const { std::sync::atomic::AtomicU64::new(0) }; 18];

/// Since the last frame-rate line: the microseconds settles waited for the
/// card, by why they settled (the `WHY_` places).
pub(super) static LATE_WAITED: [std::sync::atomic::AtomicU64; 11] = [const { std::sync::atomic::AtomicU64::new(0) }; 11];

/// Whether `[from, to)` touches one of `ranges` (in order, joined).
pub(super) fn touches(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    let at = ranges.partition_point(|r| r.1 <= from);
    ranges.get(at).is_some_and(|r| r.0 < to)
}

/// `ranges` with `more`, in order, joined where they overlap or touch.
pub(super) fn join_ranges(ranges: &mut Vec<(usize, usize)>, more: impl IntoIterator<Item = (usize, usize)>) {
    ranges.extend(more.into_iter().filter(|r| r.0 < r.1));
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(from, to) in ranges.iter() {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    *ranges = out;
}
