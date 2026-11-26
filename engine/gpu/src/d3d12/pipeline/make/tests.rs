use super::*;
use super::workers::{Queue, worker_count, worker_count_for};

#[test]
fn worker_count_follows_the_processors_and_the_switch() {
    assert_eq!(worker_count(None, 1), 2);
    assert_eq!(worker_count(None, 24), 16);
    assert_eq!(worker_count(None, 64), 16);
    assert_eq!(worker_count(Some(0), 24), 1);
    assert_eq!(worker_count(Some(20), 24), 20);
    assert_eq!(worker_count_for(None, 8, true), 6);
    assert_eq!(worker_count_for(None, 16, true), 8);
    assert_eq!(worker_count_for(None, 24, true), 8);
    assert_eq!(worker_count_for(Some(12), 8, true), 12);
}

/// A first use queues the pipeline and, within the budget, gets the
/// worker's answer (here: no host pipeline for no shaders).
#[test]
fn first_use_is_made_by_a_worker() {
    let p = Arc::new(Pending::new(Vec::new(), None));
    assert!(ready(&p).is_none());
    assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
    let started = Instant::now();
    while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
        std::thread::yield_now();
    }
    assert!(matches!(p.made.get(), Some(None)));
}

#[test]
fn pending_pipeline_is_recorded_for_the_next_launch() {
    let mut p = Pending::new(Vec::new(), None);
    p.key = "pending-pipeline-test".to_owned();
    // Simulate a worker that has taken the job but cannot finish it yet.
    p.queue_state.store(MAKING, Ordering::Relaxed);
    let p = Arc::new(p);

    assert!(!super::super::library::wanted_early(&p.key));
    assert!(ready(&p).is_none());
    assert!(p.made.get().is_none());
    assert!(super::super::library::wanted_early(&p.key));
}

#[test]
fn draws_promote_early_pipelines_without_claiming_them_twice() {
    let earlier = Arc::new(Pending::new(Vec::new(), None));
    let promoted = Arc::new(Pending::new(Vec::new(), None));
    let requested = Arc::new(Pending::new(Vec::new(), None));
    let mut queue = Queue::new();

    assert_eq!(queue.push(&earlier, EARLY), Some(true));
    assert_eq!(queue.push(&promoted, EARLY), Some(true));
    assert_eq!(queue.push(&requested, REQUESTED), Some(true));
    assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
    assert_eq!(queue.push(&promoted, REQUESTED), None);

    assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &requested));
    assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &promoted));
    assert_eq!(queue.push(&promoted, REQUESTED), None);
    assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &earlier));
    assert!(queue.pop(false, 1).is_none());
}

#[test]
fn pipelines_known_to_take_long_leave_workers_for_the_others() {
    let new = || Arc::new(Pending::new(Vec::new(), None));
    let (long, longer, asked, quick) = (new(), new(), new(), new());
    let mut queue = Queue::new();
    for p in [&long, &longer, &asked] {
        assert_eq!(queue.push_as(p, EARLY, true), Some(true));
    }
    assert_eq!(queue.push_as(&quick, EARLY, false), Some(true));
    // The quick one first, then one long one at a time; one a draw asks
    // for waits for nobody.
    assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &quick));
    assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &long));
    assert!(queue.pop(false, 1).is_none());
    assert_eq!(queue.push_as(&asked, REQUESTED, true), Some(false));
    assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &asked));
    assert!(!asked.was_slow.load(Ordering::Relaxed));
    assert!(queue.pop(false, 1).is_none());
    queue.slow_now -= 1;
    assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &longer));
    assert!(queue.pop(false, 1).is_none());
}

/// Passes over the screen go before all others, and workers of their own
/// take nothing else.
#[test]
fn passes_over_the_screen_go_first() {
    let of = |screen| {
        let mut p = Pending::new(Vec::new(), None);
        p.screen = screen;
        Arc::new(p)
    };
    let (world, screen) = (of(false), of(true));
    let mut queue = Queue::new();
    assert_eq!(queue.push(&world, REQUESTED), Some(true));
    assert_eq!(queue.push(&screen, REQUESTED), Some(true));
    assert!(Arc::ptr_eq(&queue.pop(true, 1).unwrap(), &screen));
    assert!(queue.pop(true, 1).is_none());
    assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &world));
}

#[cfg(feature = "diagnostics")]
#[test]
fn missing_reports_left_out_reason_instead_of_being_made() {
    let mut p = Pending::new(Vec::new(), None);
    p.left_out = Some("intentionally skipped in test");
    assert_eq!(missing(&p).as_deref(), Some("intentionally skipped in test"));
}
