use super::*;

pub(crate) fn relay() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| late_stage() > 0 && crate::shadow::relay())
}

/// The relay's second stage (`OATHKEEP_RELAY=2`): a submission that reads
/// title memory a flight's shaders may write (a title buffer copy, a fill
/// of upload memory, a texture) waits only until the flights that declared
/// that memory have landed, not for everything in the air; and at
/// [`late_most`] lists in the air it waits until there are fewer. At `1`
/// both wait for everything.
pub(super) fn relay_exact() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    // Stage 2 unless `OATHKEEP_RELAY=1` asks for the first one (the default
    // since the walls of the first room were found drawn in every run with
    // this set and in fewer than half without it).
    *ON.get_or_init(|| relay() && std::env::var_os("OATHKEEP_RELAY").is_none_or(|v| v == "2"))
}

/// Whether the pieces the relay fetches from the card's memory come over the
/// card's copy queue (`OATHKEEP_RELAY_COPY=1`) and not over the queue that
/// draws.
///
/// On the drawing queue a fetch runs behind whatever was sent meanwhile,
/// and with the relay that is most of a frame: the thread that lands flights
/// stood 48 ms of a 120 ms frame waiting for seven fetches of 94 MB in all,
/// a few milliseconds of copying, and everything that waits for a landing
/// (the title's fences, reads of memory flights may write) waited with it.
///
/// What changes: the fetch runs when its flights are done and beside later
/// submissions, so of the sends of title bytes it is only known to have run
/// behind those its own flights ran behind (`CardPlan::sent_as`). Bytes sent
/// later are left out of the merge (`Shadow::card_merge_relay`): they are
/// the title's, newer than what the flights wrote. A later flight's bytes
/// may be in what is fetched, as before.
pub(super) fn relay_copy() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_RELAY_COPY").is_some_and(|v| v == "1"))
}

/// Told when something joins [`LINE`], for the thread that lands flights
/// ([`relay`]); [`LINE_MOVED`] stays the word for the thread that looks at
/// what queues hold.
pub(super) static RELAY_MOVED: std::sync::Condvar = std::sync::Condvar::new();

/// Told when flights have landed ([`relay`]): who waits for that looks
/// again.
pub(super) static LINE_LANDED: std::sync::Condvar = std::sync::Condvar::new();

/// With [`relay`], the order of what we put on the queue: the number of
/// sends of changed ranges to copies in the card's memory so far
/// (`Host::card_before`). Held from the taking of the ranges until their
/// list is on the queue, and while a fetch of pieces goes to the queue: a
/// fetch sent with the count at N ran behind sends 1..N and ahead of every
/// later one.
pub(super) static QUEUE_ORDER: Mutex<u64> = Mutex::new(0);

/// Since the last frame-rate line, of the thread that lands flights
/// ([`relay`]): landings, flights landed, microseconds it waited for the
/// card with nothing to land, microseconds it waited for fetches of card
/// copies' pieces, microseconds of landing in all.
pub(super) static RELAY_STATS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Of the line's things in order (a flight: the value signalled on the
/// host's fence behind it; a write or a signal behind a flight: `None`):
/// how many, from the front, can be landed when the card has reached
/// `completed`: everything ahead of the first flight it has not run. A
/// later flight never lands before an earlier one, and what follows a
/// flight (its bottom-of-pipe writes, signals) never before the flight.
pub(super) fn landable(line: &[Option<u64>], completed: u64) -> usize {
    line.iter().position(|f| f.is_some_and(|f| f > completed)).unwrap_or(line.len())
}

/// For one plan of copies in the card's memory, with [`relay`]: `flags`
/// holds, for every piece of every range of the plan, the copy, the
/// piece's number in the copy and whether the compare flagged it there (or
/// did not run). The pieces to fetch, under whichever range, in order: all
/// of them flagged under any. Two ranges of a plan may hold parts of the same
/// 64 KiB piece; the compare hashes the whole piece for each and stores
/// the hash, so the second of them may find it stored already and not
/// flag it.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn flagged_pieces(flags: &[(usize, u32, bool)]) -> Vec<(usize, u32)> {
    let mut pieces: Vec<(usize, u32)> = flags.iter().filter(|f| f.2).map(|f| (f.0, f.1)).collect();
    pieces.sort_unstable();
    pieces.dedup();
    pieces
}

pub(super) fn relay_wait(why: usize, done: impl Fn(&Line) -> bool) {
    let mut line = line();
    if done(&line) {
        return;
    }
    LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
    LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
    let was_working = working(false);
    let waiting = std::time::Instant::now();
    let _waiting = crate::timeline::span(match why {
        WHY_SYNC => "waiting for landings: everything (a present, the sweep)",
        WHY_VALUE => "waiting for landings: a memory value",
        WHY_STALE => "waiting for landings: title memory changed under shader writes",
        WHY_READ => "waiting for landings: title memory read",
        WHY_MOST => "waiting for landings: lists in the air",
        WHY_FENCE => "waiting for landings: a blocking fence wait",
        _ => "waiting for landings: another reason",
    });
    while !done(&line) {
        line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
    }
    drop(line);
    stood(waiting);
    working(was_working);
    let us = waiting.elapsed().as_micros() as u64;
    LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
    LATE_WAITED[why.min(10)].fetch_add(us, Ordering::Relaxed);
    SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    blamed(why, us);
}

/// What the thread that lands flights fetches card copies' pieces with
/// ([`relay`]): a list, a fence and an event of its own (the host's are
/// behind the submission lock), and the memory the pieces come down to.
pub(super) struct RelayFetch {
    pub(super) list: Prelude,
    /// With [`relay_copy`]: the card's copy queue, which `list` is for.
    pub(super) queue: Option<ID3D12CommandQueue>,
    pub(super) fence: ID3D12Fence,
    pub(super) event: HANDLE,
    /// The last value signalled on `fence`.
    pub(super) sent: u64,
    /// Buffer, mapping, bytes.
    pub(super) buffer: Option<(ID3D12Resource, usize, u64)>,
    /// Pieces caught while a later flight was writing them.
    pub(super) suspects: torn::Suspects,
    /// What earlier fetches brought of flights not landed yet.
    pub(super) had: crate::land::Had,
}

/// The thread that lands flights ([`relay`]): as soon as the card has run
/// the first flights of the line it takes them, with what follows them,
/// and does what they left to do ([`Host::relay_land`]); else it waits for
/// the card, or for a flight. It takes neither the submission lock nor
/// anything a caller of [`Host::settle`] may hold: resetting the landed
/// lists and looking at what queues hold are left to [`late_helper`].
pub(super) fn relay_worker() {
    crate::cores::keep_on_fast_cores("victory-relay");
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        fail(format_args!("no event for the thread that lands submissions sent ahead (OATHKEEP_RELAY=1)"));
    };
    let mut fetch: Option<RelayFetch> = None;
    loop {
        let next = {
            let mut line = line();
            loop {
                if line.line.is_empty() {
                    line = RELAY_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let fences: Vec<Option<u64>> = line
                    .line
                    .iter()
                    .map(|l| match l {
                        Late::Run(flight) => Some(flight.fence),
                        _ => None,
                    })
                    .collect();
                // SAFETY: a plain query.
                let completed = unsafe { host.fence.GetCompletedValue() };
                let n = landable(&fences, completed);
                if n == 0 {
                    // The first thing is a flight the card has not run.
                    break Err(fences[0].unwrap_or(0));
                }
                line.settling = true;
                let items: Vec<Late> = line.line.drain(..n).collect();
                line.in_hand = items
                    .iter()
                    .filter_map(|l| match l {
                        Late::Write(at, what, bytes) => Some((*at, *what, *bytes)),
                        _ => None,
                    })
                    .collect();
                if card_steps::stage() > 0 {
                    line.landing = card_steps::writers(&items);
                }
                break Ok(items);
            }
        };
        match next {
            Ok(items) => host.relay_land(items, &mut fetch),
            Err(target) => {
                let waiting = std::time::Instant::now();
                #[cfg(feature = "diagnostics")]
                let _card = crate::timeline::card_wait(Some(target));
                // SAFETY: the host's fence and this thread's event.
                let waited = unsafe {
                    host.fence.SetEventOnCompletion(target, event).map(|()| {
                        WaitForSingleObject(event, INFINITE);
                    })
                };
                if waited.is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
    }
}

/// [`relay`]: what shaders wrote to the declared ranges `back` of copies
/// in the PC's memory goes into title memory. As [`hand_back`], each byte
/// once, the pieces that differ found on several threads; but the merge is
/// `Shadow::relay_merge`, which does not write the copy, and the ranges
/// are not done with here (each flight's are, afterwards). In two steps:
/// what is to be looked through and what of it differs ([`RelayBack::look`],
/// which needs nothing that the fetch from the card's memory brings), and
/// the merge.
pub(super) struct RelayBack {
    pub(super) work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    pub(super) found: Option<Vec<Vec<(usize, usize)>>>,
}

pub(super) fn relay_hand_back(back: &[Declared]) -> RelayBack {
    let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for (shadow, declared) in back {
        if shadow.carded() {
            continue;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        join_ranges(&mut spans, declared.iter().copied());
        for &(from, to) in &spans {
            shadow.relay_compared(from, to - from);
        }
        work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
    }
    RelayBack { work, found: None }
}

impl RelayBack {
    /// Looks the ranges through, once; its time counts as `stat`.
    pub(super) fn look(&mut self, stat: usize) {
        if self.work.is_empty() || self.found.is_some() {
            return;
        }
        let scanning = std::time::Instant::now();
        // Looked through without a lock: a piece that changes meanwhile (a
        // later flight writing, the title's bytes going in) is found now or
        // by that flight's landing, and the merge looks again under the
        // lock.
        self.found = Some(crate::shadow::find_all(&self.work));
        crate::land::spent(stat, scanning);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
    }

    /// What differs goes into title memory.
    pub(super) fn merge(mut self) {
        use crate::land;
        self.look(land::stat::SCAN);
        let Some(found) = self.found else {
            return;
        };
        let merging = std::time::Instant::now();
        let mut written: Vec<u64> = Vec::new();
        if land::on() {
            // Pieces in the same block of title memory in one job, in order.
            let pieces: Vec<(usize, usize, usize)> =
                found.iter().enumerate().flat_map(|(i, found)| found.iter().map(move |&(at, n)| (i, at, n))).collect();
            let start = |i: usize| self.work[i].0.span().0 as usize;
            let mut order: Vec<usize> = (0..pieces.len()).collect();
            order.sort_by_key(|&p| (start(pieces[p].0) + pieces[p].1) / crate::land::BLOCK);
            let cut: Vec<(usize, usize)> = order.iter().map(|&p| (start(pieces[p].0) + pieces[p].1, pieces[p].2)).collect();
            let jobs = land::jobs(&cut);
            let put: Vec<std::sync::atomic::AtomicU64> = self.work.iter().map(|_| Default::default()).collect();
            land::each(jobs.len(), &|j| {
                let working = std::time::Instant::now();
                let mut notes = land::Notes::default();
                for &p in &order[jobs[j].clone()] {
                    let (i, at, n) = pieces[p];
                    put[i].fetch_add(self.work[i].0.land_back(at, n, &mut notes), Ordering::Relaxed);
                }
                notes.flush();
                land::spent(land::stat::BACK_ALL, working);
            });
            land::count(land::stat::JOBS, jobs.len() as u64);
            written.extend(put.iter().map(|p| p.load(Ordering::Relaxed)));
        } else {
            let (mut now, mut title) = (Vec::new(), Vec::new());
            for ((shadow, ..), found) in self.work.iter().zip(found) {
                let merge = |(at, n)| shadow.relay_merge(at, n, &mut now, &mut title) as u64;
                written.push(found.into_iter().map(merge).sum());
            }
        }
        land::spent(land::stat::BACK, merging);
        for ((_, from, to), written) in self.work.iter().zip(written) {
            COPY_BACK[1].fetch_add(written, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(*from, to - from, written as usize);
            let _ = (from, to);
        }
    }
}
