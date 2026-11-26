//! `victory replay`: a captured frame run again (docs/replay.md).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use oathkeep_hle::replay::play::{self, Options};

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Timing {
    /// Each list.
    Lists,
    /// Each command (adds some time of its own).
    Detailed,
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Buffers {
    AsRecorded,
    /// All in the card's memory.
    Vram,
    /// All in the PC's memory.
    Sysmem,
}

#[derive(Args)]
pub(crate) struct Replay {
    /// The folder given as OATHKEEP_RECORD (its last capture) or a capture.
    pub(super) folder: PathBuf,
    /// How often the frame is run.
    #[arg(long, default_value_t = 5)]
    pub(super) loops: u32,
    /// Do not put back, between runs, what a run changed.
    #[arg(long)]
    pub(super) no_restore: bool,
    #[arg(long, value_enum, default_value_t = Timing::Lists)]
    pub(super) timing: Timing,
    /// Pipelines listed with detailed timing.
    #[arg(long, default_value_t = 20)]
    pub(super) top: usize,
    /// Where the picture goes (default: replay.png in the capture).
    #[arg(long)]
    pub(super) image: Option<PathBuf>,
    /// Run the frame unchanged first, and print both.
    #[arg(long)]
    pub(super) compare: bool,
    /// Leave out unordered-access barriers.
    #[arg(long)]
    pub(super) no_barriers: bool,
    /// Join barrier calls with no work between them.
    #[arg(long)]
    pub(super) merge_barriers: bool,
    /// Leave out the argument clamps: what they bound is kept from a first run.
    #[arg(long)]
    pub(super) skip_clamps: bool,
    /// The argument clamps in batches, as OATHKEEP_INDIRECT_BATCH does (2:
    /// a wait before every indirect dispatch).
    #[arg(long, num_args = 0..=1, default_missing_value = "1", default_value_t = 0)]
    pub(super) batch_clamps: u32,
    /// Leave out the hash and compare passes over card buffers.
    #[arg(long)]
    pub(super) skip_hash_passes: bool,
    /// Where buffers lie.
    #[arg(long, value_enum, default_value_t = Buffers::AsRecorded)]
    pub(super) buffers: Buffers,
    /// With --buffers, only the buffers all of these match: `name=<text>`,
    /// `min=<bytes>`, `max=<bytes>`, `seen=<No, Cached, Combined>`,
    /// `id=<a>..<b>`, or `!` and one of those (may be given several times).
    #[arg(long)]
    pub(super) only_buffers: Vec<String>,
    /// One ExecuteCommandLists for the whole frame.
    #[arg(long)]
    pub(super) one_submit: bool,
    /// Let copies of what the CPU wrote also overwrite what the card wrote in
    /// the replay (as the recorded commands say).
    #[arg(long)]
    pub(super) no_merge: bool,
    /// A full wait after every draw, dispatch, copy and clear.
    #[arg(long)]
    pub(super) wait_all: bool,
    /// Each list sent on its own, and waited for.
    #[arg(long)]
    pub(super) wait_lists: bool,
    /// Milliseconds to wait after sending each submission: the card stands
    /// idle between them, as it does while the title prepares the next.
    #[arg(long, default_value_t = 0.0)]
    pub(super) pace: f64,
    /// Keep the card's clocks up meanwhile (OATHKEEP_CARD_AWAKE=1).
    #[arg(long)]
    pub(super) awake: bool,
    /// The system's breadcrumbs after every command (OATHKEEP_D3D12_DRED=light).
    #[arg(long)]
    pub(super) breadcrumbs: bool,
    /// Run only these lists, by their place in the frame (`3..7`, or `5`);
    /// of the others only barriers.
    #[arg(long)]
    pub(super) only: Option<String>,
    /// Leave out what runs a pipeline whose name or DXIL hash has this in it
    /// (may be given several times).
    #[arg(long = "skip-shader")]
    pub(super) skip_shader: Vec<String>,
    /// Find the commands that make identical runs differ: each list is run
    /// this often from the same memory.
    #[arg(long, num_args = 0..=1, default_missing_value = "3")]
    pub(super) diverge: Option<u32>,
    /// Find where the picture's variants are decided: from each list on, the
    /// rest of the frame is run this often from the same memory.
    #[arg(long, num_args = 0..=1, default_missing_value = "5")]
    pub(super) diverge_picture: Option<u32>,
    /// Run each draw of one list (--lists) whose pipeline's name has this in
    /// it alone, --diverge times: how many outcomes it has.
    #[arg(long)]
    pub(super) draws: Option<String>,
    /// Say which lists (with --lists: commands) change words of buffers:
    /// `<resource>:<offset>:<bytes>` (hexadecimal; repeatable).
    #[arg(long)]
    pub(super) watch: Vec<String>,
    /// With --diverge-picture and one list in --lists: its commands (`0..900`)
    /// and how many are stepped over at a time.
    #[arg(long)]
    pub(super) commands: Option<String>,
    #[arg(long, default_value_t = 1)]
    pub(super) step: usize,
    /// With --diverge and --diverge-picture: only these lists (`3..7`).
    #[arg(long)]
    pub(super) lists: Option<String>,
    /// With --diverge: the most commands named of one list.
    #[arg(long, default_value_t = 4)]
    pub(super) roots: u32,
    /// Name resources the frame changes that the replay does not put back.
    #[arg(long)]
    pub(super) check_written: bool,
    /// Run the frame twice and say which words the copies of the CPU's sides
    /// of card buffers bring that the card holds another value of, and what
    /// shaders wrote that was not declared as written.
    #[arg(long)]
    pub(super) upcopies: bool,
    /// Name what the heap slots hold that a command's constants name:
    /// `<list>:<command>`.
    #[arg(long)]
    pub(super) slots: Option<String>,
    /// Only write the DXIL of pipelines with this in their name to the
    /// capture's `shaders` folder.
    #[arg(long)]
    pub(super) dump: Option<String>,
    /// Only write the frame's buffers and the commands of these lists (`3..7`).
    #[arg(long)]
    pub(super) listing: Option<String>,
    /// Only write what the dispatches of these lists have bound (`3..7`).
    #[arg(long)]
    pub(super) flow: Option<String>,
    /// With --flow: draws too.
    #[arg(long)]
    pub(super) flow_draws: bool,
    /// With --flow: heap slots shown from each place named.
    #[arg(long, default_value_t = 40)]
    pub(super) flow_slots: u32,
    /// Run the frame once and save textures as pictures in this folder, with
    /// what values they hold (see --texture, --until).
    #[arg(long)]
    pub(super) images: Option<PathBuf>,
    /// With --images: `<resource>[.<subresource>]` (repeatable); none: all
    /// that the frame's shaders write.
    #[arg(long)]
    pub(super) texture: Vec<String>,
    /// With --images and no --texture: from this many texels on.
    #[arg(long, default_value_t = 4096)]
    pub(super) least: u64,
    /// With --images: the widest picture.
    #[arg(long, default_value_t = 960)]
    pub(super) width: u32,
    /// With --images: the values too.
    #[arg(long)]
    pub(super) raw: bool,
    /// The last command that runs, `<list>:<command>`; after it only barriers.
    #[arg(long)]
    pub(super) until: Option<String>,
    /// Say which commands change the pixel: `<x>,<y>` of the presented
    /// picture, or `<resource>[.<subresource>]:<x>,<y>` of a texture
    /// (repeatable). With --lists, only those are looked into.
    #[arg(long)]
    pub(super) pixel: Vec<String>,
    /// With --pixel: the size of the picture the points are of (`1280x720`),
    /// if it is another than the presented one's.
    #[arg(long)]
    pub(super) of: Option<String>,
    /// With --pixel: depth targets too.
    #[arg(long)]
    pub(super) depth: bool,
    /// With --pixel: leave out textures that shaders may write (unordered
    /// access).
    #[arg(long)]
    pub(super) targets_only: bool,
    /// With --pixel: indirect calls whose draws are run one by one:
    /// `<list>:<command>` (repeatable).
    #[arg(long)]
    pub(super) split: Vec<String>,
    /// Take shaders from this folder where it has them (`<hash>.dxil`, by the
    /// hash the runtime names the title's shader by).
    #[arg(long)]
    pub(super) shaders: Option<PathBuf>,
    /// With --compare: the folder the second run's shaders are from.
    #[arg(long)]
    pub(super) shaders_b: Option<PathBuf>,
    /// Only translate the capture's shaders, into this folder (for --shaders),
    /// with the translator's settings of this run. Containers are looked up in
    /// `containers` beside the capture, and first collected there from the
    /// title's files.
    #[arg(long)]
    pub(super) retranslate: Option<PathBuf>,
    /// With --retranslate: the view heap's null slot, and where raw twins of
    /// views start, as the capture's shaders have them (0: none).
    #[arg(long, default_value_t = 0)]
    pub(super) view_limit: u32,
    #[arg(long, default_value_t = 0)]
    pub(super) raw_views: u32,
    /// Use the D3D12 debug layer (slow).
    #[arg(long)]
    pub(super) debug_layer: bool,
}

/// `3..7` (both included) or `5`.
pub(super) fn range(text: &str) -> Option<(u32, u32)> {
    match text.split_once("..") {
        Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
        None => text.trim().parse().ok().map(|n| (n, n)),
    }
}

/// Two numbers with `by` between them.
pub(super) fn pair(text: &str, by: char) -> Option<(u32, u32)> {
    let (a, b) = text.split_once(by)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

mod exec;

pub(crate) use exec::run;

#[cfg(test)]
mod tests {
    #[test]
    fn ranges() {
        assert_eq!(super::range("3..7"), Some((3, 7)));
        assert_eq!(super::range("5"), Some((5, 5)));
        assert_eq!(super::range("x"), None);
        assert_eq!(super::pair("12,7", ','), Some((12, 7)));
        assert_eq!(super::pair("12", ','), None);
        assert_eq!(super::range("1..x"), None);
    }
}
