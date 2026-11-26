use super::chrome::chrome;
use super::summary::summary;
use super::trace::{Part, Trace};
use super::{Names, OUTSIDE, Threads, ms};
use crate::diagnostics::timeline::record::{Event, Kind};

fn event(kind: Kind, name: &'static str, thread: u32, t: (u64, u64), a: u64, b: u64) -> Event {
    Event {
        t0: t.0,
        t1: t.1,
        a,
        b,
        name,
        thread,
        stack: 0,
        cpu: 0,
        kind,
    }
}

fn causes(parts: &[Part]) -> Vec<(u32, u64, u64, &str)> {
    parts.iter().map(|p| (p.thread, p.from, p.to, p.cause.as_str())).collect()
}

/// The present's thread waited for the title's, which waited for the
/// relay, which waited for the card, whose submission a third thread
/// sent: the chain goes back through all of them, end to end.
#[test]
fn a_chain_goes_back_through_who_woke_whom_and_the_card() {
    let events = vec![
        event(Kind::Frame, "present", 1, (1000, 1000), 0, 0),
        event(Kind::Frame, "present", 1, (2000, 2000), 0, 0),
        // Thread 1 presents after waiting for object 7.
        event(Kind::Span, "present", 1, (1900, 2000), 0, 0),
        event(Kind::Wait, "WaitForSingleObject", 1, (1100, 1850), 7, 0),
        event(Kind::Wake, "SetEvent", 2, (1840, 1840), 7, 0),
        // (An earlier wake of the same object, before the wait.)
        event(Kind::Wake, "SetEvent", 9, (1050, 1050), 7, 0),
        // Thread 2 landed flights after the card reached 12.
        event(Kind::Span, "landing", 2, (1700, 1845), 0, 0),
        event(Kind::Span, "merging", 2, (1750, 1800), 0, 0),
        event(Kind::Wait, "WaitForSingleObject", 2, (1200, 1690), 99, 12),
        // The card ran 11 and 12, both sent by thread 3; 11 found it idle.
        event(Kind::Sent, "ExecuteCommandLists", 3, (1290, 1300), 11, 1),
        event(Kind::Sent, "ExecuteCommandLists", 3, (1390, 1400), 12, 2),
        event(Kind::Span, "steps", 3, (1100, 1290), 0, 0),
    ];
    let trace = Trace::new(events, &[(1310, 1500), (1500, 1680)]);
    let chain = trace.chain(1, 2000, 1000);
    assert_eq!(
        causes(&chain),
        [
            (1, 1900, 2000, "present"),
            (1, 1850, 1900, OUTSIDE),
            (1, 1840, 1850, "woken, not running yet"),
            (2, 1800, 1840, "landing"),
            (2, 1750, 1800, "merging"),
            (2, 1700, 1750, "landing"),
            (2, 1690, 1700, OUTSIDE),
            (0, 1680, 1690, "the card was done, the wait not over yet"),
            (0, 1500, 1680, "the card running submissions"),
            (0, 1310, 1500, "the card running submissions"),
            (0, 1300, 1310, "sent, the card not started yet"),
            (3, 1290, 1300, "ExecuteCommandLists"),
            (3, 1100, 1290, "steps"),
            (3, 1000, 1100, OUTSIDE),
        ]
    );
    // End to end: no part of the frame is missing or counted twice.
    assert!(chain.windows(2).all(|p| p[0].from == p[1].to));
}

/// A wait nobody was seen to end (a timer) is a cause of its own, and
/// the chain goes on with the thread before it.
#[test]
fn a_wait_without_a_waker_stays_with_its_thread() {
    let events = vec![
        event(Kind::Wait, "Sleep", 1, (120, 180), 0, 0),
        event(Kind::Wait, "WaitOnAddress", 1, (20, 60), 5, 0),
        event(Kind::Wake, "WakeByAddressSingle", 1, (50, 50), 5, 0),
    ];
    let trace = Trace::new(events, &[]);
    assert_eq!(
        causes(&trace.chain(1, 200, 0)),
        [
            (1, 180, 200, OUTSIDE),
            (1, 120, 180, "a wait nobody seen here ended (Sleep)"),
            (1, 60, 120, OUTSIDE),
            (1, 20, 60, "a wait nobody seen here ended (WaitOnAddress)"),
            (1, 0, 20, OUTSIDE),
        ]
    );
}

/// Real threads, real waits: what the wrappers record names who ended
/// them, and the chain leads to the thread that worked.
#[test]
fn real_waits_are_recorded_with_who_ended_them() {
    use super::super::{hooks, record, span};
    use std::sync::{Arc, Condvar, Mutex};
    hooks::wrap_own();
    record::make_room();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let lock = Arc::new(Mutex::new(()));
    record::begin();
    record::frame();
    let first = super::super::record::ticks();
    let worker = {
        let (gate, lock) = (gate.clone(), lock.clone());
        std::thread::spawn(move || {
            let held = lock.lock().unwrap();
            let working = span("the work of a test");
            std::thread::sleep(std::time::Duration::from_millis(5));
            let spun = std::time::Instant::now();
            while spun.elapsed().as_millis() < 30 {
                std::hint::spin_loop();
            }
            working.end();
            drop(held);
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
            record::thread()
        })
    };
    {
        let mut open = gate.0.lock().unwrap();
        while !*open {
            open = gate.1.wait(open).unwrap();
        }
    }
    let last = super::super::record::ticks();
    let me = record::thread();
    let other = worker.join().unwrap();
    // A contended lock.
    let held = lock.lock().unwrap();
    let second = {
        let lock = lock.clone();
        std::thread::spawn(move || {
            drop(lock.lock().unwrap());
            record::thread()
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(20));
    drop(held);
    let second = second.join().unwrap();
    let data = record::end();
    assert_eq!(data.lost, 0);
    let trace = Trace::new(data.events, &[]);
    let chain = trace.chain(me, last, first);
    let work: u64 = chain.iter().filter(|p| p.thread == other && p.cause == "the work of a test").map(|p| p.to - p.from).sum();
    assert!(ms(work) >= 20.0, "{chain:?}");
    let contended = trace.waits[&second]
        .iter()
        .filter(|w| ms(w.t1 - w.t0) >= 10.0)
        .find_map(|w| trace.waker(second, w));
    assert_eq!(contended.map(|c| c.0), Some(me));
    let mut names = Names::new();
    let sites: Vec<String> = data.stacks.iter().map(|s| names.site(s)).collect();
    assert!(sites.iter().any(|s| s.contains("real_waits_are_recorded")), "{sites:?}");
    let mut json = Vec::new();
    chrome(&trace, &mut names, &data.stacks, &Threads::now(), &mut json).unwrap();
    assert!(json.starts_with(b"{\"traceEvents\""));
    let text = summary(&trace, &mut names, &data.stacks, &Threads::now(), &Threads::now());
    assert!(text.contains("frames in"));
}
