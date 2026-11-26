//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap
//! (unless `OATHKEEP_LATE_WAIT=1`, see [`late`]);
//! overlap them with per-submission fences once more than clears run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    pub(super) queue: ID3D12CommandQueue,
    pub(super) fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    pub(super) submitted: Mutex<u64>,
    pub(super) event: HANDLE,
    pub(super) rtvs: ID3D12DescriptorHeap,
    pub(super) rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    pub(super) dsvs: ID3D12DescriptorHeap,
    pub(super) dsv_size: u32,
    pub(super) next_dsv: AtomicU32,
    /// With `OATHKEEP_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    pub(super) messages: Option<ID3D12InfoQueue>,
    pub(super) next_rtv: AtomicU32,
    pub(super) presenter: Mutex<Option<Presenter>>,
    /// The adapter the device is on, for its memory budget
    /// ([`Host::video_memory`]); `None` if the system does not name it.
    pub(super) adapter: Option<IDXGIAdapter3>,
    /// 1-descriptor shader-visible fallback heaps bound when a command list
    /// omits a view or sampler heap while using directly-indexed root signatures.
    pub(super) fallback_view: OnceLock<Option<ID3D12DescriptorHeap>>,
    pub(super) fallback_sampler: OnceLock<Option<ID3D12DescriptorHeap>>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}


pub(crate) mod heap_pass_async;
pub(crate) mod clock_keepalive;
mod gpu_steps;
pub(crate) mod dred_indirects;
#[cfg(feature = "diagnostics")]
mod hud_overlay;
mod inline_copies;
mod upload_snapshot;
pub(crate) use upload_snapshot::{goes_as_words, words_under};
#[cfg(feature = "diagnostics")]
pub(crate) mod gpu_spans;
mod torn_read_check;
#[cfg(feature = "diagnostics")]
mod bus_traffic;
pub(crate) mod upload_verify;
#[cfg(feature = "diagnostics")]
mod timeline_stamps;
#[cfg(feature = "diagnostics")]
pub(crate) use timeline_stamps::{timeline_ready, timeline_restart, timeline_stamps};


mod batch;
mod buffers;
mod card_before;
mod card_compare;
mod clock;
mod create;
mod dred;
mod execute;
mod late;
mod late_blame;
mod late_land;
mod list;
mod list_defer;
mod list_indirect;
mod list_notes;
mod list_twins;
mod mapped_io;
mod plane_pass;
mod plane_shaders;
mod present;
mod present_meter;
mod relay;
mod relay_card;
mod relay_land;
mod retire;
mod scratch;
mod split;
mod submit;
mod host_trail;
mod vram;
mod walls;
mod window;
#[cfg(test)]
mod tests;

pub(super) use batch::*;
pub(super) use buffers::*;
pub(super) use card_before::*;
pub(super) use card_compare::*;
pub use clock::on_failure;
pub(super) use clock::*;
pub(super) use create::*;
pub(super) use dred::*;
pub(super) use execute::*;
pub(super) use late::*;
pub(super) use late_blame::*;
pub(super) use late_land::*;
pub(super) use list::*;
pub(super) use list_defer::*;
pub(super) use list_indirect::*;
pub(super) use list_notes::*;
pub(super) use list_twins::*;
pub(super) use mapped_io::*;
pub(super) use plane_pass::*;
pub(super) use plane_shaders::*;
pub(super) use present::*;
pub(super) use present_meter::*;
pub(super) use relay::*;
pub(super) use relay_card::*;
pub(super) use relay_land::*;
pub(super) use retire::*;
pub(super) use scratch::*;
pub(super) use split::*;
pub(super) use submit::*;
pub(super) use host_trail::*;
pub(super) use vram::*;
pub(super) use walls::*;
pub(super) use window::*;
