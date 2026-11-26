use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use super::lane::Work;
use super::{BUSY, FORCED, JOINED, QUEUED, RUNS, STATS, This, jobs_of};

type Recordings = std::sync::Arc<[std::sync::Arc<crate::d3d12::command_list::deferred::Recording>]>;

/// What the title's queues put in the line.
pub(super) enum Job {
    /// The queue waits for the fence (its address) to reach the value.
    Wait(usize, u64),
    /// A part of a submission waits for a memory value.
    Value(crate::d3d12::command_list::ValueWait),
    /// A submission, or a part of one: the lists' host sides and the order
    /// they run in.
    Run(usize, Vec<crate::d3d12::command_list::Detached>, Vec<usize>),
    /// A submission whose lists are being translated ([`crate::d3d12::command_list::deferred`]):
    /// the parts of it are known when they are.
    Lists(usize, Recordings),
    /// Parts taken along with a submission ([`crate::backend::joins`]): where they
    /// begin in the order, and the signals given behind them.
    Joined(usize, Vec<crate::d3d12::command_list::Detached>, Vec<usize>, Vec<usize>, Vec<(usize, u64)>),
    /// A signal of the queue's.
    Signal(usize, u64),
    /// A frame is shown: the base texture (and optional overlay plane) and the states their lists leave them in.
    Present(
        Option<(std::sync::Arc<crate::d3d12::resource::HostTexture>, u32)>,
        Option<(std::sync::Arc<crate::d3d12::resource::HostTexture>, u32)>,
        super::super::Shown,
    ),
}

// SAFETY: D3D12 objects are free-threaded; a job is touched by whoever put it
// in the line and then, once it is taken out, by our thread alone (the flights
// it becomes are handed to other threads the same way).
unsafe impl Send for Job {}

pub(super) enum Ask {
    Wait(usize, u64),
    Value(crate::d3d12::command_list::ValueWait),
    Lists(Recordings),
}

impl Work for Job {
    type Ask = Ask;

    fn ask(&self) -> Option<Ask> {
        match self {
            &Job::Wait(fence, value) => Some(Ask::Wait(fence, value)),
            Job::Value(wait) => Some(Ask::Value(*wait)),
            Job::Lists(_, lists) => Some(Ask::Lists(lists.clone())),
            _ => None,
        }
    }

    fn folded(&self) -> bool {
        matches!(self, Job::Lists(..))
    }

    fn unfold(self) -> Vec<Self> {
        let Job::Lists(queue, recordings) = self else {
            return vec![self];
        };
        let lists: Vec<This> = recordings.iter().map(|r| r.back()).collect();
        let jobs = jobs_of(queue as This, &lists);
        // It stood for one submission not begun.
        let runs = jobs.iter().filter(|j| j.runs()).count() as u64;
        QUEUED.fetch_add(runs, Relaxed);
        QUEUED.fetch_sub(1, Relaxed);
        STATS[RUNS].fetch_add(runs, Relaxed);
        jobs
    }

    fn settle(ask: &Ask) {
        if let Ask::Lists(recordings) = ask {
            recordings.iter().for_each(|r| r.wait());
        }
    }

    fn met(ask: &Ask) -> bool {
        match ask {
            &Ask::Wait(fence, value) => crate::d3d12::fence::completed(fence as This) >= value,
            Ask::Value(wait) => super::super::value_met(wait),
            Ask::Lists(recordings) => recordings.iter().all(|r| r.done()),
        }
    }

    fn shows(&self) -> bool {
        matches!(self, Job::Present(..))
    }

    fn runs(&self) -> bool {
        matches!(self, Job::Run(..) | Job::Joined(..) | Job::Lists(..))
    }

    fn take_along(&mut self, next: Job) -> Result<bool, Job> {
        let (most, most_lists) = crate::backend::joins();
        if most < 2 {
            return Err(next);
        }
        // Nothing but waits and signals that a submission may follow.
        let (count, lists, signals) = match self {
            Job::Run(_, lists, order) => (1, (&*lists, &*order), &[][..]),
            Job::Joined(_, lists, order, parts, signals) => (1 + parts.len(), (&*lists, &*order), &signals[..]),
            _ => return Err(next),
        };
        let hosts = || lists.1.iter().map(|&i| lists.0[i].host());
        match &next {
            Job::Value(wait) => {
                let met = wait.met()
                    || crate::backend::late_met_known(wait) == Some(true)
                    || hosts().any(|list| crate::backend::will_meet(list, wait));
                if !met {
                    return Err(next);
                }
            }
            &Job::Wait(fence, value) => {
                if crate::d3d12::fence::completed(fence as This) < value && !signals.iter().any(|&(f, v)| f == fence && v >= value) {
                    return Err(next);
                }
            }
            Job::Signal(..) => {}
            Job::Run(_, more, order) => {
                let runs: Vec<&crate::backend::HostList> = order.iter().map(|&i| more[i].host()).collect();
                if count >= most || lists.1.len() + runs.len() > most_lists || !crate::backend::joinable(hosts(), &runs) {
                    return Err(next);
                }
            }
            Job::Joined(..) | Job::Present(..) | Job::Lists(..) => return Err(next),
        }
        let taken = std::mem::replace(self, Job::Wait(0, 0));
        let (queue, mut lists, mut order, mut parts, mut signals) = match taken {
            Job::Run(queue, lists, order) => (queue, lists, order, Vec::new(), Vec::new()),
            Job::Joined(queue, lists, order, parts, signals) => (queue, lists, order, parts, signals),
            other => {
                *self = other;
                return Err(next);
            }
        };
        let run = matches!(next, Job::Run(..));
        match next {
            Job::Run(_, more, then) => {
                parts.push(order.len());
                order.extend(then.into_iter().map(|i| i + lists.len()));
                lists.extend(more);
                STATS[JOINED].fetch_add(1, Relaxed);
            }
            Job::Signal(fence, value) => signals.push((fence, value)),
            _ => {}
        }
        *self = Job::Joined(queue, lists, order, parts, signals);
        Ok(run)
    }

    fn go(self, forced: bool) {
        let began = Instant::now();
        let _doing = crate::diagnostics::timeline::span(match self {
            Job::Run(..) => "the line: a submission",
            Job::Joined(..) => "the line: submissions joined",
            Job::Present(..) => "the line: a present",
            Job::Signal(..) => "the line: a signal",
            _ => "the line: a wait that is met",
        });
        if forced {
            STATS[FORCED].fetch_add(1, Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::Forced, 1);
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("a queue still stood at a wait at a present: it was not met"));
        }
        match self {
            // (A folded one is unfolded, never done.)
            Job::Wait(..) | Job::Value(..) | Job::Lists(..) => {}
            Job::Run(queue, lists, order) => {
                let _ = queue;
                #[cfg(feature = "diagnostics")]
                crate::diagnostics::capture::op(queue, || format!("RUN submission of {} lists (its thread's)", order.len()));
                match crate::backend::host() {
                    Some(host) => host.execute_late(lists, order),
                    None => drop(lists),
                }
                QUEUED.fetch_sub(1, Relaxed);
            }
            Job::Joined(queue, lists, order, parts, signals) => {
                let _ = queue;
                #[cfg(feature = "diagnostics")]
                crate::diagnostics::capture::op(queue, || {
                    format!("RUN {} parts of submissions in one, {} lists (its thread's)", 1 + parts.len(), order.len())
                });
                match crate::backend::host() {
                    Some(host) => host.execute_parts(lists, order, &parts),
                    None => drop(lists),
                }
                // (A part that had no list is one all the same.)
                QUEUED.fetch_sub(1 + parts.len() as u64, Relaxed);
                for (fence, value) in signals {
                    if !crate::backend::late_signal(fence, value) {
                        crate::d3d12::fence::set(fence as This, value);
                    }
                }
            }
            Job::Signal(fence, value) => {
                if !crate::backend::late_signal(fence, value) {
                    crate::d3d12::fence::set(fence as This, value);
                }
            }
            Job::Present(texture, overlay, shown) => {
                if let Some((texture, state)) = texture {
                    // (Where its lists were not translated when the title
                    // presented: the state they left it in.)
                    let state = if state == u32::MAX { texture.state.load(Relaxed) } else { state };
                    let over = overlay.as_ref().map(|(ov, ov_state)| {
                        let st = if *ov_state == u32::MAX { ov.state.load(Relaxed) } else { *ov_state };
                        (&ov.texture, st)
                    });
                    super::super::show_planes(&texture.texture, state, over, shown);
                }
                QUEUED.fetch_sub(1, Relaxed);
            }
        }
        STATS[BUSY].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}
