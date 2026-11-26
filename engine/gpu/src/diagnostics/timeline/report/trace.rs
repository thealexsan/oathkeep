//! Ordered trace representation and causal chain reconstruction.

use std::collections::HashMap;

use super::OUTSIDE;
use crate::diagnostics::timeline::record::{COPY_QUEUE, Event, Kind};

/// A part of a chain: what was done, or waited for, from when to when.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Part {
    pub(super) from: u64,
    pub(super) to: u64,
    /// The thread (0: the card).
    pub(super) thread: u32,
    pub(super) cause: String,
}

/// A wait that ended, as the chains see it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Wait {
    pub(super) t0: u64,
    pub(super) t1: u64,
    pub(super) object: u64,
    pub(super) fence: u64,
    pub(super) event: usize,
}

/// A submission: when it went to the card, from which thread, and from when
/// to when it ran there.
#[derive(Clone, Copy, Debug)]
pub(super) struct Run {
    pub(super) fence: u64,
    pub(super) t0: u64,
    pub(super) t1: u64,
    pub(super) thread: u32,
    pub(super) on: (u64, u64),
}

/// What was recorded, by thread and in order.
pub(super) struct Trace {
    pub(super) events: Vec<Event>,
    /// The spans of a thread (places among the events), by their beginning.
    pub(super) spans: HashMap<u32, Vec<usize>>,
    /// The waits of a thread, by their end.
    pub(super) waits: HashMap<u32, Vec<Wait>>,
    /// Who signalled an object when.
    wakes: HashMap<u64, Vec<(u64, u32)>>,
    /// The submissions, by the fence's value.
    pub(super) runs: Vec<Run>,
    pub(super) frames: Vec<(u64, u32)>,
}

impl Trace {
    /// `stamps`: the card's times of a submission by its place, as ticks.
    pub(super) fn new(mut events: Vec<Event>, stamps: &[(u64, u64)]) -> Trace {
        events.sort_by_key(|e| (e.t0, std::cmp::Reverse(e.t1)));
        let mut trace = Trace {
            events: Vec::new(),
            spans: HashMap::new(),
            waits: HashMap::new(),
            wakes: HashMap::new(),
            runs: Vec::new(),
            frames: Vec::new(),
        };
        for (i, e) in events.iter().enumerate() {
            match e.kind {
                Kind::Span | Kind::Calls | Kind::Kernel => trace.spans.entry(e.thread).or_default().push(i),
                Kind::Wait => trace.waits.entry(e.thread).or_default().push(Wait {
                    t0: e.t0,
                    t1: e.t1,
                    object: e.a,
                    fence: e.b,
                    event: i,
                }),
                Kind::Wake => trace.wakes.entry(e.a).or_default().push((e.t0, e.thread)),
                Kind::Sent => {
                    let on = (e.b as usize).checked_sub(1).and_then(|at| stamps.get(at)).copied().unwrap_or((0, 0));
                    trace.runs.push(Run {
                        fence: e.a,
                        t0: e.t0,
                        t1: e.t1,
                        thread: e.thread,
                        on,
                    });
                    trace.spans.entry(e.thread).or_default().push(i);
                }
                Kind::Frame => trace.frames.push((e.t1, e.thread)),
                Kind::Told | Kind::Poll | Kind::Sample => {}
            }
        }
        for waits in trace.waits.values_mut() {
            waits.sort_by_key(|w| w.t1);
        }
        trace.runs.sort_by_key(|r| r.fence);
        trace.events = events;
        trace
    }

    /// Who ended the wait `w` of `thread`, and when.
    pub(super) fn waker(&self, thread: u32, w: &Wait) -> Option<(u32, u64)> {
        let wakes = self.wakes.get(&w.object)?;
        let end = wakes.partition_point(|k| k.0 <= w.t1);
        wakes[..end].iter().rev().take_while(|k| k.0 >= w.t0).find(|k| k.1 != thread).map(|k| (k.1, k.0))
    }

    /// What `thread` was doing in `[from, to)`, the innermost span counting;
    /// the newest part first.
    fn doing(&self, thread: u32, from: u64, to: u64, out: &mut Vec<Part>) {
        if from >= to {
            return;
        }
        let none = Vec::new();
        let spans = self.spans.get(&thread).unwrap_or(&none);
        // The spans that touch it (a span is at most some frames long).
        let end = spans.partition_point(|&i| self.events[i].t0 < to);
        let touching: Vec<&Event> =
            spans[..end].iter().map(|&i| &self.events[i]).filter(|e| e.t1 > from).collect();
        let mut edges: Vec<u64> = vec![from, to];
        for e in &touching {
            edges.extend([e.t0, e.t1].iter().filter(|t| **t > from && **t < to));
        }
        edges.sort_unstable();
        edges.dedup();
        let mut parts: Vec<Part> = Vec::new();
        for pair in edges.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let inner = touching.iter().filter(|e| e.t0 <= a && e.t1 >= b).max_by_key(|e| (e.t0, std::cmp::Reverse(e.t1)));
            let cause = match inner {
                Some(e) if e.kind == Kind::Calls => {
                    // Of a run of list calls, the part that was ours.
                    let ours = (b - a) * e.b.min(e.t1 - e.t0) / (e.t1 - e.t0).max(1);
                    parts.push(Part {
                        from: a,
                        to: a + ours,
                        thread,
                        cause: "recording: inside our list calls".into(),
                    });
                    parts.push(Part {
                        from: a + ours,
                        to: b,
                        thread,
                        cause: "recording: between list calls (the caller's code)".into(),
                    });
                    continue;
                }
                Some(e) if e.kind == Kind::Kernel => format!("kernel: {}", e.name),
                Some(e) => e.name.to_string(),
                None => OUTSIDE.into(),
            };
            match parts.last_mut() {
                Some(last) if last.cause == cause => last.to = b,
                _ => parts.push(Part { from: a, to: b, thread, cause }),
            }
        }
        out.extend(parts.into_iter().rev().filter(|p| p.from < p.to));
    }

    /// The chain that ended at `t` on `thread`, back to `until`: what that
    /// thread did since its last wait ended, then what the thread that ended
    /// the wait had done, or the card, and so on. Newest first, end to end.
    pub(super) fn chain(&self, mut thread: u32, mut t: u64, until: u64) -> Vec<Part> {
        let mut out = Vec::new();
        for _ in 0..200_000 {
            if t <= until {
                break;
            }
            let waits = self.waits.get(&thread).map_or(&[][..], |w| w);
            let last = waits[..waits.partition_point(|w| w.t1 <= t)].last().filter(|w| w.t1 > until).copied();
            let Some(w) = last else {
                self.doing(thread, until, t, &mut out);
                break;
            };
            self.doing(thread, w.t1, t, &mut out);
            let name = self.events[w.event].name;
            if w.fence != 0 && w.fence != COPY_QUEUE {
                match self.card(w.fence, w.t1, until, &mut out) {
                    Some((sender, at)) if at < t => {
                        (thread, t) = (sender, at);
                        continue;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            match self.waker(thread, &w) {
                Some((other, at)) if at < t && w.fence == 0 => {
                    out.push(Part {
                        from: at.max(until),
                        to: w.t1,
                        thread,
                        cause: "woken, not running yet".into(),
                    });
                    (thread, t) = (other, at);
                }
                _ => {
                    let cause = match w.fence {
                        0 => format!("a wait nobody seen here ended ({name})"),
                        COPY_QUEUE => "the card's copy queue".into(),
                        _ => "the card (no times of it)".into(),
                    };
                    out.push(Part {
                        from: w.t0.max(until),
                        to: w.t1,
                        thread,
                        cause,
                    });
                    t = w.t0;
                }
            }
        }
        out.retain(|p| p.from < p.to);
        out
    }

    /// The card's part of a chain: it reached `fence` by `t`. Back through
    /// the submissions that stood in its queue, to the sending of one that
    /// found it idle: by whom, and when; `None` if that goes back before
    /// `until`, or (nothing put in) if the card's times are not known.
    fn card(&self, fence: u64, t: u64, until: u64, out: &mut Vec<Part>) -> Option<(u32, u64)> {
        let mut i = self.runs.partition_point(|r| r.fence < fence);
        let first = self.runs.get(i)?;
        if first.on == (0, 0) || first.t0 >= t {
            return Some((0, u64::MAX));
        }
        let part = |from: u64, to: u64, cause: &str| Part {
            from: from.max(until),
            to: to.max(until),
            thread: 0,
            cause: cause.into(),
        };
        let mut end = first.on.1.min(t);
        out.push(part(end, t, "the card was done, the wait not over yet"));
        loop {
            let run = self.runs[i];
            let begun = run.on.0.clamp(run.t0.min(end), end);
            out.push(part(begun, end, "the card running submissions"));
            if begun <= until {
                return None;
            }
            match i.checked_sub(1).map(|before| self.runs[before]) {
                Some(before) if before.on != (0, 0) && before.on.1 >= run.t0 => {
                    let to = before.on.1.min(begun);
                    out.push(part(to, begun, "the card between submissions"));
                    end = to;
                    i -= 1;
                    if end <= until {
                        return None;
                    }
                }
                _ => {
                    let sent = run.t1.min(begun);
                    out.push(part(sent, begun, "sent, the card not started yet"));
                    return Some((run.thread, sent));
                }
            }
        }
    }
}
