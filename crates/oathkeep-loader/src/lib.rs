#[cfg(windows)]
mod console_insn;
mod crash;
pub mod prepare;
pub mod resolve;
#[cfg(all(windows, feature = "diagnostics"))]
mod stacks;
#[cfg(windows)]
mod threads;
#[cfg(windows)]
mod thunks;
#[cfg(windows)]
mod tls;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use console_insn::set_demand_reserved_end;
#[cfg(all(windows, feature = "diagnostics"))]
pub use crash::save_on_crash;
#[cfg(windows)]
pub use crash::{
    locate, log_crash, log_crashes_to, note_title_output, report_crashes, report_unhandled,
};
pub use prepare::{PrepareError, PreparedImage, prepare};
pub use resolve::{ImportResolver, Provenance, Resolution};
#[cfg(all(windows, feature = "diagnostics"))]
pub use stacks::{dump_stacks, dump_stacks_on_schedule};
#[cfg(windows)]
pub use threads::disable_thread_calls;
#[cfg(windows)]
pub use thunks::{UnimplementedPolicy, unimplemented_call_count};
#[cfg(windows)]
pub use windows::{
    BindReport, LoadedImage, MissingImport, WithTitleDlls, load, load_title_dll, load_title_dlls,
};

pub const DLL_PROCESS_ATTACH: u32 = 1;
pub const DLL_PROCESS_DETACH: u32 = 0;
