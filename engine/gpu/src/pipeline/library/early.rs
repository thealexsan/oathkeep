use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use super::DIR;

pub(super) const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 100_000;

/// How long each title pipeline that took long to make took, in ms (beside
/// the early list; a key may be there more than once).
pub(super) const SLOW_FILE: &str = "slow-pipelines.txt";
/// A pipeline that takes this long is one ([`slow`]).
const SLOW_MS: u64 = 1000;
static SLOW: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

pub(super) fn slow_ones() -> MutexGuard<'static, Option<HashMap<String, u64>>> {
    let mut slow = SLOW.lock().unwrap_or_else(|e| e.into_inner());
    if slow.is_none() {
        let listed = DIR.get().and_then(|d| std::fs::read_to_string(d.join(SLOW_FILE)).ok()).unwrap_or_default();
        *slow = Some(
            listed
                .lines()
                .filter_map(|l| l.split_once(' '))
                .filter_map(|(key, ms)| Some((key.to_owned(), ms.parse().ok()?)))
                .collect(),
        );
    }
    slow
}

/// Whether making the title pipeline with `key` took [`SLOW_MS`] in this or
/// an earlier run.
pub(crate) fn slow(key: &str) -> bool {
    slow_ones().as_ref().is_some_and(|s| s.contains_key(key))
}

/// The title pipeline with `key` took `ms` to make.
pub(crate) fn note_time(key: &str, ms: u64) {
    if ms < SLOW_MS {
        return;
    }
    let mut slow = slow_ones();
    let known = slow.get_or_insert_default().entry(key.to_owned()).or_insert(0);
    if ms > *known {
        *known = ms;
        // With the next save of the early list.
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

/// Title pipeline keys requested by draws or dispatches (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
pub(super) static USED_CHANGED: AtomicBool = AtomicBool::new(false);

pub(super) fn used() -> MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was requested in this or an earlier run.
///
/// `OATHKEEP_EARLY_ALL=1`: every title pipeline is started at creation,
/// requested before or not, so that the driver's own cache holds it for
/// later runs.
pub(crate) fn wanted_early(key: &str) -> bool {
    static ALL: OnceLock<bool> = OnceLock::new();
    if *ALL.get_or_init(|| std::env::var_os("OATHKEEP_EARLY_ALL").is_some_and(|v| v == "1")) {
        return true;
    }
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// A draw or dispatch requested the title pipeline with `key`.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() < EARLY_MAX && set.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}
