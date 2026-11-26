//! GPU diagnostics, frame capture, timelines, memory ledgers, and toggles.

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) mod ab_toggle;
#[cfg(windows)]
pub(crate) mod ambient;
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) mod capture;
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) mod copy_stats;
#[cfg(windows)]
pub(crate) mod cores;
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) mod fault_log;
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) mod memory_ledger;
#[cfg(windows)]
pub(crate) mod picked_writes;
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) mod pop_watch;
pub(crate) mod switch;
#[cfg(windows)]
pub(crate) mod tdr;
pub mod timeline;
