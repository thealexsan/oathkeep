//! The replayer: makes a captured frame's objects again, puts their
//! contents back, and runs the frame as often as asked, with its time on
//! the card and experiments on it (`super::edit`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::File;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, D3D_PRIMITIVE_TOPOLOGY};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use super::edit::{self, Clamp, RESULTS};
use super::gpu::{self, Seen, Worker};
use super::snap::read_sub;
use super::states::{
    self, COMMON, COPY_DEST, COPY_SOURCE, DEPTH_WRITE, RENDER_TARGET, RESOLVE_DEST, States,
    UNORDERED_ACCESS,
};
use super::wire::{
    self, ALL, Barrier, Cmd, Content, Delta, Frame, Loc, MISSING, NONE, Place, Slot, Va, ZERO, slot,
};

mod build;
mod command;
mod contents;
mod descriptors;
mod device;
pub mod diverge;
mod edit_lists;
mod inspect;
pub mod look;
mod masters;
mod merge;
mod options;
mod pipelines;
pub mod pixel;
mod resources;
mod run;
mod upcopies;

use device::{alias, barriers, debug_messages, make_device, read_at, rows_equal, to_rect, uav_barrier};
pub use inspect::{dump, find, listing, title_shaders};
use inspect::title_hash;
pub use options::{Buffers, Options, Report};
use options::{chosen, median};

/// The name of the argument clamp's pipeline, by which its passes are
/// recognised.
pub const CLAMP: &str = "victory: argument clamp";
/// The name of the pipeline that compares and hashes card buffers.
pub const COMPARE: &str = "victory: card compare";

/// An upload buffer contents come back through.
const STAGING: u64 = 256 << 20;

/// Loads a capture and runs it: the whole of `victory replay`. The reports,
/// with what each is of.
pub fn replay(folder: &Path, options: &Options) -> Result<Vec<(String, Report)>> {
    let mut player = Player::load(folder, options)?;
    let mut out = Vec::new();
    if options.compare && (options.edited() || options.shaders_b.is_some()) {
        // Turn by turn: what else the machine does falls on both.
        let image = Some(player.folder.join("replay-unchanged.png"));
        let plain = Options {
            image,
            ..options.plain()
        };
        let mut a = player.prepare(&plain)?;
        // The lists of the first hold its pipelines by name alone: kept.
        let _pipelines = match &options.shaders_b {
            Some(folder) => {
                let kept = std::mem::take(&mut player.pipelines);
                player.make_pipelines(Some(folder))?;
                kept
            }
            None => HashMap::new(),
        };
        let mut b = player.prepare(options)?;
        let (mut stamps_a, mut stamps_b) = (Vec::new(), Vec::new());
        for _ in 0..options.loops.max(1) {
            stamps_a.push(player.once(&plain, &mut a)?);
            stamps_b.push(player.once(options, &mut b)?);
        }
        out.push(("unchanged".into(), player.finish(&plain, a, &stamps_a)?));
        out.push(("with the experiments".into(), player.finish(options, b, &stamps_b)?));
        return Ok(out);
    }
    let what = if options.edited() {
        "with the experiments"
    } else {
        "unchanged"
    };
    out.push((what.into(), player.run(options)?));
    Ok(out)
}

struct Object {
    res: ID3D12Resource,
    va: u64,
    /// In the capture's `resources`.
    index: usize,
    subs: u32,
}

/// One version of a shader-visible descriptor heap.
struct Version {
    heap: ID3D12DescriptorHeap,
    gpu: u64,
}

pub struct Player {
    pub folder: PathBuf,
    frame: Frame,
    device: ID3D12Device,
    worker: Worker,
    info: Option<ID3D12InfoQueue>,
    frequency: f64,
    objects: HashMap<u32, Object>,
    _heaps: HashMap<u32, ID3D12Heap>,
    roots: HashMap<u32, ID3D12RootSignature>,
    pipelines: HashMap<u32, ID3D12PipelineState>,
    signatures: HashMap<u32, ID3D12CommandSignature>,
    queries: HashMap<u32, ID3D12QueryHeap>,
    /// Descriptor heaps by id, in their versions; each submission's version.
    descriptors: HashMap<u32, (Vec<Version>, u64)>,
    version_of: Vec<usize>,
    /// Render target and depth views by what they are of.
    targets: HashMap<Slot, D3D12_CPU_DESCRIPTOR_HANDLE>,
    _target_heaps: Vec<ID3D12DescriptorHeap>,
    /// Copies of what the frame changes, as at its start.
    masters: Vec<(u32, ID3D12Resource)>,
    /// What the CPU wrote between submissions, and for each submission
    /// where in it.
    deltas: Option<ID3D12Resource>,
    delta_parts: Vec<Vec<(Delta, u64)>>,
    /// States at the start of the frame and after it.
    start: HashMap<u32, Vec<u32>>,
    end: HashMap<u32, Vec<u32>>,
    /// For `Options::merge`.
    merger: Option<merge::Merger>,
    merging: bool,
    /// Resources the card may write in the frame.
    card_writes: HashSet<u32>,
    /// The results of clamp passes ([`RESULTS`]).
    results: Option<ID3D12Resource>,
    /// Other buffers of ours that commands name.
    extra: HashMap<u32, ID3D12Resource>,
    commands: Vec<Vec<Cmd>>,
    /// Resources in the states of the start (not those of the end).
    fresh: bool,
    /// States as the last run left them (`diverge`).
    at: States,
    notes: Vec<String>,
}

/// What a command of a list is timed as.
type Stamp = (usize, &'static str, u32);

/// With a stamp of a copy to a texture: this bit and the texture's number
/// stand for the pipeline's.
const COPY_TO: u32 = 1 << 31;

/// A list made from commands, with what its timestamps are of.
struct Built {
    _allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    /// The query of its start; its end is the one before the next list's.
    first: u32,
    last: u32,
    stamps: Vec<Stamp>,
}

/// Everything to send for a frame.
struct Sends {
    /// For each submission: the copies of what the CPU wrote, its lists.
    steps: Vec<(Option<Built>, Vec<Built>)>,
    joints: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)>,
    resolve: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    _heap: ID3D12QueryHeap,
    _back: ID3D12Resource,
    data: usize,
    count: u32,
}

impl Player {
    pub fn load(folder: &Path, options: &Options) -> Result<Player> {
        let folder = find(folder)?;
        let began = Instant::now();
        let frame = Frame::read(&folder.join("frame.bin"))?;
        let (device, info) = make_device(&frame, options.debug, options.breadcrumbs)?;
        // SAFETY: plain creation and queries.
        let (queue, frequency) = unsafe {
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })?;
            let frequency = queue.GetTimestampFrequency()? as f64;
            (queue, frequency)
        };
        let commands: Vec<Vec<Cmd>> = frame
            .lists
            .iter()
            .map(|l| Cmd::all(&l.commands.0))
            .collect::<std::result::Result<_, _>>()?;
        let mut player = Player {
            folder,
            worker: Worker::new(&device, &queue)?,
            device,
            info,
            frequency,
            objects: HashMap::new(),
            _heaps: HashMap::new(),
            roots: HashMap::new(),
            pipelines: HashMap::new(),
            signatures: HashMap::new(),
            queries: HashMap::new(),
            descriptors: HashMap::new(),
            version_of: Vec::new(),
            targets: HashMap::new(),
            _target_heaps: Vec::new(),
            masters: Vec::new(),
            deltas: None,
            delta_parts: Vec::new(),
            start: HashMap::new(),
            end: HashMap::new(),
            merger: None,
            merging: false,
            card_writes: HashSet::new(),
            results: None,
            extra: HashMap::new(),
            commands,
            fresh: true,
            at: States::default(),
            notes: frame
                .notes
                .iter()
                .map(|n| format!("recorder: {n}"))
                .collect(),
            frame,
        };
        player.check()?;
        let mut steps: Vec<(&str, std::time::Duration)> = Vec::new();
        let mut step = |what: &'static str, at: &mut Instant| {
            steps.push((what, at.elapsed()));
            *at = Instant::now();
        };
        let mut at = Instant::now();
        player.make_resources(options.buffers, &options.only_buffers)?;
        step("resources", &mut at);
        player.make_pipelines(options.shaders.as_deref())?;
        step("pipelines", &mut at);
        player.make_descriptors()?;
        step("descriptors", &mut at);
        player.load_contents()?;
        step("contents", &mut at);
        player.make_masters(options.buffers, options.restore)?;
        step("copies to restore from", &mut at);
        let steps: Vec<String> = steps
            .iter()
            .map(|(w, d)| format!("{w} {:.1} s", d.as_secs_f64()))
            .collect();
        if let Some([budget, used, pc]) = player.memory() {
            let over = if used + 256 > budget {
                ": over the budget, so the system moves memory to the PC's and the times are not the card's"
            } else {
                ""
            };
            player.notes.push(format!(
                "{used} MB of the card's memory used, budget {budget} MB; {pc} MB of the PC's{over}"
            ));
        }
        player.notes.push(format!(
            "loaded in {:.1} s: {}",
            began.elapsed().as_secs_f64(),
            steps.join(", ")
        ));
        Ok(player)
    }

    fn note(&mut self, n: String) {
        tracing::warn!("{n}");
        self.notes.push(n);
    }

    /// Says what of the frame is not there.
    fn check(&mut self) -> Result<()> {
        let missing = self
            .frame
            .submits
            .iter()
            .flat_map(|s| &s.lists)
            .filter(|&&l| l == MISSING)
            .count();
        if missing > 0 {
            self.note(format!(
                "{missing} lists of the frame were not recorded: the frame is incomplete"
            ));
        }
        if self.frame.submits.is_empty() {
            bail!("the capture has no submission");
        }
        let unknown = self
            .commands
            .iter()
            .flatten()
            .filter(|c| {
                matches!(
                    c,
                    Cmd::RootView {
                        va: Va::Unknown { .. },
                        ..
                    }
                )
            })
            .count();
        if unknown > 0 {
            self.note(format!(
                "{unknown} root views name memory in no known buffer: they are null in the replay"
            ));
        }
        Ok(())
    }

    fn resource(&self, id: u32) -> Option<&ID3D12Resource> {
        if id == RESULTS {
            return self.results.as_ref();
        }
        self.objects
            .get(&id)
            .map(|o| &o.res)
            .or_else(|| self.extra.get(&id))
    }

    fn va(&self, va: &Va) -> u64 {
        match va {
            Va::At { res, offset } => self.objects.get(res).map_or(0, |o| o.va + offset),
            _ => 0,
        }
    }

    /// The id of the resource of a name (tests).
    pub fn named(&self, name: &str) -> Option<u32> {
        self.frame
            .resources
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_are_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find(dir.path()).is_err());
        for n in [1, 2, 10, 11] {
            std::fs::create_dir(dir.path().join(format!("capture-{n}"))).unwrap();
        }
        for n in [1, 2, 10] {
            std::fs::write(dir.path().join(format!("capture-{n}/frame.bin")), "").unwrap();
        }
        // The last complete one; itself when named.
        assert_eq!(find(dir.path()).unwrap(), dir.path().join("capture-10"));
        assert_eq!(
            find(&dir.path().join("capture-2")).unwrap(),
            dir.path().join("capture-2")
        );
    }

    #[test]
    fn reports() {
        assert_eq!(median(&[]), 0.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        let r = Report {
            frame_ms: vec![10.0, 12.0, 11.0],
            busy_ms: vec![9.0; 3],
            delta_ms: vec![0.5; 3],
            submits: vec![(2, 9.0)],
            picture: Some((40.0, false)),
            ..Default::default()
        };
        let t = r.text("a frame");
        assert!(t.contains("a frame: 11.00 ms"), "{t}");
        assert!(t.contains("between submissions 1.50 ms"), "{t}");
        assert!(t.contains("PSNR 40.0 dB"), "{t}");
        let o = Options {
            skip_clamps: true,
            buffers: Buffers::Card,
            ..Default::default()
        };
        assert!(o.edited() && !o.plain().edited() && o.plain().buffers == Buffers::Card);
    }
}
