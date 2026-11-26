//! What was recorded, put in order: the Chrome trace, and the text summary
//! with the chain each present came out of.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::record::{DEPTH, Data, frequency};

mod chrome;
mod summary;
#[cfg(test)]
mod tests;
mod trace;

use chrome::chrome;
use summary::summary;
use trace::Trace;

/// The cause of what a thread did outside every span.
pub(super) const OUTSIDE: &str = "outside our spans";

pub(super) fn ms(ticks: u64) -> f64 {
    ticks as f64 * 1e3 / frequency() as f64
}

/// The process's threads at a moment.
#[derive(Default)]
pub(in crate::diagnostics::timeline) struct Threads {
    /// By number: its processor cycles so far, its name, where it started.
    pub(super) all: HashMap<u32, (u64, String, usize)>,
    /// The processor's clock and the performance counter then.
    pub(super) cycles: u64,
    pub(super) ticks: u64,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryThreadCycleTime(thread: *mut std::ffi::c_void, cycles: *mut u64) -> i32;
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationThread(
        thread: *mut std::ffi::c_void,
        class: i32,
        out: *mut std::ffi::c_void,
        len: u32,
        returned: *mut u32,
    ) -> i32;
}

impl Threads {
    pub(in crate::diagnostics::timeline) fn now() -> Threads {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcessId, GetThreadDescription, OpenThread, THREAD_QUERY_INFORMATION,
        };
        /// `ThreadQuerySetWin32StartAddress`.
        const START: i32 = 9;
        let mut threads = Threads::default();
        // SAFETY: a snapshot of our own, walked with a sized entry; handles
        // opened here are closed; the name is the system's, freed as asked.
        unsafe {
            let own = GetCurrentProcessId();
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return threads;
            }
            let mut entry = THREADENTRY32 {
                dwSize: size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == own {
                    let thread = OpenThread(THREAD_QUERY_INFORMATION, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        let (mut cycles, mut start, mut name) = (0, 0usize, std::ptr::null_mut());
                        QueryThreadCycleTime(thread, &mut cycles);
                        NtQueryInformationThread(thread, START, (&raw mut start).cast(), 8, std::ptr::null_mut());
                        let mut text = String::new();
                        if GetThreadDescription(thread, &mut name) >= 0 && !name.is_null() {
                            let len = (0..).take_while(|&i| *name.add(i) != 0).count();
                            text = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
                            LocalFree(name.cast());
                        }
                        CloseHandle(thread);
                        threads.all.insert(entry.th32ThreadID, (cycles, text, start));
                    }
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            threads.cycles = std::arch::x86_64::_rdtsc();
        }
        threads.ticks = super::record::ticks();
        threads
    }
}

/// Names of addresses.
pub(super) struct Names {
    known: HashMap<usize, String>,
    symbols: Option<Symbols>,
}

/// The system's symbol library (asked for when a timeline is written, not
/// at start).
struct Symbols {
    from_address: unsafe extern "system" fn(*mut std::ffi::c_void, u64, *mut u64, *mut u8) -> i32,
    process: *mut std::ffi::c_void,
}

impl Symbols {
    fn load() -> Option<Symbols> {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExA};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        type Initialize = unsafe extern "system" fn(*mut std::ffi::c_void, *const u16, i32) -> i32;
        type Options = unsafe extern "system" fn(u32) -> u32;
        // SAFETY: the system's own library and functions, by their names and
        // signatures.
        unsafe {
            let module = LoadLibraryExA(c"dbghelp.dll".as_ptr().cast(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
            if module.is_null() {
                return None;
            }
            let options: Options = std::mem::transmute(GetProcAddress(module, c"SymSetOptions".as_ptr().cast())?);
            let initialize: Initialize = std::mem::transmute(GetProcAddress(module, c"SymInitializeW".as_ptr().cast())?);
            let from_address = std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                unsafe extern "system" fn(*mut std::ffi::c_void, u64, *mut u64, *mut u8) -> i32,
            >(GetProcAddress(module, c"SymFromAddr".as_ptr().cast())?);
            let process = GetCurrentProcess();
            // Undecorated names, loaded when asked for, no dialogs.
            options(0x2 | 0x4 | 0x200);
            // Our symbols lie beside the program, which names them by their
            // file alone. (It fails if a backtrace was asked for before:
            // then it is there, and looks beside every module.)
            let beside = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.to_path_buf())).unwrap_or_default();
            let beside: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(beside.as_os_str()).chain([0]).collect();
            initialize(process, beside.as_ptr(), 1);
            Some(Symbols { from_address, process })
        }
    }

    fn name(&self, address: usize) -> Option<String> {
        // `SYMBOL_INFO`: 88 bytes with the name's first character at 84.
        const NAME: usize = 84;
        const ROOM: usize = 512;
        let mut info = [0u64; (NAME + ROOM) / 8 + 1];
        let at = info.as_mut_ptr().cast::<u8>();
        let mut off = 0u64;
        // SAFETY: a buffer of the structure's size and the name's, its size
        // fields set as asked.
        unsafe {
            at.cast::<u32>().write(88);
            at.add(80).cast::<u32>().write(ROOM as u32);
            if (self.from_address)(self.process, address as u64 - 1, &mut off, at) == 0 {
                return None;
            }
            let name = std::ffi::CStr::from_ptr(at.add(NAME).cast());
            Some(name.to_string_lossy().into_owned())
        }
    }
}

impl Names {
    pub(super) fn new() -> Names {
        Names {
            known: HashMap::new(),
            symbols: Symbols::load(),
        }
    }

    /// The module `address` lies in, and where in it; whether it is ours.
    fn module(address: usize) -> (String, usize, bool) {
        use windows_sys::Win32::System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
            GetModuleHandleExW, GetModuleHandleW,
        };
        let mut module = std::ptr::null_mut();
        let mut path = [0u16; 260];
        // SAFETY: an address, no name; plain queries.
        unsafe {
            let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
            if GetModuleHandleExW(flags, address as *const u16, &mut module) == 0 {
                return ("?".into(), address, false);
            }
            let len = GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) as usize;
            let path = String::from_utf16_lossy(&path[..len]);
            let file = path.rsplit('\\').next().unwrap_or("?").to_string();
            (file, address - module as usize, module == GetModuleHandleW(std::ptr::null()))
        }
    }

    /// A name for `address`: our own functions by their names, others by
    /// their module and place in it.
    pub(super) fn of(&mut self, address: usize) -> String {
        if let Some(name) = self.known.get(&address) {
            return name.clone();
        }
        let (file, at, ours) = Self::module(address);
        let name = match self.symbols.as_ref().filter(|_| ours).and_then(|s| s.name(address)) {
            Some(name) => name,
            None => format!("{}+{at:#x}", file.strip_suffix(".dll").unwrap_or(&file)),
        };
        self.known.insert(address, name.clone());
        name
    }

    /// Where a stack says the thread was: the innermost functions that are
    /// not the standard library's or this module's.
    pub(super) fn site(&mut self, stack: &[usize; DEPTH]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for &address in stack.iter().take_while(|a| **a != 0) {
            let name = self.of(address);
            let plain = name.trim_start_matches('<').trim_start_matches("impl$").trim_start_matches(|c: char| c.is_ascii_digit());
            let skipped = ["std::", "core::", "alloc::", "windows_core::", "windows::"].iter().any(|p| plain.starts_with(p))
                || ["timeline::hooks::", "timeline::record::"].iter().any(|p| name.contains(p))
                || ["KERNEL", "ntdll", "kernel"].iter().any(|p| name.starts_with(p));
            if !skipped {
                parts.push(name.replace("oathkeep_gpu::", "").replace("oathkeep_hle::", "hle::"));
            }
            if parts.len() == 3 {
                break;
            }
        }
        parts.join(" < ")
    }
}

/// Writes both files.
pub(in crate::diagnostics::timeline) fn write(
    file: &std::path::Path,
    data: Data,
    before: &Threads,
    after: &Threads,
    stamps: Vec<(u64, u64)>,
) {
    let mut names = Names::new();
    let (events, lost) = (data.events.len(), data.lost);
    let trace = Trace::new(data.events, &stamps);
    let mut text = summary(&trace, &mut names, &data.stacks, before, after);
    if lost > 0 {
        let _ = writeln!(text, "\n{lost} events were lost: the buffer was full");
    }
    let beside = file.with_extension("txt");
    let written = std::fs::write(&beside, text).and_then(|()| {
        let mut out = std::io::BufWriter::new(std::fs::File::create(file)?);
        chrome(&trace, &mut names, &data.stacks, after, &mut out)?;
        std::io::Write::flush(&mut out)
    });
    match written {
        Ok(()) => tracing::info!(file = %file.display(), summary = %beside.display(), events, lost, "the timeline is written"),
        Err(e) => tracing::warn!(error = %e, file = %file.display(), "the timeline could not be written"),
    }
}
