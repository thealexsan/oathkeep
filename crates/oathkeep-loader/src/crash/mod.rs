//! Names the faulting code when title code crashes.
//!
//! A vectored handler sees every exception first, so it only prints the
//! kinds that end a process (access violations and the like, and
//! breakpoints: titles assert with `int3`, fatal without a debugger) as
//! first-chance, since a handler may still take them (ntdll's loader does
//! with its own probes). What nobody handles, of any kind (an uncaught C++
//! exception too), is reported from the unhandled-exception filter
//! (`report_unhandled`, which the runtime calls ahead of the title's own)
//! and appended to the crash log, with the title's last debug output, which
//! often states the reason. Panics of ours go to the crash log too.

use windows_sys::Win32::Foundation::{
    EXCEPTION_ACCESS_VIOLATION, EXCEPTION_BREAKPOINT, EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_PRIV_INSTRUCTION, EXCEPTION_STACK_OVERFLOW,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
};


mod amd_only;
mod report;
mod unwind;
#[cfg(test)]
mod tests;

pub(crate) use amd_only::{CMP_FLAGS, cmp_flags};
pub use report::report_unhandled;
pub use unwind::locate;

pub(super) const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
/// Frames listed per report.
pub(crate) const MAX_FRAMES: usize = 40;

/// Memory saved to files when a crash is reported: (address, length, file).
#[cfg(feature = "diagnostics")]
static SAVE: std::sync::OnceLock<Vec<(usize, usize, std::path::PathBuf)>> =
    std::sync::OnceLock::new();

/// Debugging aid: on a reported crash, also write each `[address, address
/// + len)` (readable title memory, e.g. an engine table) to its file.
#[cfg(feature = "diagnostics")]
pub fn save_on_crash(ranges: Vec<(usize, usize, std::path::PathBuf)>) {
    let _ = SAVE.set(ranges);
}

/// The file crash reports are appended to, besides being printed.
static LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Also appends every unhandled-exception report and panic to `file`, so a
/// crash can be read after the console is gone.
pub fn log_crashes_to(file: std::path::PathBuf) {
    if LOG.set(file).is_ok() {
        let print = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            print(info);
            append_to_log(&format!(
                "Oathkeep: panic on thread {:?}: {info}\n{}",
                std::thread::current().name().unwrap_or("?"),
                title_output()
            ));
        }));
    }
}

/// Appends a failure that is not an exception (the host GPU's) to the crash
/// log, with the title's last debug output, and prints it.
pub fn log_crash(text: &str) {
    let text = format!("{text}
{}", title_output());
    eprint!("{text}");
    append_to_log(&text);
}

pub(super) fn append_to_log(text: &str) {
    use std::io::Write;
    let Some(file) = LOG.get() else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let opened = std::fs::OpenOptions::new().create(true).append(true).open(file);
    if let Ok(mut f) = opened {
        let _ = write!(f, "unix time {now}, process {}\n{text}\n", std::process::id());
    }
}

/// The title's last lines of debug output, kept for crash reports.
static SAID: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const SAID_LINES: usize = 8;

/// Keeps a line of the title's debug output for the next crash report.
pub fn note_title_output(line: &str) {
    if line.is_empty() {
        return;
    }
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.len() == SAID_LINES {
        said.pop_front();
    }
    said.push_back(line.to_owned());
}

/// The kept lines, one per report line; nothing if another thread holds
/// them (it may be the one that faulted).
pub(super) fn title_output() -> String {
    let Ok(said) = SAID.try_lock() else {
        return String::new();
    };
    said.iter().map(|l| format!("  title said: {l}\n")).collect()
}

/// Installs the handlers; call once before running title code.
pub fn report_crashes() {
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        unsafe { report_unhandled(info) };
        EXCEPTION_CONTINUE_SEARCH
    }
    // SAFETY: registers handlers that only read the exception they are
    // given; the title's own filter, once it sets one, takes the place of
    // ours and calls `report_unhandled` first.
    unsafe {
        AddVectoredExceptionHandler(0, Some(amd_only::handler));
        SetUnhandledExceptionFilter(Some(filter));
    }
}

/// What the vectored handler reports as it happens.
pub(super) const FATAL: [i32; 6] = [
    EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO,
    EXCEPTION_STACK_OVERFLOW,
    EXCEPTION_BREAKPOINT,
];
