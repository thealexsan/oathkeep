//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! Organized by domain:
//! - [`d3d12`]: console D3D12.X and DXGI COM objects (`device`, `dxgi`,
//!   `resource`, `fence`, `command_queue`, `command_list`, `root`, `query`,
//!   `signature`, `pipeline`, `frame`, `object`, `sampler`).
//! - `backend`: host D3D12 device, submission, swapchain present, and DRED.
//! - [`descriptors`]: console descriptor heaps and host heap mirroring.
//! - `execution`: draw/dispatch recording, indirect batching, and composition.
//! - `textures`: texture staging, detiling, prefetch, and readback.
//! - `memory`: buffer shadowing, page write-watching, tiling, and pools.
//! - `replay`: frame capture and offline host D3D12 replay (`diagnostics`).
//! - [`diagnostics`]: timelines, copy stats, memory ledgers, and toggles.
//! - [`console`]: console hardware identity seen by the title.
//! - `shader_cache`: persistent translated shader cache.

#[cfg(windows)]
pub(crate) mod backend;
pub mod console;
pub mod d3d12;
pub mod descriptors;
pub mod diagnostics;
#[cfg(windows)]
pub(crate) mod execution;
#[cfg(windows)]
pub mod memory;
#[cfg(all(windows, feature = "diagnostics"))]
pub mod replay;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
pub(crate) mod textures;

// External API compatibility re-exports (`oathkeep_gpu::*`).
pub use d3d12::{
    command_list as list, command_queue as queue, device, dxgi, fence, frame, object, pipeline,
    query, resource, root, signature,
};
pub use descriptors as descriptor;
pub use diagnostics::timeline;
#[cfg(windows)]
pub use memory::title_write as titlewrite;

#[cfg(windows)]
pub use backend::on_failure;

/// Whether `switch` is on now, if it is one of those `OATHKEEP_AB` turns on and
/// off while the title runs (`ab_toggle`).
#[cfg(all(windows, feature = "diagnostics"))]
pub fn switched(switch: &str) -> Option<bool> {
    diagnostics::ab_toggle::pick(switch)
}

/// What the frame is at (`copy_stats::phase`), for the sampling profiler.
#[cfg(all(windows, feature = "diagnostics"))]
pub fn frame_phase() -> u32 {
    diagnostics::copy_stats::phase()
}

#[cfg(windows)]
pub use memory::inplace::{
    title_memory_freeing, title_memory_section, title_memory_section_gone, title_memory_view,
};
#[cfg(windows)]
pub use memory::watch::{
    kernel_writing, title_memory_allocated, title_memory_freed, title_memory_reading,
    title_memory_remapped, title_memory_remapping, title_memory_splitting, title_memory_unmapping,
    title_memory_written, writing as kernel_will_write,
};
