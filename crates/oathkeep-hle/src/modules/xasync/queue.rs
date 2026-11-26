use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use super::super::xgameruntime::{HRESULT, S_OK};
use super::{E_ABORT, E_INVALIDARG};

pub(super) const WORK: u32 = 0;
pub(super) const COMPLETION: u32 = 1;
pub(super) const INFINITE: u32 = u32::MAX;

pub(super) type Callback = extern "system" fn(*mut c_void, bool);
pub(super) type MonitorCallback = extern "system" fn(*mut c_void, *const Queue, u32);
pub(super) type TerminatedCallback = extern "system" fn(*mut c_void);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Manual,
    ThreadPool,
    Serialized,
    Immediate,
}

impl Mode {
    fn from_raw(mode: u32) -> Option<Mode> {
        [
            Mode::Manual,
            Mode::ThreadPool,
            Mode::Serialized,
            Mode::Immediate,
        ]
        .get(mode as usize)
        .copied()
    }
}

/// A title callback and its context (addresses, so tasks can cross threads).
#[derive(Clone, Copy)]
pub(super) struct Task {
    context: usize,
    callback: usize,
}

impl Task {
    pub(super) fn new(context: *mut c_void, callback: Callback) -> Task {
        Task {
            context: context as usize,
            callback: callback as usize,
        }
    }

    pub(super) fn run(self, canceled: bool) {
        // SAFETY: built from a `Callback` in `new`.
        let callback: Callback = unsafe { std::mem::transmute(self.callback) };
        callback(self.context as *mut c_void, canceled);
    }
}

struct Port {
    mode: Mode,
    tasks: Mutex<VecDeque<Task>>,
    ready: Condvar,
}

impl Port {
    fn new(mode: Mode) -> Arc<Port> {
        let port = Arc::new(Port {
            mode,
            tasks: Mutex::default(),
            ready: Condvar::new(),
        });
        if mode == Mode::Serialized {
            // The worker ends once it holds the last reference (every queue
            // using the port is closed) and nothing is left to run.
            let worker = port.clone();
            std::thread::spawn(move || {
                loop {
                    match worker.take(1000, || false) {
                        Some(task) => task.run(false),
                        None if Arc::strong_count(&worker) == 1 => break,
                        None => {}
                    }
                }
            });
        }
        port
    }

    fn submit(&self, task: Task) {
        match self.mode {
            Mode::Immediate => task.run(false),
            Mode::ThreadPool => POOL.submit(task),
            Mode::Manual | Mode::Serialized => {
                lock(&self.tasks).push_back(task);
                self.ready.notify_one();
            }
        }
    }

    /// The next queued task, waiting up to `timeout_ms`, or until `ended`
    /// (checked whenever the port is woken) says no task will come.
    fn take(&self, timeout_ms: u32, ended: impl Fn() -> bool) -> Option<Task> {
        let waiting = |t: &mut VecDeque<Task>| t.is_empty() && !ended();
        let tasks = lock(&self.tasks);
        let mut tasks = if timeout_ms == INFINITE {
            self.ready
                .wait_while(tasks, waiting)
                .unwrap_or_else(|e| e.into_inner())
        } else {
            let timeout = Duration::from_millis(timeout_ms.into());
            self.ready
                .wait_timeout_while(tasks, timeout, waiting)
                .unwrap_or_else(|e| e.into_inner())
                .0
        };
        tasks.pop_front()
    }
}

/// The thread pool behind thread-pool ports: idle workers are reused, and a
/// new one starts whenever more tasks wait than workers idle, so a callback
/// that blocks on another never starves it. Workers stay for reuse.
struct Pool {
    /// Queued tasks and the number of idle workers.
    state: Mutex<(VecDeque<Task>, usize)>,
    ready: Condvar,
}

static POOL: Pool = Pool {
    state: Mutex::new((VecDeque::new(), 0)),
    ready: Condvar::new(),
};

impl Pool {
    fn submit(&'static self, task: Task) {
        let mut state = lock(&self.state);
        state.0.push_back(task);
        if state.0.len() > state.1 {
            std::thread::spawn(move || self.work());
        } else {
            self.ready.notify_one();
        }
    }

    fn work(&self) {
        let mut state = lock(&self.state);
        loop {
            while state.0.is_empty() {
                state.1 += 1;
                state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                state.1 -= 1;
            }
            let task = state.0.pop_front();
            drop(state);
            if let Some(task) = task {
                task.run(false);
            }
            state = lock(&self.state);
        }
    }
}

/// A queue handle is `Arc::into_raw` of this; duplicating and closing a
/// handle add and drop a reference.
pub struct Queue {
    ports: [Arc<Port>; 2],
    monitors: Mutex<Vec<(u64, usize, usize)>>,
    terminated: AtomicBool,
    /// Delayed callbacks still waiting out their delay, in submission
    /// order: terminating the queue cancels them with the queued ones.
    delayed: Mutex<BTreeMap<u64, Task>>,
}

impl Queue {
    fn new(ports: [Arc<Port>; 2]) -> Arc<Queue> {
        Arc::new(Queue {
            ports,
            monitors: Mutex::default(),
            terminated: AtomicBool::new(false),
            delayed: Mutex::default(),
        })
    }

    pub(super) fn submit(self: &Arc<Self>, port: u32, delay_ms: u32, task: Task) -> HRESULT {
        static NEXT_DELAYED: AtomicU64 = AtomicU64::new(0);
        if port > COMPLETION {
            return E_INVALIDARG;
        }
        if self.terminated.load(Ordering::Acquire) {
            return E_ABORT;
        }
        if delay_ms == 0 {
            self.deliver(port, task);
            return S_OK;
        }
        let id = NEXT_DELAYED.fetch_add(1, Ordering::Relaxed);
        {
            // Checked again under the lock termination drains: the callback
            // is either canceled with the others or refused.
            let mut delayed = lock(&self.delayed);
            if self.terminated.load(Ordering::Acquire) {
                return E_ABORT;
            }
            delayed.insert(id, task);
        }
        let queue = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms.into()));
            // Gone if the queue was terminated meanwhile, which canceled it.
            let task = lock(&queue.delayed).remove(&id);
            if let Some(task) = task {
                queue.deliver(port, task);
            }
        });
        S_OK
    }

    fn deliver(self: &Arc<Self>, port: u32, task: Task) {
        self.ports[port as usize].submit(task);
        // Copied out: a monitor may unregister itself.
        let monitors = lock(&self.monitors).clone();
        for (_, context, callback) in monitors {
            // SAFETY: stored from a `MonitorCallback` in `register_monitor`.
            let callback: MonitorCallback = unsafe { std::mem::transmute(callback) };
            callback(context as *mut c_void, Arc::as_ptr(self), port);
        }
    }
}

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The process queue: `None` until first used (then a thread-pool queue),
/// `Some(None)` once the title clears it.
static PROCESS_QUEUE: Mutex<Option<Option<Arc<Queue>>>> = Mutex::new(None);

fn process_queue() -> Option<Arc<Queue>> {
    lock(&PROCESS_QUEUE)
        .get_or_insert_with(|| {
            Some(Queue::new([
                Port::new(Mode::ThreadPool),
                Port::new(Mode::ThreadPool),
            ]))
        })
        .clone()
}

/// A new reference to the queue behind `handle` (null: the process queue).
pub(super) fn queue(handle: *const Queue) -> Option<Arc<Queue>> {
    if handle.is_null() {
        return process_queue();
    }
    // SAFETY: a handle we returned, which the title has not closed.
    unsafe {
        Arc::increment_strong_count(handle);
        Some(Arc::from_raw(handle))
    }
}

pub fn XTaskQueueCreate(work: u32, completion: u32, out: *mut *const Queue) -> HRESULT {
    let (Some(work), Some(completion)) = (Mode::from_raw(work), Mode::from_raw(completion)) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let queue = Queue::new([Port::new(work), Port::new(completion)]);
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

/// A port handle points at the port, which its queue keeps alive.
pub fn XTaskQueueGetPort(handle: *const Queue, port: u32, out: *mut *const c_void) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    if port > COMPLETION || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::as_ptr(&queue.ports[port as usize]).cast()) };
    S_OK
}

pub fn XTaskQueueCreateComposite(
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    if work.is_null() || completion.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let port = |p: *const c_void| {
        // SAFETY: a port handle from `XTaskQueueGetPort`, whose queue is open.
        unsafe {
            Arc::increment_strong_count(p.cast::<Port>());
            Arc::from_raw(p.cast::<Port>())
        }
    };
    let queue = Queue::new([port(work), port(completion)]);
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

pub fn XTaskQueueDuplicateHandle(handle: *const Queue, out: *mut *const Queue) -> HRESULT {
    match queue(handle) {
        Some(queue) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(Arc::into_raw(queue)) };
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub fn XTaskQueueCloseHandle(handle: *const Queue) {
    if !handle.is_null() {
        // SAFETY: a handle we returned; this drops its reference.
        unsafe { Arc::decrement_strong_count(handle) };
    }
}

/// Runs one queued callback of a manual port; false if none came in time,
/// or none can come: the queue was terminated (which ends a wait under way,
/// as a dispatcher thread's loop expects).
pub fn XTaskQueueDispatch(handle: *const Queue, port: u32, timeout_ms: u32) -> bool {
    let Some(queue) = queue(handle) else {
        return false;
    };
    let Some(port) = queue.ports.get(port as usize) else {
        return false;
    };
    if port.mode != Mode::Manual {
        return false;
    }
    match port.take(timeout_ms, || queue.terminated.load(Ordering::Acquire)) {
        Some(task) => {
            task.run(false);
            true
        }
        None => false,
    }
}

pub fn XTaskQueueSubmitDelayedCallback(
    handle: *const Queue,
    port: u32,
    delay_ms: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    match (queue(handle), callback) {
        (Some(queue), Some(callback)) => queue.submit(port, delay_ms, Task::new(context, callback)),
        _ => E_INVALIDARG,
    }
}

/// Cancels what is still queued or waiting out a delay (callbacks see
/// `canceled`), refuses new work, then calls `callback`.
pub fn XTaskQueueTerminate(
    handle: *const Queue,
    _wait: bool,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    queue.terminated.store(true, Ordering::Release);
    for port in &queue.ports {
        let pending: Vec<Task> = lock(&port.tasks).drain(..).collect();
        // Dispatches waiting on this queue end: nothing more will come.
        // (Those of other queues on the port see theirs is open and wait
        // on.) After the lock above, so none misses both flag and wake.
        port.ready.notify_all();
        for task in pending {
            task.run(true);
        }
    }
    let delayed = std::mem::take(&mut *lock(&queue.delayed));
    for task in delayed.into_values() {
        task.run(true);
    }
    if let Some(callback) = callback {
        callback(context);
    }
    S_OK
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub fn XTaskQueueRegisterMonitor(
    handle: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    let (Some(queue), Some(callback)) = (queue(handle), callback) else {
        return E_INVALIDARG;
    };
    if token.is_null() {
        return E_INVALIDARG;
    }
    let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    lock(&queue.monitors).push((id, context as usize, callback as usize));
    // SAFETY: checked non-null.
    unsafe { token.write(id) };
    S_OK
}

pub fn XTaskQueueUnregisterMonitor(handle: *const Queue, token: u64) {
    if let Some(queue) = queue(handle) {
        lock(&queue.monitors).retain(|m| m.0 != token);
    }
}

/// A new handle to the process queue, if there is one.
pub fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *const Queue) -> bool {
    let found = process_queue();
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(found.clone().map_or(std::ptr::null(), Arc::into_raw)) };
    }
    found.is_some()
}

pub fn XTaskQueueSetCurrentProcessTaskQueue(handle: *const Queue) {
    let new = if handle.is_null() {
        None
    } else {
        queue(handle)
    };
    *lock(&PROCESS_QUEUE) = Some(new);
}
