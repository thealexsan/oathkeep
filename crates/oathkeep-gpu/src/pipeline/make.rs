use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Mutex<Option<Box<GraphicsState>>>,
    is_compute: bool,
    queue_state: AtomicU8,
    last_frame: AtomicU64,
    made: OnceLock<Option<HostPipeline>>,
    key: String,
    used: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(
            graphics
                .as_ref()
                .map(|g| g.describe())
                .unwrap_or_default()
                .as_bytes(),
        );
        let is_compute = graphics.is_none() && shaders.iter().any(|s| s.stage == "compute");
        Pending {
            shaders,
            graphics: Mutex::new(graphics),
            is_compute,
            queue_state: AtomicU8::new(NOT_QUEUED),
            last_frame: AtomicU64::new(0),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    fn make(&self) {
        self.made.get_or_init(|| {
            let graphics = self
                .graphics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            HostPipeline::new(&self.shaders, graphics.as_deref())
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("OATHKEEP_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(16);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

static WAITING: AtomicU64 = AtomicU64::new(0);

pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        let frame = crate::frame::current();
        p.last_frame.fetch_max(frame, Ordering::Relaxed);
        start(p, REQUESTED);
        wait(p, frame);
    }
    p.made.get()?.as_ref()
}

pub(crate) fn now(p: &Arc<Pending>) -> bool {
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

fn wait(p: &Pending, frame: u64) {
    let budget = wait_budget();
    if budget.is_zero() {
        return;
    }
    static SPENT: Mutex<(u64, Duration)> = Mutex::new((u64::MAX, Duration::ZERO));
    let shared = shared_budget();
    let left = {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done
            .wait_timeout(held, rest)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

struct Queue {
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: VecDeque::new(),
            requested: VecDeque::new(),
        }
    }

    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self, allow_early: bool) -> Option<(Arc<Pending>, bool)> {
        if !self.requested.is_empty() {
            let mut best_idx = self.requested.len() - 1;
            let mut best_key = (
                self.requested[best_idx].last_frame.load(Ordering::Relaxed),
                self.requested[best_idx].is_compute,
            );
            for i in (0..best_idx).rev() {
                let key = (
                    self.requested[i].last_frame.load(Ordering::Relaxed),
                    self.requested[i].is_compute,
                );
                if key > best_key {
                    best_key = key;
                    best_idx = i;
                }
            }
            let p = self.requested.remove(best_idx)?;
            p.queue_state.store(MAKING, Ordering::Relaxed);
            return Some((p, false));
        }
        if !allow_early {
            return None;
        }
        let p = std::iter::from_fn(|| self.early.pop_front())
            .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some((p, true))
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(
                WAITING.fetch_add(1, Ordering::Relaxed) + 1,
                Ordering::Relaxed,
            );
        }
        AVAILABLE.notify_one();
    }
}

fn early_commit_floor() -> u64 {
    static FLOOR: OnceLock<u64> = OnceLock::new();
    *FLOOR.get_or_init(|| {
        std::env::var("OATHKEEP_EARLY_PIPELINE_COMMIT_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(3072)
            << 20
    })
}

fn early_allowed() -> bool {
    let floor = early_commit_floor();
    if floor == 0 {
        return true;
    }
    crate::host::available_commit().is_none_or(|avail| avail >= floor)
}

fn early_limit() -> usize {
    let max = worker_count().min(4);
    let Some(avail) = crate::host::available_commit() else {
        return max;
    };
    if avail < (3072u64 << 20) {
        1
    } else if avail < (4096u64 << 20) {
        max.min(2)
    } else {
        max
    }
}

fn next() -> (Arc<Pending>, bool) {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let allow_early = early_allowed();
        if let Some(item) = queue.pop(allow_early) {
            return item;
        }
        if !allow_early && !queue.early.is_empty() {
            let (guard, _) = AVAILABLE
                .wait_timeout(queue, Duration::from_millis(250))
                .unwrap_or_else(|e| e.into_inner());
            queue = guard;
        } else {
            queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }
}

fn worker_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        std::env::var("OATHKEEP_PIPELINE_WORKERS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map(|n| n.clamp(1, 8))
            .unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8))
            })
    })
}

fn compile_limit() -> usize {
    let max = worker_count();
    let Some(avail) = crate::host::available_commit() else {
        return max;
    };
    if avail < (2560u64 << 20) {
        1
    } else if avail < (4096u64 << 20) {
        max.min(2)
    } else {
        max
    }
}

static COMPILE_GATE: (Mutex<(usize, usize)>, Condvar) = (Mutex::new((0, 0)), Condvar::new());

struct ActiveCompile {
    early: bool,
}

impl ActiveCompile {
    fn enter(early: bool) -> Self {
        let (lock, cv) = &COMPILE_GATE;
        let mut active = lock.lock().unwrap_or_else(|e| e.into_inner());
        while active.0 >= compile_limit() || (early && active.1 >= early_limit()) {
            let (guard, _) = cv
                .wait_timeout(active, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner());
            active = guard;
        }
        active.0 += 1;
        if early {
            active.1 += 1;
        }
        drop(active);
        if crate::host::available_commit().is_some_and(|c| c < (1536u64 << 20)) {
            static LAST_RECLAIM: Mutex<Option<Instant>> = Mutex::new(None);
            let should = {
                let mut last = LAST_RECLAIM.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                if last.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(5)) {
                    *last = Some(now);
                    true
                } else {
                    false
                }
            };
            if should {
                crate::host::reclaim_for_oom();
            }
        }
        ActiveCompile { early }
    }
}

impl Drop for ActiveCompile {
    fn drop(&mut self) {
        let (lock, cv) = &COMPILE_GATE;
        let mut active = lock.lock().unwrap_or_else(|e| e.into_inner());
        active.0 = active.0.saturating_sub(1);
        if self.early {
            active.1 = active.1.saturating_sub(1);
        }
        cv.notify_all();
    }
}

fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count();
        for i in 0..workers {
            let spawned = std::thread::Builder::new()
                .name(format!("oathkeep-pipelines-{i}"))
                .spawn(|| {
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        let (pending, is_early) = next();
                        {
                            let _permit = ActiveCompile::enter(is_early);
                            pending.make();
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

static PEAK: AtomicU64 = AtomicU64::new(0);

fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert!(Arc::ptr_eq(&queue.pop(true).unwrap().0, &promoted));
        assert!(Arc::ptr_eq(&queue.pop(true).unwrap().0, &requested));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(queue.pop(false).is_none());
        assert!(Arc::ptr_eq(&queue.pop(true).unwrap().0, &earlier));
        assert!(queue.pop(true).is_none());
    }

    #[test]
    fn active_frame_and_compute_pipelines_beat_stale_graphics() {
        let stale_gfx = Arc::new(Pending::new(Vec::new(), None));
        stale_gfx.last_frame.store(10, Ordering::Relaxed);
        let active_gfx = Arc::new(Pending::new(Vec::new(), None));
        active_gfx.last_frame.store(20, Ordering::Relaxed);
        let mut active_cs = Pending::new(Vec::new(), None);
        active_cs.is_compute = true;
        active_cs.last_frame.store(20, Ordering::Relaxed);
        let active_cs = Arc::new(active_cs);

        let mut queue = Queue::new();
        assert_eq!(queue.push(&active_cs, REQUESTED), Some(true));
        assert_eq!(queue.push(&stale_gfx, REQUESTED), Some(true));
        assert_eq!(queue.push(&active_gfx, REQUESTED), Some(true));

        assert!(Arc::ptr_eq(&queue.pop(true).unwrap().0, &active_cs));
        assert!(Arc::ptr_eq(&queue.pop(true).unwrap().0, &active_gfx));
        assert!(Arc::ptr_eq(&queue.pop(true).unwrap().0, &stale_gfx));
        assert!(queue.pop(true).is_none());
    }
}
