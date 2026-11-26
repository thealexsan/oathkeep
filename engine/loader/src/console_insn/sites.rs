use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use super::{AtomicU64, Relaxed};
use std::sync::atomic::AtomicUsize;

/// Address, what is there (an index into [`NAMES`]), exceptions since the last line.
static SITES: [(AtomicUsize, AtomicUsize, AtomicU64); 512] =
    [const { (AtomicUsize::new(0), AtomicUsize::new(0), AtomicU64::new(0)) }; 512];
const NAMES: [&str; 8] = ["other", "clwb", "monitorx", "mwaitx", "clzero", "clzero loop", "rdpru", "extrq/insertq"];
static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
static PATCHED: AtomicU64 = AtomicU64::new(0);

pub(super) fn patched() {
    PATCHED.fetch_add(1, Relaxed);
}

pub(super) fn count(at: usize, what: super::ConsoleOnly) {
    use super::ConsoleOnly as C;
    let kind = match what {
        C::Clwb => 1,
        C::Monitorx => 2,
        C::Mwaitx => 3,
        C::Clzero => 4,
        C::ClzeroLoop { .. } => 5,
        C::Rdpru => 6,
        C::Extrq { .. } | C::Insertq { .. } => 7,
        _ => 0,
    };
    let mut i = (at ^ at >> 9) % SITES.len();
    for _ in 0..8 {
        let s = &SITES[i];
        let there = s.0.compare_exchange(0, at, Relaxed, Relaxed).unwrap_or_else(|there| there);
        if there == 0 || there == at {
            s.1.store(kind, Relaxed);
            s.2.fetch_add(1, Relaxed);
            break;
        }
        i = (i + 1) % SITES.len();
    }
    // Now and then: is a line due? Never waited for.
    if let Ok(mut last) = LAST.try_lock() {
        let since = *last.get_or_insert_with(std::time::Instant::now);
        if since.elapsed().as_secs() >= 5 {
            let s = since.elapsed().as_secs_f64();
            *last = Some(std::time::Instant::now());
            drop(last);
            line(s);
        }
    }
}

fn line(seconds: f64) {
    let mut all: Vec<(u64, usize, usize)> =
        SITES.iter().map(|s| (s.2.swap(0, Relaxed), s.0.load(Relaxed), s.1.load(Relaxed))).filter(|s| s.0 > 0).collect();
    all.sort_unstable_by_key(|s| std::cmp::Reverse(s.0));
    let mut kinds = [0u64; NAMES.len()];
    for s in &all {
        kinds[s.2] += s.0;
    }
    let kinds: Vec<String> = NAMES
        .iter()
        .zip(kinds)
        .filter(|(_, n)| *n > 0)
        .map(|(name, n)| format!("{name} {:.0}", n as f64 / seconds))
        .collect();
    let top: Vec<String> =
        all.iter().take(8).map(|s| format!("{:#x} {} {:.0}", s.1, NAMES[s.2], s.0 as f64 / seconds)).collect();
    tracing::info!(
        "console CPU instructions that fault, a second: {}; places replaced so far: {}; the busiest places: {}",
        kinds.join(", "),
        PATCHED.load(Relaxed),
        top.join(", ")
    );
}
