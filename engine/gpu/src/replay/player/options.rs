//! Replay options, buffer placement filters, and timing/picture reports.

use super::*;

/// Where buffers the CPU saw lie in a replay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Buffers {
    /// Where they lay.
    #[default]
    AsRecorded,
    /// All in the card's memory.
    Card,
    /// All in the PC's memory, the card's own buffers too.
    Pc,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub loops: u32,
    /// Put back, before each run, what the run before changed.
    pub restore: bool,
    /// Time every command, not only lists.
    pub detailed: bool,
    /// Shaders listed.
    pub top: usize,
    /// Where the picture goes (default: `replay.png` in the capture).
    pub image: Option<PathBuf>,
    pub buffers: Buffers,
    /// Of those, only the ones all of these match ([`chosen`]).
    pub only_buffers: Vec<String>,
    pub no_barriers: bool,
    pub merge_barriers: bool,
    pub skip_clamps: bool,
    /// The clamps in batches (`edit::batch_clamps`); 2: carefully.
    pub batch_clamps: u32,
    pub skip_hash_passes: bool,
    pub one_submit: bool,
    /// Copies of what the CPU wrote leave alone what the card wrote in this
    /// run (`merge`).
    pub merge: bool,
    /// A full wait after every command that works.
    pub wait_all: bool,
    /// Each list sent on its own, and waited for.
    pub wait_lists: bool,
    /// Milliseconds the sender waits after each submission, as a title does
    /// that prepares the next one meanwhile: the card stands idle.
    pub pace: f64,
    /// The card kept awake meanwhile (`crate::backend::clock_keepalive`).
    pub awake: bool,
    /// Run only these lists (by their place in the frame).
    pub only: Option<(u32, u32)>,
    /// The last command that runs (list, command); after it only barriers.
    pub until: Option<(u32, usize)>,
    /// Skip what runs a pipeline whose name or shader hash has one of
    /// these in it.
    pub skip_shaders: Vec<String>,
    /// Shaders taken from this folder where it has them: `<hash>.dxil`, by
    /// the hash the runtime names the title's shader by (ours: by their own).
    pub shaders: Option<PathBuf>,
    /// With `compare`: the folder the shaders of the second are from.
    pub shaders_b: Option<PathBuf>,
    /// Run the frame unchanged first, and compare.
    pub compare: bool,
    /// The debug layer, if it is installed.
    pub debug: bool,
    /// The system's breadcrumbs after every command, as
    /// `OATHKEEP_D3D12_DRED=light` has them.
    pub breadcrumbs: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            loops: 5,
            restore: true,
            detailed: false,
            top: 20,
            image: None,
            buffers: Buffers::AsRecorded,
            only_buffers: Vec::new(),
            no_barriers: false,
            merge_barriers: false,
            skip_clamps: false,
            batch_clamps: 0,
            skip_hash_passes: false,
            one_submit: false,
            merge: true,
            wait_all: false,
            wait_lists: false,
            pace: 0.0,
            awake: false,
            only: None,
            until: None,
            skip_shaders: Vec::new(),
            shaders: None,
            shaders_b: None,
            compare: false,
            debug: false,
            breadcrumbs: false,
        }
    }
}

impl Options {
    pub(super) fn edited(&self) -> bool {
        self.no_barriers
            || self.merge_barriers
            || self.skip_clamps
            || self.batch_clamps > 0
            || self.skip_hash_passes
            || self.one_submit
            || self.wait_all
            || self.wait_lists
            || self.only.is_some()
            || !self.skip_shaders.is_empty()
    }

    /// The same without the experiments.
    pub(super) fn plain(&self) -> Options {
        Options {
            no_barriers: false,
            merge_barriers: false,
            skip_clamps: false,
            batch_clamps: 0,
            skip_hash_passes: false,
            one_submit: false,
            wait_all: false,
            wait_lists: false,
            awake: false,
            only: None,
            skip_shaders: Vec::new(),
            ..self.clone()
        }
    }
}

/// How a run went.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Per loop: from the first list's start to the last one's end, in ms.
    pub frame_ms: Vec<f64>,
    /// Per loop: how long the lists of submissions ran.
    pub busy_ms: Vec<f64>,
    /// Per loop: copies of ours that stand for what the CPU wrote.
    pub delta_ms: Vec<f64>,
    /// Of the last loop, by submission: lists, ms.
    pub submits: Vec<(usize, f64)>,
    /// With detailed timing: ms (median of the loops) and count by category.
    pub categories: Vec<(String, f64, u32)>,
    /// With detailed timing: ms, count and name of the costliest pipelines.
    pub shaders: Vec<(f64, u32, String)>,
    /// Against the presented picture: dB (infinite: the same), and whether
    /// its bytes are the same.
    pub picture: Option<(f64, bool)>,
    /// Of each loop's picture: dB against the presented one, and its hash.
    pub pictures: Vec<(f64, String)>,
    /// Things to know: from the capture and the replay.
    pub notes: Vec<String>,
}

impl Report {
    pub fn median_ms(&self) -> f64 {
        median(&self.frame_ms)
    }

    /// The report as text.
    pub fn text(&self, what: &str) -> String {
        let mut t = String::new();
        let ms = |v: &[f64]| {
            v.iter()
                .map(|x| format!("{x:.2}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let _ = writeln!(
            t,
            "{what}: {:.2} ms on the card (median of {})",
            self.median_ms(),
            self.frame_ms.len()
        );
        let _ = writeln!(t, "  loops, ms: {}", ms(&self.frame_ms));
        let (busy, delta) = (median(&self.busy_ms), median(&self.delta_ms));
        let idle = (self.median_ms() - busy - delta).max(0.0);
        let _ = writeln!(
            t,
            "  submissions busy {busy:.2} ms, copies standing for CPU writes {delta:.2} ms, between submissions {idle:.2} ms"
        );
        let _ = writeln!(t, "  by submission (lists: ms): {}", {
            self.submits
                .iter()
                .enumerate()
                .map(|(i, (n, ms))| format!("{i} ({n}): {ms:.2}"))
                .collect::<Vec<_>>()
                .join(", ")
        });
        if !self.categories.is_empty() {
            let _ = writeln!(t, "  by kind of command, ms (commands):");
            for (name, ms, n) in &self.categories {
                let _ = writeln!(t, "    {ms:8.2}  {n:6}  {name}");
            }
        }
        if !self.shaders.is_empty() {
            let _ = writeln!(t, "  costliest pipelines, ms (commands):");
            for (ms, n, name) in &self.shaders {
                let _ = writeln!(t, "    {ms:8.2}  {n:6}  {name}");
            }
        }
        match self.picture {
            Some((_, true)) => _ = writeln!(t, "  picture: the same bytes as presented"),
            Some((db, false)) => {
                _ = writeln!(
                    t,
                    "  picture: differs from the presented one, PSNR {db:.1} dB"
                )
            }
            None => {}
        }
        if self.pictures.len() > 1 {
            let all: Vec<String> = self
                .pictures
                .iter()
                .map(|(db, hash)| format!("{db:.1} dB {}", &hash[..8]))
                .collect();
            let kinds: HashSet<&String> = self.pictures.iter().map(|p| &p.1).collect();
            let _ = writeln!(
                t,
                "  pictures of the loops: {} different: {}",
                kinds.len(),
                all.join(", ")
            );
        }
        for n in &self.notes {
            let _ = writeln!(t, "  note: {n}");
        }
        t
    }
}

/// Whether `--only-buffers` means a buffer: all of `only` match it.
/// `name=<text in its name>`, `min=<bytes>`, `max=<bytes>`,
/// `seen=<how the CPU saw it>`, `id=<a>..<b>`; with `!` in front: that does not.
pub(super) fn chosen(r: &wire::Resource, only: &[String]) -> bool {
    only.iter().all(|f| {
        let (no, f) = match f.strip_prefix('!') {
            Some(f) => (true, f),
            None => (false, f.as_str()),
        };
        let (key, value) = f.split_once('=').unwrap_or(("name", f));
        let number = value.parse().unwrap_or(0u64);
        let is = match key {
            "min" => r.desc.width >= number,
            "max" => r.desc.width <= number,
            "seen" => {
                matches!(&r.place, Place::Committed { props, .. } if format!("{:?}", gpu::seen(props)).eq_ignore_ascii_case(value))
            }
            "id" => value.split_once("..").is_some_and(|(a, b)| {
                (a.parse().unwrap_or(0)..b.parse().unwrap_or(u32::MAX)).contains(&r.id)
            }),
            _ => r.name.contains(value),
        };
        is != no
    })
}

pub(super) fn median(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}
