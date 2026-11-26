//! The title's life cycle. A console title is never told it is closing:
//! quitting it or launching a new game first constrains it (its
//! constrained-change routines run), then suspends it (its suspend routines
//! run, and it has a second to save) and then ends the process. Closing the
//! title's window does the same here; a new launch then starts `oathkeep`
//! again, which waits for this process to be gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

mod window;

#[cfg(test)]
mod tests;

pub use window::{CreateWindowExA, CreateWindowExW};

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

use crate::{HleModule, hle_export};

/// A suspend routine (`quiesced`) or a constrained-change routine
/// (`constrained`): both take a flag and the title's context.
type Routine = extern "system" fn(flag: u8, context: *mut c_void);

/// Registration id, routine and context.
type Registration = (usize, Routine, usize);

/// Each suspend routine.
static ROUTINES: Mutex<Vec<Registration>> = Mutex::new(Vec::new());

/// Each constrained-change routine.
static CONSTRAINED: Mutex<Vec<Registration>> = Mutex::new(Vec::new());

fn routines() -> MutexGuard<'static, Vec<Registration>> {
    ROUTINES.lock().unwrap_or_else(|e| e.into_inner())
}

fn constrained() -> MutexGuard<'static, Vec<Registration>> {
    CONSTRAINED.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long the console lets suspend routines run before it ends the title.
const SUSPEND_DEADLINE: Duration = Duration::from_secs(1);

const ERROR_INVALID_PARAMETER: u32 = 87;

/// Adds `routine` to `list` and writes its registration id.
fn register(
    mut list: MutexGuard<'static, Vec<Registration>>,
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    let Some(routine) = routine else {
        return ERROR_INVALID_PARAMETER;
    };
    if registration.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    list.push((id, routine, context as usize));
    // SAFETY: checked non-null; the title passes room for its registration.
    unsafe { registration.write(id) };
    0
}

pub extern "system" fn RegisterAppStateChangeNotification(
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    register(routines(), routine, context, registration)
}

pub extern "system" fn UnregisterAppStateChangeNotification(registration: usize) {
    routines().retain(|&(id, ..)| id != registration);
}

/// The host's own version never calls the routine (a desktop app is never
/// constrained), so a title waiting for it would never suspend.
pub extern "system" fn RegisterAppConstrainedChangeNotification(
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    register(constrained(), routine, context, registration)
}

pub extern "system" fn UnregisterAppConstrainedChangeNotification(registration: usize) {
    constrained().retain(|&(id, ..)| id != registration);
}

/// Suspends the title as the console does when it ends it, runs `then`
/// (once), then ends the process. Returns at once; later calls do nothing.
pub(crate) fn end_title(why: &'static str, then: Box<dyn FnOnce() + Send>) {
    static ENDING: AtomicBool = AtomicBool::new(false);
    if ENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let constrain = constrained().clone();
    let suspend = routines().clone();
    tracing::info!(
        constrained = constrain.len(),
        routines = suspend.len(),
        "{why}: suspending the title"
    );
    // Whoever comes first (the routines or the deadline) runs `then`; the
    // other waits for it, so the process never ends halfway through.
    let then = std::sync::Arc::new(Mutex::new(Some(then)));
    let finish = move |how: &str| {
        let mut then = then.lock().unwrap_or_else(|e| e.into_inner());
        tracing::info!("{how}; ending the title");
        if let Some(then) = then.take() {
            then();
        }
        end();
    };
    let late = finish.clone();
    // Not on the calling thread: titles hand the suspend to their window
    // thread and wait for it inside the routine.
    std::thread::spawn(move || {
        constrain_then_suspend(&constrain, &suspend);
        finish("title suspended");
    });
    std::thread::spawn(move || {
        std::thread::sleep(SUSPEND_DEADLINE);
        late("title did not finish suspending in time");
    });
}

/// Runs the constrained-change routines (constrained), then the suspend
/// routines (quiesced), as the console does when it suspends a title.
/// Some titles' suspend routines wait for a state that only their
/// constrained routine brings about; called alone it never returns.
fn constrain_then_suspend(constrain: &[Registration], suspend: &[Registration]) {
    for &(_, routine, context) in constrain.iter().chain(suspend) {
        routine(1, context as *mut c_void);
    }
}

/// Set in the environment of a relaunched `oathkeep`: an inherited handle
/// of the process it replaces.
const RELAUNCHED_FROM: &str = "OATHKEEP_RELAUNCHED_FROM";

/// Starts `oathkeep` again with `args` (after the title has suspended); the
/// new process waits until this one has ended.
pub(crate) fn relaunch(args: Vec<std::ffi::OsString>) {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    let mut me: HANDLE = std::ptr::null_mut();
    // SAFETY: an inheritable duplicate of our own process handle.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut me,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    let spawned = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        command.args(&args);
        if duplicated {
            command.env(RELAUNCHED_FROM, (me as usize).to_string());
        }
        command.spawn()
    });
    match spawned {
        Ok(child) => tracing::info!(pid = child.id(), ?args, "relaunched"),
        Err(e) => tracing::warn!(%e, "relaunch failed; the title just ends"),
    }
}

/// In a relaunched `oathkeep`: waits (30 s at most) for the process it
/// replaces to be gone, so the two never share the title's files.
pub fn wait_for_previous() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let Some(handle) = std::env::var(RELAUNCHED_FROM)
        .ok()
        .and_then(|h| h.parse::<usize>().ok())
    else {
        return;
    };
    // SAFETY: removing our own variable before any other thread reads it.
    unsafe { std::env::remove_var(RELAUNCHED_FROM) };
    let handle = handle as HANDLE;
    // SAFETY: the inherited process handle the previous process gave us.
    unsafe {
        WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
    }
}

fn end() {
    // Compiled host pipelines kept for the next run.
    #[cfg(windows)]
    oathkeep_gpu::pipeline::save_pipeline_cache();
    // SAFETY: ends this process, as the console ends a suspended title.
    unsafe { TerminateProcess(GetCurrentProcess(), 0) };
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "lifecycle",
        dlls: &[
            "api-ms-win-core-psm-appnotify-*",
            "ext-ms-win-rtcore-ntuser-window-*",
        ],
        exports: vec![
            hle_export!(RegisterAppStateChangeNotification),
            hle_export!(UnregisterAppStateChangeNotification),
            hle_export!(RegisterAppConstrainedChangeNotification),
            hle_export!(UnregisterAppConstrainedChangeNotification),
            hle_export!(CreateWindowExW),
            hle_export!(CreateWindowExA),
        ],
    }
}
