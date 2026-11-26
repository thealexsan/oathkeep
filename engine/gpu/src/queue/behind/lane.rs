use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// One thing in the line: what it stands at, if it does, and what it does.
pub(super) trait Work: Send {
    /// What has to be met before it goes.
    type Ask;
    fn ask(&self) -> Option<Self::Ask>;
    fn met(ask: &Self::Ask) -> bool;
    /// Whether it is a present: everything before it goes first, met or not.
    fn shows(&self) -> bool;
    /// Whether it is a submission.
    fn runs(&self) -> bool;
    /// Whether it stands for other work, which takes its place in the line
    /// once it is free to go ([`Work::unfold`]).
    fn folded(&self) -> bool {
        false
    }
    fn unfold(self) -> Vec<Self>
    where
        Self: Sized,
    {
        vec![self]
    }
    /// Waits until a folded one is free to go.
    fn settle(_: &Self::Ask) {}
    /// Takes `next`, what its queue was given behind it, along to be done with
    /// it; or gives it back. Whether it is a submission that was taken.
    fn take_along(&mut self, next: Self) -> Result<bool, Self>
    where
        Self: Sized,
    {
        Err(next)
    }
    /// Does it. `forced`: its wait was not met.
    fn go(self, forced: bool);
}

pub(super) struct Entry<W> {
    number: u64,
    queue: usize,
    what: W,
}

pub(super) struct State<W> {
    /// In the order of the title's calls, whatever queue they were made on.
    pub(super) items: VecDeque<Entry<W>>,
    /// The number the next entry gets.
    pub(super) next: u64,
    /// The queue whose entry is being done now (it is no longer in `items`;
    /// what that queue is given stays behind it all the same).
    pub(super) running: Option<usize>,
    /// Times a look at the line was asked for, and the last of them that a
    /// whole look has followed.
    asked: u64,
    served: u64,
    /// Presents and submissions in the line or being done.
    pub(super) presents: usize,
    pub(super) runs: usize,
}

/// See the module's text.
pub(super) struct Lane<W: Work> {
    state: Mutex<State<W>>,
    /// Something to look at.
    work: Condvar,
    /// Something was done, or a look at the line ended.
    moved: Condvar,
}

impl<W: Work> Lane<W> {
    pub(super) const fn new() -> Self {
        Lane {
            state: Mutex::new(State {
                items: VecDeque::new(),
                next: 0,
                running: None,
                asked: 0,
                served: 0,
                presents: 0,
                runs: 0,
            }),
            work: Condvar::new(),
            moved: Condvar::new(),
        }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, State<W>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn put(state: &mut State<W>, queue: usize, what: W) {
        state.presents += usize::from(what.shows());
        state.runs += usize::from(what.runs());
        let number = state.next;
        state.next += 1;
        state.items.push_back(Entry { number, queue, what });
    }

    /// Adds to the end of the line, and has it looked at.
    pub(super) fn push(&self, queue: usize, what: impl IntoIterator<Item = W>) {
        let mut state = self.lock();
        for what in what {
            Self::put(&mut state, queue, what);
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Adds `what` if `queue` has anything in the line or being done (or
    /// `always`); else does `now`, with nothing of the queue's able to come
    /// in between.
    pub(super) fn push_behind(&self, queue: usize, always: bool, what: W, now: impl FnOnce()) {
        let mut state = self.lock();
        if always || state.running == Some(queue) || state.items.iter().any(|e| e.queue == queue) {
            Self::put(&mut state, queue, what);
        } else {
            now();
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Has the line looked at: something an entry stands at may have moved.
    pub(super) fn wake(&self) {
        let mut state = self.lock();
        if state.items.is_empty() {
            return;
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Waits until everything in the line that is free to run has been done
    /// (what comes in after this call may not be).
    pub(super) fn drain(&self) {
        let folded: Vec<W::Ask> =
            self.lock().items.iter().filter(|e| e.what.folded()).filter_map(|e| e.what.ask()).collect();
        folded.iter().for_each(W::settle);
        let mut state = self.lock();
        if state.items.is_empty() && state.running.is_none() {
            return;
        }
        state.asked += 1;
        let ticket = state.asked;
        self.work.notify_one();
        while state.served < ticket {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits until at most `most` presents are in the line.
    pub(super) fn wait_presents(&self, most: usize) {
        let mut state = self.lock();
        while state.presents > most {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits until at most `most` submissions are in the line, or what is
    /// left of it stands at something.
    pub(super) fn wait_runs(&self, most: usize) {
        let mut state = self.lock();
        while state.runs > most && state.served < state.asked {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Looks at the line for as long as the process runs.
    pub(super) fn serve(&self) -> ! {
        loop {
            let (asked, limit) = {
                let mut state = self.lock();
                while state.asked == state.served {
                    if state.items.is_empty() {
                        state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
                    } else {
                        // It stands at something the CPU may write without
                        // a call of ours.
                        let (back, out) =
                            self.work.wait_timeout(state, Duration::from_millis(1)).unwrap_or_else(|e| e.into_inner());
                        state = back;
                        if out.timed_out() {
                            break;
                        }
                    }
                }
                (state.asked, state.next)
            };
            self.look(limit);
            let mut state = self.lock();
            state.served = state.served.max(asked);
            drop(state);
            self.moved.notify_all();
        }
    }

    /// Does what is free to run of the entries before number `limit`.
    pub(super) fn look(&self, limit: u64) {
        while self.step(limit) {}
    }

    /// The parts of `entry`, which stands for them, in place `at`.
    fn unfold(state: &mut State<W>, at: usize, entry: Entry<W>) {
        let (number, queue, runs) = (entry.number, entry.queue, usize::from(entry.what.runs()));
        let parts = entry.what.unfold();
        state.runs -= runs;
        state.runs += parts.iter().filter(|p| p.runs()).count();
        for (i, what) in parts.into_iter().enumerate() {
            state.items.insert(at + i, Entry { number, queue, what });
        }
    }

    /// Does one entry: the oldest that is the first of its queue and free to
    /// run. Whether there was one. Only one thread calls this.
    fn step(&self, limit: u64) -> bool {
        // Each queue's first entry, oldest first.
        let mut heads: Vec<(u64, usize, Option<W::Ask>, bool)> = Vec::new();
        // Whether one of them stands for others not known yet.
        let mut folded = false;
        {
            let state = self.lock();
            for e in state.items.iter().take_while(|e| e.number < limit) {
                if !heads.iter().any(|h| h.1 == e.queue) {
                    heads.push((e.number, e.queue, e.what.ask(), e.what.shows()));
                    folded |= e.what.folded();
                }
            }
        }
        let Some(&(first, ..)) = heads.first() else {
            return false;
        };
        let ready = heads.iter().find(|(number, _, ask, shows)| match ask {
            Some(ask) => W::met(ask),
            // A present comes behind everything given before it.
            None => !shows || *number == first || !super::super::forces(),
        });
        let (number, forced) = match ready {
            Some(head) => (head.0, false),
            None => {
                // Nothing is free to run. With a present in the line the
                // oldest goes on regardless: a wait is not kept across
                // frames (`super::present`). (Not while there is work whose
                // parts are not known: it may be what is waited for.)
                if folded {
                    return false;
                }
                let state = self.lock();
                if !super::super::forces() || !state.items.iter().any(|e| e.number < limit && e.what.shows()) {
                    return false;
                }
                (first, true)
            }
        };
        let mut entry = {
            let mut state = self.lock();
            let Some(at) = state.items.iter().position(|e| e.number == number) else {
                return false;
            };
            let Some(entry) = state.items.remove(at) else {
                return false;
            };
            state.running = Some(entry.queue);
            entry
        };
        let (shows, mut runs) = (entry.what.shows(), usize::from(entry.what.runs()));
        if entry.what.folded() {
            // Its parts take its place: behind everything before it, before
            // everything after it (they share its number; the first of them
            // is found first).
            let mut state = self.lock();
            state.running = None;
            let at = state.items.iter().position(|e| e.number > entry.number).unwrap_or(state.items.len());
            Self::unfold(&mut state, at, entry);
            drop(state);
            self.moved.notify_all();
            return true;
        }
        // What follows it in its queue, if that can go with it. (Nobody else
        // takes anything out, and what comes in goes to the end.)
        while runs > 0 && !forced {
            let next = {
                let mut state = self.lock();
                let at = state.items.iter().position(|e| e.queue == entry.queue && e.number < limit);
                at.and_then(|at| Some((at, state.items.remove(at)?)))
            };
            let Some((at, next)) = next else {
                break;
            };
            // (Back where it was: parts of one submission share a number.)
            if next.what.folded() {
                let free = next.what.ask().is_none_or(|ask| W::met(&ask));
                let mut state = self.lock();
                if free {
                    Self::unfold(&mut state, at, next);
                    continue;
                }
                state.items.insert(at, next);
                break;
            }
            let Entry { number, queue, what } = next;
            match entry.what.take_along(what) {
                Ok(run) => runs += usize::from(run),
                Err(what) => {
                    self.lock().items.insert(at, Entry { number, queue, what });
                    break;
                }
            }
        }
        entry.what.go(forced);
        {
            let mut state = self.lock();
            state.running = None;
            state.presents -= usize::from(shows);
            state.runs -= runs;
        }
        self.moved.notify_all();
        true
    }
}
