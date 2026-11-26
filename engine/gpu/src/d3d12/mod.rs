//! Console D3D12.X and DXGI COM interfaces implemented over host D3D12.

pub mod command_list;
pub mod command_queue;
pub mod device;
pub mod dxgi;
pub mod fence;
pub mod frame;
pub mod object;
pub mod pipeline;
pub mod query;
pub mod resource;
pub mod root;
#[cfg(windows)]
pub(crate) mod sampler;
pub mod signature;
