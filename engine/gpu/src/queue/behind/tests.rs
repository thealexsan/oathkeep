use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::This;
use super::lane::{Lane, Work};

type Log = Arc<Mutex<Vec<String>>>;

/// A job of a test: what it stands at, and what it does.
enum T {
    /// Stands until the fence has the value.
    Wait(usize, u64),
    Run(&'static str, Log),
    /// Runs once the gate is open: a submission that takes long.
    Slow(&'static str, Log, Arc<(Mutex<bool>, Condvar)>),
    Signal(usize, u64, Log),
    Present(&'static str, Log),
    /// Takes others of its kind, waits that are met, and signals along.
    Group(Vec<&'static str>, Log, Vec<(usize, u64)>),
}

impl Work for T {
    type Ask = (usize, u64);
    fn ask(&self) -> Option<(usize, u64)> {
        match self {
            &T::Wait(fence, value) => Some((fence, value)),
            _ => None,
        }
    }
    fn met(ask: &(usize, u64)) -> bool {
        crate::fence::completed(ask.0 as This) >= ask.1
    }
    fn shows(&self) -> bool {
        matches!(self, T::Present(..))
    }
    fn runs(&self) -> bool {
        matches!(self, T::Run(..) | T::Slow(..) | T::Group(..))
    }
    fn take_along(&mut self, next: T) -> Result<bool, T> {
        let T::Group(names, _, signals) = self else {
            return Err(next);
        };
        match next {
            T::Group(more, ..) if names.len() < 3 => {
                names.extend(more);
                Ok(true)
            }
            T::Wait(f, v) if crate::fence::completed(f as This) >= v || signals.contains(&(f, v)) => Ok(false),
            T::Signal(f, v, _) => {
                signals.push((f, v));
                Ok(false)
            }
            other => Err(other),
        }
    }
    fn go(self, forced: bool) {
        match self {
            T::Wait(..) => {
                if forced {
                    FORCED_HERE.with(|f| f.set(f.get() + 1));
                }
            }
            T::Run(name, log) | T::Present(name, log) => log.lock().unwrap().push(name.into()),
            T::Slow(name, log, gate) => {
                let mut open = gate.0.lock().unwrap();
                while !*open {
                    open = gate.1.wait(open).unwrap();
                }
                log.lock().unwrap().push(name.into());
            }
            T::Signal(fence, value, log) => {
                log.lock().unwrap().push(format!("signal {value}"));
                crate::fence::set(fence as This, value);
            }
            T::Group(names, log, signals) => {
                log.lock().unwrap().push(names.join("+"));
                for (fence, value) in signals {
                    crate::fence::set(fence as This, value);
                }
            }
        }
    }
}

thread_local! {
    static FORCED_HERE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn fence() -> usize {
    let mut f = std::ptr::null();
    assert_eq!(
        crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut f),
        super::super::S_OK
    );
    f as usize
}

fn value(f: usize) -> u64 {
    crate::fence::completed(f as This)
}

fn look(lane: &Lane<T>) {
    let limit = lane.lock().next;
    lane.look(limit);
}

/// A queue's order is kept, a wait for another queue's signal holds only
/// what is behind it on its own queue, and the oldest goes first.
#[test]
fn queues_keep_their_order_and_wait_for_each_others_fences() {
    let lane = Lane::new();
    let log: Log = Default::default();
    let f = fence();
    let run = |name| T::Run(name, log.clone());
    // The compute queue waits for what the graphics queue signals later.
    lane.push(2, [T::Wait(f, 1), run("compute")]);
    lane.push(1, [run("draw 1")]);
    lane.push_behind(1, false, T::Signal(f, 1, log.clone()), || panic!("the queue is not empty"));
    lane.push(1, [run("draw 2")]);
    look(&lane);
    assert_eq!(*log.lock().unwrap(), ["draw 1", "signal 1", "compute", "draw 2"]);
    assert_eq!(value(f), 1);
    // Nothing in the line: a signal is at once.
    let mut done = false;
    lane.push_behind(1, false, T::Signal(f, 2, log.clone()), || done = true);
    assert!(done);
}

/// A submission takes what follows it in its queue along, up to what it
/// cannot take, which stays at its place; the counts come out even.
#[test]
fn submissions_take_those_behind_them_along() {
    let lane = Lane::new();
    let log: Log = Default::default();
    let f = fence();
    let part = |name| T::Group(vec![name], log.clone(), Vec::new());
    lane.push(1, [part("a"), T::Wait(f, 0), part("b")]);
    lane.push(2, [part("other queue"), T::Wait(f, 1), part("behind a's signal")]);
    lane.push(1, [T::Signal(f, 1, log.clone()), T::Wait(f, 1), part("c"), part("d")]);
    lane.push(1, [T::Wait(f, 9), part("held"), T::Present("present", log.clone()), part("next frame")]);
    // What came after the look was asked for is not touched.
    let limit = lane.lock().next;
    lane.push(1, [part("later")]);
    lane.look(limit);
    assert_eq!(
        *log.lock().unwrap(),
        ["a+b+c", "other queue+behind a's signal", "d", "held", "present", "next frame"],
        "{:?}",
        lane.lock().items.len()
    );
    look(&lane);
    assert_eq!(log.lock().unwrap().last().unwrap(), "later");
    let state = lane.lock();
    assert_eq!((state.presents, state.runs, state.items.len()), (0, 0, 0));
}

/// A wait that nothing meets holds its queue until a present, which
/// comes behind everything given before it and before what comes after.
#[test]
fn a_present_goes_behind_everything_before_it_met_or_not() {
    let lane = Lane::new();
    let log: Log = Default::default();
    let f = fence();
    let run = |name| T::Run(name, log.clone());
    lane.push(2, [T::Wait(f, 9), run("held")]);
    lane.push(1, [run("draw")]);
    look(&lane);
    assert_eq!(*log.lock().unwrap(), ["draw"], "the wait holds its queue and no other");
    lane.push(1, [T::Present("present", log.clone())]);
    lane.push(1, [run("next frame")]);
    lane.push(2, [T::Wait(f, 10), run("held again")]);
    look(&lane);
    assert_eq!(*log.lock().unwrap(), ["draw", "held", "present", "next frame"]);
    assert_eq!(FORCED_HERE.get(), 1);
    // The second wait is met by a signal of the CPU.
    crate::fence::set(f as This, 10);
    look(&lane);
    assert_eq!(log.lock().unwrap().last().unwrap(), "held again");
    assert_eq!(FORCED_HERE.get(), 1);
    let state = lane.lock();
    assert_eq!((state.presents, state.runs, state.items.len()), (0, 0, 0));
}

/// With the thread: a fence behind a submission is not reached before the
/// submission has been done, presents hold the title back a frame later,
/// and a drain comes back when everything was done.
#[test]
fn the_title_goes_on_and_is_told_in_order() {
    let lane: &'static Lane<T> = Box::leak(Box::new(Lane::new()));
    std::thread::spawn(|| lane.serve());
    let log: Log = Default::default();
    let f = fence();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    lane.push(1, [T::Slow("slow", log.clone(), gate.clone())]);
    lane.push_behind(1, false, T::Signal(f, 1, log.clone()), || panic!("behind the submission"));
    lane.push(1, [T::Present("present 1", log.clone())]);
    // One present may be in the line: this returns with it there.
    lane.wait_presents(1);
    assert_eq!(value(f), 0);
    assert!(log.lock().unwrap().is_empty());
    // A fence behind a submission being done (no longer in the line).
    while lane.lock().running.is_none() {
        std::thread::yield_now();
    }
    lane.push(1, [T::Run("second", log.clone())]);
    lane.push(1, [T::Present("present 2", log.clone())]);
    let opener = {
        let gate = gate.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        })
    };
    lane.wait_presents(1);
    assert!(value(f) == 1, "the first frame was done");
    lane.drain();
    assert_eq!(*log.lock().unwrap(), ["slow", "signal 1", "present 1", "second", "present 2"]);
    opener.join().unwrap();
    // A wait the CPU meets without telling anybody; a drain does not
    // stand at it.
    lane.push(1, [T::Wait(f, 5), T::Run("after the wait", log.clone())]);
    lane.drain();
    assert_eq!(log.lock().unwrap().len(), 5);
    lane.wait_runs(0);
    crate::fence::set(f as This, 5);
    while log.lock().unwrap().len() < 6 {
        std::thread::yield_now();
    }
}
