use super::*;

/// The number of the newest submission begun, and of the newest of which all
/// before it have finished too.
/// Host objects the title let go of while a submission may still use them
/// (a texture its descriptors name, a descriptor slot's old resource), by
/// the submission count they wait for. The title's descriptor writes and
/// releases reach the host at once, from its own threads, while the host
/// runs a submission: freeing there is a GPU page fault on the freed
/// texture (a device hang).
pub(super) static RETIRED: Mutex<std::collections::VecDeque<(u64, Retired)>> = Mutex::new(std::collections::VecDeque::new());
/// Submissions started and finished (waited for).
pub(super) static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(super) static FINISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A retired object, dropped on whatever thread finishes the submission.
#[allow(dead_code)]
pub(super) enum Retired {
    Any(Box<dyn std::any::Any>),
    /// What a descriptor slot showed: 20,000 a frame, without a box each.
    Resource(ID3D12Resource),
}
// SAFETY: what is retired is D3D12 objects (free-threaded) and the
// reference-counted wrappers holding them, dropped once.
unsafe impl Send for Retired {}

pub(crate) fn submissions() -> (u64, u64) {
    (STARTED.load(Ordering::Acquire), FINISHED.load(Ordering::Acquire))
}

/// Drops `object` once every submission running now has finished: at once
/// if none is.
pub(crate) fn retire<T: 'static>(object: T) {
    // (Those the title made and the host has not begun get the next numbers.)
    let queued = crate::queue::queued();
    let started = STARTED.load(Ordering::Acquire) + queued;
    if bury_lag() == 0 && started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
        drop(object);
        return;
    }
    RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back((started, Retired::Any(Box::new(object))));
}

/// [`retire`] for a resource.
pub(crate) fn retire_resource(object: ID3D12Resource) {
    let queued = crate::queue::queued();
    let started = STARTED.load(Ordering::Acquire) + queued;
    if bury_lag() == 0 && started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
        drop(object);
        return;
    }
    RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
    RETIRED.lock().unwrap_or_else(|e| e.into_inner()).push_back((started, Retired::Resource(object)));
}

/// How many submissions later than it could (the last one that may use it
/// has finished) an object let go is dropped, and a host list that has run
/// is reset, about two frames: the device losses of the night of 2026-11-25
/// were page faults in memory nothing of ours named any more, at no command
/// in particular. Objects let go while nothing runs wait as well: lists
/// recorded and not yet sent may name them. `OATHKEEP_BURY_LAG=0`: not.
pub(super) fn bury_lag() -> u64 {
    static LAG: OnceLock<u64> = OnceLock::new();
    *LAG.get_or_init(|| {
        let default = if crate::residency::limited_host_ram() { 16 } else { 64 };
        std::env::var("OATHKEEP_BURY_LAG").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    })
}

/// Background recycler thread for retired D3D12 resources so WDDM `Release()`
/// calls do not stall the relay landing thread (~25 ms/frame in `stat::REST`).
/// Bounded capacity ensures backpressure drops inline if the recycler ever
/// falls behind.
pub(super) fn bury_recycler() -> &'static std::sync::mpsc::SyncSender<Vec<(u64, Retired)>> {
    static TX: OnceLock<std::sync::mpsc::SyncSender<Vec<(u64, Retired)>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<(u64, Retired)>>(64);
        let _ = std::thread::Builder::new().name("oathkeep-bury".into()).spawn(move || {
            for batch in rx {
                drop(batch);
            }
        });
        tx
    })
}

/// Drops what waited for submissions up to `finished` ([`bury_lag`]). The
/// numbers come in order, but for threads that overtake each other: one of
/// those is dropped a little later than it could be.
pub(super) fn bury(finished: u64) {
    let finished = finished.saturating_sub(bury_lag());
    let done: Vec<(u64, Retired)> = {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        let n = retired.iter().take_while(|(at, _)| *at <= finished).count();
        if n == 0 {
            return;
        }
        retired.drain(..n).collect()
    };
    let _ = bury_recycler().try_send(done);
}

/// Objects [`retire`] dropped at once (no submission was running) and
/// kept until a running submission finished, since the start.
pub(super) static RETIREMENTS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];
