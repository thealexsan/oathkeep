//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod alloc;
mod mapping;
mod physical;
mod physical_io;
mod pool;
mod stats;

#[cfg(test)]
mod tests;

use std::sync::LazyLock;

use crate::{HleModule, hle_export};

pub use alloc::{
    BasicInformation, XMemAlloc, XMemAllocDefault, XMemFree, XMemFreeDefault,
    XMemIsToolsMemAvailable, XMemVirtualAlloc, XMemVirtualQuery,
};
pub use mapping::{XMemMapPhysicalPages, XMemMapPhysicalPagesScatter, map_alias, map_aliases};
pub(crate) use mapping::map_costs;
pub use physical::{XMemAllocatePhysicalPages, XMemFreePhysicalPages};
pub(crate) use physical_io::{
    page_window, pages_known, pages_shown, pages_written, stream_notice, write_physical,
};
pub use stats::{WorkingSet, XMemGetWorkingSetStatistics, XMemTransferMemory};
use stats::title_virtual_free;

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll", "kernel32.dll", "kernelbase.dll", "api-ms-win-core-memory-*"],
        exports: vec![
            crate::HleExport { name: "VirtualFree", address: title_virtual_free as *const () as usize },
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}
