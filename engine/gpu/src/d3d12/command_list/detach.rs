use super::{List, This, Upload, ValueWait};

/// One part of a title list cut at its waits: the waits the queue stands
/// at, then the host list that runs once they are met.
pub(super) struct Part {
    pub(super) waits: Vec<ValueWait>,
    pub(super) host: crate::backend::HostList,
}

/// The host side of a submitted list, taken from it: a queue that waits
/// for a fence runs it later, and the title may reset the list meanwhile.
/// With it go the upload memory and the dynamic descriptor slots its
/// commands name, kept until it has run.
pub(crate) struct Detached {
    host: crate::backend::HostList,
    _uploads: Vec<Upload>,
    _state: crate::execution::draw::State,
}

impl Detached {
    pub(crate) fn host(&self) -> &crate::backend::HostList {
        &self.host
    }

    /// Releases this list's dynamic descriptor slots once its GPU submission
    /// has completed, without waiting for the full `bury_lag` allocator delay.
    pub(crate) fn release_tables(&mut self) {
        self._state.tables_done();
    }
}

/// A host list that has run and waits to be used again, and the bytes of
/// upload memory it keeps.
struct Idle(crate::backend::HostList, u64);

// SAFETY: D3D12 objects are free-threaded, and an idle list is touched by
// nobody until it is taken out again.
unsafe impl Send for Idle {}

/// Host lists that have run, reset and ready to record: the host side of
/// a list a queue held, or of a part of a list cut at a wait, is taken
/// from the title's list at its submission, and the title's list needs
/// another. Each was made new and thrown away after its run: a hundred a
/// frame in gameplay, each two command allocators and two lists, and for
/// four in ten a budget and an argument buffer (run 61: "104 host lists
/// made 21.2 ms" a frame, and the graphics kernel's time to free them).
static IDLE: std::sync::Mutex<(Vec<Idle>, u64)> = std::sync::Mutex::new((Vec::new(), 0));

/// The most host lists kept idle (128 were too few: flights land several at
/// a time, so lists were thrown away and others made in the same frame, with
/// their preludes and buffers, 12 % of the submitting thread in the opening,
/// profile cpu-1), and the most upload memory they may keep
/// between them (a list keeps scratch chunks over a reset only while lists
/// do not share them, `OATHKEEP_SCRATCH_POOL=0`).
const IDLE_MOST: (usize, u64) = (512, 256 << 20);

/// Whether host lists that have run are used again (on by default;
/// `OATHKEEP_LIST_POOL=0`: each is made new and thrown away, as before).
fn pools() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LIST_POOL").is_none_or(|v| v != "0"))
}

/// A host list open for recording: one that has run, as a title's list is
/// after its reset, or a new one.
pub(super) fn fresh_list() -> Option<crate::backend::HostList> {
    let idle = {
        let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
        let taken = idle.0.pop();
        if let Some(Idle(_, bytes)) = &taken {
            idle.1 -= bytes;
        }
        taken
    };
    match idle {
        Some(Idle(list, _)) => Some(list),
        None => crate::backend::host().and_then(|h| h.new_list().ok()),
    }
}

/// The host sides of lists whose submission has run (submissions are
/// waited for; `ran` names the ones it ran, by their place in `lists`):
/// what they held is let go, as before, and the host lists are reset and
/// kept for [`fresh_list`].
pub(crate) fn recycle(lists: Vec<Detached>, ran: &[usize]) {
    if !pools() {
        return;
    }
    for (i, Detached { host, _uploads, _state }) in lists.into_iter().enumerate() {
        // The descriptor slots and the upload memory its commands named.
        drop(_state);
        drop(_uploads);
        if !ran.contains(&i) || IDLE.lock().unwrap_or_else(|e| e.into_inner()).0.len() >= IDLE_MOST.0 {
            continue;
        }
        // As the title's reset does: what the list kept for its commands
        // goes, its allocators stay.
        host.reset();
        let bytes = host.scratch_kept();
        let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
        if idle.0.len() < IDLE_MOST.0 && idle.1 + bytes <= IDLE_MOST.1 {
            idle.0.push(Idle(host, bytes));
            idle.1 += bytes;
        }
    }
}

/// A wait for a memory value recorded into list `this`, with waits cut
/// into lists (`crate::d3d12::command_queue::cuts`). On the console the queue runs the
/// list up to the wait, stands there until the value is in memory, and
/// runs on; the list's bottom-of-pipe writes before the wait are done by
/// then, and other queues may be waiting for exactly those. So the
/// recording is cut here: the host list so far is closed and kept as a
/// part of its own ([`Part`]), the title's calls record into a new host
/// list from here on, and the wait stands before that one. A submission
/// then gives the queue the parts in order ([`detach_parts`]).
///
/// A wait before any work (the first call after a reset, or a wait right
/// after another) cuts nothing: it joins the waits before the current
/// host list.
/// shortcut: a bottom-of-pipe write is done when its part has run, not at
/// its place in the part: a queue waiting for it starts later than on the
/// console, never earlier, and nothing can wait for itself that way (a
/// part has no wait after its first work).
pub(super) fn cut(this: This, wait: ValueWait) {
    if this.is_null() {
        return;
    }
    // SAFETY: the title passes one of our lists; a list is recorded by one
    // thread at a time and is not submitted while it records.
    let list = unsafe { &mut *this.cast_mut().cast::<List>() };
    let Some(current) = list.host.as_ref() else {
        return;
    };
    if current.untouched() {
        list.gate.push(wait);
        return;
    }
    let Some(fresh) = fresh_list() else {
        // shortcut: without a new host list the wait stands before the part
        // it was recorded in.
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("no host list for the part after a wait: the wait moves to the part's start"));
        list.gate.push(wait);
        return;
    };
    current.close();
    static CUTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if CUTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
        tracing::info!(parts = list.earlier.len() + 2, "a list is cut at a wait for a memory value");
    }
    if let Some(done) = list.host.replace(fresh) {
        let waits = std::mem::replace(&mut list.gate, vec![wait]);
        list.earlier.push(Part { waits, host: done });
    }
    // What the title set so far, on the new host list.
    if let Some(host) = list.host.as_ref() {
        list.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replay(host);
    }
}

/// Whether list `this` recorded a wait for a memory value (with waits cut
/// into lists).
pub(crate) fn cut_list(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    // SAFETY: one of our lists, closed and submitted: nothing records
    // into it.
    let list = unsafe { &*this.cast::<List>() };
    !list.earlier.is_empty() || !list.gate.is_empty()
}

/// What a queue is given for submitted lists that were cut at their waits:
/// the waits it stands at, the host lists it runs once they are met, and
/// the order those run in.
pub(crate) type Parts = Vec<(Vec<ValueWait>, Vec<Detached>, Vec<usize>)>;

/// Takes the host sides of the submitted `lists`, part by part ([`cut`]),
/// and leaves each list a new, closed host list for its next reset, as
/// [`detach`] does. What comes back is in the order the queue runs it: a
/// new entry starts at every part that has waits; host lists without
/// waits join the entry before them. The list's upload memory and draw
/// state (its dynamic descriptor slots) go with its last part, which runs
/// last.
/// shortcut: a list the submission names a second time runs again only if
/// it has one part and that part is in the entry being filled; otherwise
/// the second naming is dropped (logged once).
pub(crate) fn detach_parts(lists: &[This]) -> Parts {
    let mut parts: Parts = vec![(Vec::new(), Vec::new(), Vec::new())];
    // The lists taken: where its one host list is (entry, place), or none
    // for a list of several parts.
    let mut seen: Vec<(This, Option<(usize, usize)>)> = Vec::new();
    for &this in lists {
        if this.is_null() {
            continue;
        }
        if let Some(&(_, at)) = seen.iter().find(|(t, _)| *t == this) {
            match at {
                Some((entry, place)) if entry + 1 == parts.len() => parts[entry].2.push(place),
                _ => {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| {
                        tracing::warn!("a list named twice by a submission that waits for memory values runs once")
                    });
                }
            }
            continue;
        }
        let Some(fresh) = fresh_list() else {
            continue;
        };
        // A new host list is open; the title's next call on its list is a
        // reset, which wants it closed.
        fresh.close();
        // SAFETY: one of our lists, closed and submitted: nothing records
        // into it until the title resets it.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        let Some(host) = list.host.replace(fresh) else {
            continue;
        };
        let earlier = std::mem::take(&mut list.earlier);
        let gate = std::mem::take(&mut list.gate);
        let uploads = std::mem::take(&mut *list.uploads.lock().unwrap_or_else(|e| e.into_inner()));
        let state = std::mem::take(&mut *list.state.lock().unwrap_or_else(|e| e.into_inner()));
        let single = earlier.is_empty() && gate.is_empty();
        let mut pieces: Vec<(Vec<ValueWait>, Detached)> = earlier
            .into_iter()
            .map(|part| {
                (
                    part.waits,
                    Detached {
                        host: part.host,
                        _uploads: Vec::new(),
                        _state: Default::default(),
                    },
                )
            })
            .collect();
        pieces.push((
            gate,
            Detached {
                host,
                _uploads: uploads,
                _state: state,
            },
        ));
        let mut last = None;
        for (waits, detached) in pieces {
            if !waits.is_empty() {
                parts.push((waits, Vec::new(), Vec::new()));
            }
            let entry = parts.len() - 1;
            let (_, taken, order) = &mut parts[entry];
            let place = taken.len();
            order.push(place);
            taken.push(detached);
            last = Some((entry, place));
        }
        seen.push((this, if single { last } else { None }));
    }
    // (The first entry is empty if the first list begins with a wait.)
    parts.retain(|(_, taken, _)| !taken.is_empty());
    parts
}

/// Takes the host sides of submitted lists (each once, however often the
/// submission names it) and leaves each list a new, closed host list for
/// its next reset: what was taken, and which of them run in what order.
pub(crate) fn detach(lists: &[This]) -> (Vec<Detached>, Vec<usize>) {
    let mut taken: Vec<(This, Detached)> = Vec::new();
    let mut order = Vec::new();
    for &this in lists {
        if this.is_null() {
            continue;
        }
        if let Some(i) = taken.iter().position(|(t, _)| *t == this) {
            order.push(i);
            continue;
        }
        let Some(fresh) = fresh_list() else {
            continue;
        };
        // A new host list is open; the title's next call on its list is a
        // reset, which wants it closed.
        fresh.close();
        let list = this.cast_mut().cast::<List>();
        // SAFETY: one of our lists, closed and submitted: nothing records
        // into it until the title resets it.
        let (host, uploads, state) = unsafe {
            let Some(host) = (*list).host.replace(fresh) else {
                continue;
            };
            let uploads =
                std::mem::take(&mut *(*list).uploads.lock().unwrap_or_else(|e| e.into_inner()));
            let state =
                std::mem::take(&mut *(*list).state.lock().unwrap_or_else(|e| e.into_inner()));
            (host, uploads, state)
        };
        order.push(taken.len());
        taken.push((
            this,
            Detached {
                host,
                _uploads: uploads,
                _state: state,
            },
        ));
    }
    (taken.into_iter().map(|(_, d)| d).collect(), order)
}
