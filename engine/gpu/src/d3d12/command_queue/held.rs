use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{Queue, This, async_submit, queue};

/// One thing a waiting queue holds back.
pub(super) enum Held {
    /// The queue waits for the fence (its address) to reach the value.
    Wait(usize, u64),
    /// A list of the next submission waits for a memory value.
    Value(crate::d3d12::command_list::ValueWait),
    /// A submission: the lists' host sides, taken from the title's lists
    /// (which it may reset meanwhile), and the order they run in.
    Run(Vec<crate::d3d12::command_list::Detached>, Vec<usize>),
    /// A signal of the queue's, due after what it holds before it.
    Signal(usize, u64),
}

/// The queues that hold something (their addresses).
pub(super) static WAITING: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

pub(super) fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether queues wait for fences. `OATHKEEP_QUEUE_WAIT=0`: they do not,
/// and every submission runs when it is made (as before).
pub(super) fn waits() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_QUEUE_WAIT").is_none_or(|v| v != "0"))
}

/// Whether a submission is held list by list: the lists before the first
/// one whose wait for a memory value is not met run at once, and each list
/// that waits is held with the lists after it, behind its own waits.
/// Holding a submission whole made the title's queues wait for each other:
/// a graphics submission was held for a value a compute list writes, and
/// that compute list for a value an earlier list of the same graphics
/// submission writes. Nothing moved until the present forced it, and the
/// title reached its present only after waiting about 2.5 s, every frame
/// (runs 27-30: 2.4-2.9 s from the last submission to the present call,
/// 13-32 ms before waits were honoured; the frame's main graphics
/// submissions, 88 lists, ran forced, the compute lists before them).
/// On by default; `OATHKEEP_SPLIT_WAITS=0` holds whole submissions, as
/// before.
pub(super) fn split() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_SPLIT_WAITS").is_none_or(|v| v != "0"))
}

/// Whether a list is cut into host lists at its waits for memory values
/// (`crate::d3d12::command_list::cut`), and a queue runs a submission part by part: it
/// stands at each wait until the value is in memory, with everything
/// recorded before the wait run and its bottom-of-pipe writes done, as on
/// the console. Held list by list ([`split`]) a wait still held its whole
/// list: the title's lists are chains of jobs, each behind a wait for a
/// value another queue's list writes in the middle of its own chain
/// (frame 858 of run 33: a compute list of 433 commands with 13 waits;
/// graphics lists of the form "341 dispatches, wait, draws"), so two lists
/// each waited for a write behind the other's wait and only the present
/// moved them, in the wrong order.
/// On by default where waits are honoured list by list;
/// `OATHKEEP_CUT_WAITS=0` holds whole lists, as before.
pub(crate) fn cuts() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    waits() && split() && *ON.get_or_init(|| std::env::var_os("OATHKEEP_CUT_WAITS").is_none_or(|v| v != "0"))
}

/// Whether a present runs what queues still hold (on by default: a wait
/// is not kept across frames). `OATHKEEP_WAIT_FORCE=0` keeps it held: for
/// finding out whether the title's waits reach across its presents; a
/// wait nothing ever meets then stops the title for good.
pub(super) fn forces() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WAIT_FORCE").is_none_or(|v| v != "0"))
}

/// A submission to a queue whose lists are cut at their waits ([`cuts`]).
/// The lists before the first one that waits run at once if the queue
/// holds nothing; the rest goes behind what the queue holds, part by part
/// (`crate::d3d12::command_list::detach_parts`): the waits of a part, then its host
/// lists. [`release_held`] runs the queue's line as far as the values are
/// there, now and whenever a submission has run.
pub(super) fn submit_cut(this: This, lists: &[This]) {
    let Some(host) = crate::backend::host() else {
        return;
    };
    let first = if lock(&queue(this).held).is_empty() {
        lists
            .iter()
            .position(|&l| crate::d3d12::command_list::cut_list(l))
            .unwrap_or(lists.len())
    } else {
        0
    };
    #[cfg(feature = "diagnostics")]
    crate::diagnostics::capture::op(this as usize, || {
        let named: Vec<String> = lists
            .iter()
            .map(|&l| match crate::d3d12::command_list::host_list(l) {
                Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                None => format!("title {l:?}"),
            })
            .collect();
        format!(
            "SUBMIT queue kind {:?} held {} lists {named:?} (the first {first} run at once, the rest part by part)",
            Some(queue(this).kind),
            first < lists.len()
        )
    });
    if first > 0 && crate::backend::late() {
        // Sent to the card and not waited for: their host sides are taken
        // from the title's lists (which it may reset before they have run)
        // and come back, with their tables' slots, when they are settled.
        let (taken, order) = crate::d3d12::command_list::detach(&lists[..first]);
        host.execute_late(taken, order);
    } else if first > 0 {
        let now: Vec<_> = lists[..first]
            .iter()
            .filter_map(|&l| crate::d3d12::command_list::host_list(l))
            .collect();
        host.execute(&now);
        // They have run: their descriptor tables' slots are free again, as
        // on the path without cuts ([`execute`]). This path replaced that
        // one without the call, so the lists that run at once kept their
        // slots until the title's reset and the ring ran out as before
        // (run 45: 3,143 of 15,942 tables refused in one frame).
        crate::d3d12::command_list::tables_done(&lists[..first]);
    }
    if first < lists.len() {
        let parts = crate::d3d12::command_list::detach_parts(&lists[first..]);
        static SEEN: AtomicU64 = AtomicU64::new(0);
        if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
            tracing::info!(
                kind = queue(this).kind,
                lists = lists.len(),
                run_at_once = first,
                parts = parts.len(),
                waits = parts.iter().map(|p| p.0.len()).sum::<usize>(),
                "a submission is given to its queue part by part (its lists wait for memory values)"
            );
        }
        {
            let mut held = lock(&queue(this).held);
            for (waits, taken, order) in parts {
                held.extend(waits.into_iter().map(Held::Value));
                held.push(Held::Run(taken, order));
            }
        }
        let mut waiting = lock(&WAITING);
        if !waiting.contains(&(this as usize)) {
            waiting.push(this as usize);
        }
    }
    // What ran may be what another queue waits for, and what was just
    // given to the queue may be free to run already.
    release_held(false);
}

/// A submission to a queue that honours waits, list by list ([`split`]).
/// shortcut: a wait still holds its whole list, not only the commands
/// after it; and lists named twice among those held stay in one piece.
pub(super) fn submit_split(this: This, lists: &[This]) {
    let Some(host) = crate::backend::host() else {
        return;
    };
    let by = crate::d3d12::command_list::waits_by_list(lists);
    // Behind what the queue already holds, nothing runs now.
    let cut = if lock(&queue(this).held).is_empty() {
        (0..lists.len())
            .find(|&i| by[i].iter().any(|w| !w.met()))
            .unwrap_or(lists.len())
    } else {
        0
    };
    #[cfg(feature = "diagnostics")]
    crate::diagnostics::capture::op(this as usize, || {
        let named: Vec<String> = lists
            .iter()
            .map(|&l| match crate::d3d12::command_list::host_list(l) {
                Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                None => format!("title {l:?}"),
            })
            .collect();
        format!(
            "SUBMIT queue kind {:?} held {} lists {named:?} (the first {cut} run at once)",
            Some(queue(this).kind),
            cut < lists.len()
        )
    });
    if cut > 0 {
        let now: Vec<_> = lists[..cut]
            .iter()
            .filter_map(|&l| crate::d3d12::command_list::host_list(l))
            .collect();
        host.execute(&now);
    }
    if cut < lists.len() {
        static SEEN: AtomicU64 = AtomicU64::new(0);
        if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
            tracing::info!(
                kind = queue(this).kind,
                lists = lists.len(),
                run_at_once = cut,
                waits = ?by[cut],
                "lists of a submission are held for a memory value"
            );
        }
        let rest = &lists[cut..];
        // A list's host side can be taken once.
        let twice = rest.iter().enumerate().any(|(i, l)| rest[..i].contains(l));
        let mut pieces: Vec<Held> = Vec::new();
        let mut from = cut;
        while from < lists.len() {
            // Up to the next list that waits for anything (met or not: it
            // is looked at again when its turn comes).
            let to = if twice {
                lists.len()
            } else {
                (from + 1..lists.len())
                    .find(|&i| !by[i].is_empty())
                    .unwrap_or(lists.len())
            };
            let waits: Vec<crate::d3d12::command_list::ValueWait> = by[from..to].concat();
            pieces.extend(waits.into_iter().map(Held::Value));
            let (taken, order) = crate::d3d12::command_list::detach(&lists[from..to]);
            pieces.push(Held::Run(taken, order));
            from = to;
        }
        lock(&queue(this).held).extend(pieces);
        let mut waiting = lock(&WAITING);
        if !waiting.contains(&(this as usize)) {
            waiting.push(this as usize);
        }
    }
    // What ran may be what another queue waits for, and what was just held
    // may be free to run already.
    release_held(false);
}

/// Adds `what()` to what the queue holds, if it holds anything (it waits
/// for a fence): whether it was added.
pub(super) fn hold(this: This, what: impl FnOnce() -> Held) -> bool {
    if this.is_null() {
        return false;
    }
    let mut held = lock(&queue(this).held);
    if held.is_empty() {
        return false;
    }
    held.push(what());
    true
}

/// Runs what waiting queues hold as far as their fences have been reached
/// (`all`: all of it), each queue's in the order it was given. Called
/// where a fence may have moved: after a signal and after a submission (a
/// list's bottom-of-pipe write may be into a fence), never from inside
/// one.
pub(crate) fn release_held(all: bool) {
    if async_submit::on() {
        // What queues stand at is looked at by the thread that runs them.
        async_submit::wake();
        return;
    }
    // One caller at a time, so a queue's submissions keep their order.
    static RELEASING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    if lock(&WAITING).is_empty() {
        return;
    }
    let _looking = crate::diagnostics::timeline::span("looking at what queues hold");
    let _one = lock(&RELEASING);
    loop {
        let queues = lock(&WAITING).clone();
        let mut moved = false;
        for q in queues {
            // SAFETY: a live queue: one released is taken off the list.
            let held = unsafe { &(*(q as *const Queue)).held };
            let mut held = lock(held);
            loop {
                match held.first() {
                    None => break,
                    Some(&Held::Wait(fence, value))
                        if !all && crate::d3d12::fence::completed(fence as This) < value =>
                    {
                        break;
                    }
                    Some(Held::Value(wait)) if !all && !value_met(wait) => break,
                    Some(_) => {}
                }
                match held.remove(0) {
                    Held::Wait(..) => {}
                    Held::Value(wait) => {
                        // Passed at a present without being met: what the
                        // queue stood at.
                        if all && !value_met(&wait) {
                            static SEEN: AtomicU64 = AtomicU64::new(0);
                            if SEEN.fetch_add(1, Ordering::Relaxed) < 16 {
                                tracing::warn!(
                                    queue = format_args!("{q:#x}"),
                                    ?wait,
                                    "a wait for a memory value was not met at a present: the queue runs on"
                                );
                            }
                        }
                    }
                    Held::Run(lists, order) => {
                        #[cfg(feature = "diagnostics")]
                        crate::diagnostics::capture::op(q, || {
                            format!(
                                "RUN held submission of {} lists (forced {all})",
                                order.len()
                            )
                        });
                        // Diagnostics: the part behind this one stands at
                        // waits that are met already, so the two could have
                        // been one host submission (one wait for the card,
                        // each range shaders write compared once).
                        #[cfg(feature = "diagnostics")]
                        {
                            let waits = held.iter().take_while(|h| matches!(h, Held::Value(_))).count();
                            if waits > 0
                                && matches!(held.get(waits), Some(Held::Run(..)))
                                && held[..waits].iter().all(|h| matches!(h, Held::Value(w) if w.met()))
                            {
                                crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::PartsTogether, 1);
                            }
                        }
                        if crate::backend::late()
                            && let Some(host) = crate::backend::host()
                        {
                            // Not waited for: the host keeps the lists
                            // until they are settled.
                            host.execute_late(lists, order);
                        } else {
                            let runs: Vec<&crate::backend::HostList> =
                                order.iter().map(|&i| lists[i].host()).collect();
                            let ran = match crate::backend::host() {
                                Some(host) => {
                                    host.execute(&runs);
                                    true
                                }
                                None => false,
                            };
                            // They have run: their host lists are used again.
                            if ran {
                                crate::d3d12::command_list::recycle(lists, &order);
                            }
                        }
                        if all {
                            #[cfg(feature = "diagnostics")]
                            crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::Forced, 1);
                            static ONCE: AtomicBool = AtomicBool::new(false);
                            if !ONCE.swap(true, Ordering::Relaxed) {
                                tracing::warn!(
                                    "a queue still held a submission at a present: its fence was not reached"
                                );
                            }
                        }
                    }
                    // Behind what was sent to the card and not waited
                    // for, if anything was.
                    Held::Signal(fence, value) => {
                        if !crate::backend::late_signal(fence, value) {
                            crate::d3d12::fence::set(fence as This, value);
                        }
                    }
                }
                moved = true;
            }
            if held.is_empty() {
                lock(&WAITING).retain(|&w| w != q);
            }
        }
        if !moved {
            break;
        }
    }
}

/// Before a submission of ours that is waited for, or a wait for flights by
/// a thread of the title's: what the title gave its queues before goes to the
/// card first ([`behind`]).
pub(crate) fn before_sync() {
    if async_submit::on() {
        async_submit::drain();
    }
}

/// Whether submissions go into the line of a thread of ours ([`behind`]).
pub(crate) fn line_on() -> bool {
    async_submit::on()
}

/// Lists in the line may have been translated ([`crate::d3d12::command_list::deferred`]).
pub(crate) fn line_moved() {
    async_submit::wake();
}

/// This thread does work the line waits for: it does not wait for the line.
pub(crate) fn no_drain_here() {
    async_submit::no_drain_here();
}

/// Submissions the title made that the host has not begun ([`behind`]).
pub(crate) fn queued() -> u64 {
    async_submit::queued()
}

/// For the frame-rate line.
pub(crate) fn behind_line(frames: f64) {
    if async_submit::on() {
        crate::d3d12::command_list::deferred::line(frames);
        async_submit::line(frames);
    }
}

/// Whether a queue may pass `wait`: memory holds the value, or, with
/// submissions sent to the card without a wait (`crate::backend::late`), it
/// will once those in the air have run.
pub(super) fn value_met(wait: &crate::d3d12::command_list::ValueWait) -> bool {
    wait.met() || (crate::backend::late() && crate::backend::late_met(wait))
}
