//! One frame's D3D12 work recorded and run again, to find out what it costs
//! the card and to try changes on it in seconds (docs/replay.md).
//!
//! [`recorder`] hooks the host's D3D12 objects (`hook`) and keeps what a frame
//! needs; `snapshot` writes one frame down; [`player`] runs it. `wire` is what a
//! capture holds, `pso` pipeline descriptions, `states` the states textures
//! are in, `edit` the experiments, `gpu` helpers.

mod edit;
mod gpu;
mod hook;
pub mod player;
mod pso;
pub mod recorder;
mod snapshot;
mod states;
mod wire;

pub use player as play;
pub use recorder as record;
