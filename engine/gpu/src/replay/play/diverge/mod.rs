//! Where identical runs of a frame part: every list is run several times
//! from the very same memory (what the frame writes is put back before each,
//! and everything before it was run once), so what differs after it was made
//! different by it; then the first command of the list that does so, by
//! halving the list. The state after the last run is what the rest goes on
//! from, so each finding is a root, not a consequence.

use super::*;

mod compare;
mod inspect;
mod runner;
mod watch;

/// A part of memory compared as one.
const PIECE: usize = 64 << 10;

/// Where a run's contents come down: for each resource (and subresource) the
/// frame writes, its place in read-back memory.
pub(super) struct Reader {
    blocks: Vec<(ID3D12Resource, usize)>,
    pub(super) parts: Vec<Part>,
}

#[derive(Clone)]
pub(super) struct Part {
    pub(super) id: u32,
    /// `None` for a buffer.
    sub: Option<(u32, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)>,
    block: usize,
    at: u64,
    pub(super) len: u64,
}

/// One way to run commands `from..to` of a list.
struct Unit<'a> {
    submit: usize,
    cmds: &'a [Cmd],
    from: usize,
    to: usize,
    /// With the copies of what the CPU wrote before the submission.
    delta: bool,
}

/// What differed between runs: part and piece.
type Pieces = Vec<(usize, usize)>;

/// What to look at.
#[derive(Clone, Debug)]
pub struct Search {
    /// Runs of each list.
    pub runs: u32,
    /// Only these lists (the others are run once).
    pub lists: Option<(u32, u32)>,
    /// Roots looked for in one list.
    pub roots: u32,
    /// A full wait after each command.
    pub wait_all: bool,
    /// Experiments on the frame.
    pub options: Options,
}

impl Player {
    /// Looks for what makes identical runs differ; `say` gets the findings
    /// as they are made.
    pub fn diverge(&mut self, s: &Search, say: &mut dyn FnMut(String)) -> Result<()> {
        let mut report = Report::default();
        let clamps = if s.options.skip_clamps {
            self.clamp_results(&mut report)?
        } else {
            Vec::new()
        };
        let submits = self.edited(&s.options, &mut report, &clamps);
        self.restore(true)?;
        self.fresh = false;
        let ids: Vec<u32> = self.masters.iter().map(|m| m.0).collect();
        let reader = self.reader(&ids)?;
        let mb: u64 = reader.parts.iter().map(|p| p.len).sum::<u64>() >> 20;
        say(format!(
            "{} resources the frame writes ({mb} MB in {} parts) compared after each list, {} runs of each",
            self.masters.len(),
            reader.parts.len(),
            s.runs
        ));
        let mut states = self.start_states();
        let mut global = 0u32;
        for (i, lists) in submits.iter().enumerate() {
            for (j, cmds) in lists.iter().enumerate() {
                let here = global;
                global += 1;
                let delta = j == 0;
                let wanted = s.lists.is_none_or(|(a, b)| (a..=b).contains(&here));
                let mut from = 0;
                let mut roots = 0;
                loop {
                    let unit = Unit {
                        submit: i,
                        cmds,
                        from,
                        to: cmds.len(),
                        delta: delta && from == 0,
                    };
                    let looks = wanted && cmds[from..].iter().any(Cmd::works);
                    let runs = if looks { s.runs } else { 1 };
                    let pieces = self.trials(&reader, &unit, &states, runs, s.wait_all)?;
                    if pieces.is_empty() || roots >= s.roots {
                        self.commit(&unit, &mut states)?;
                        break;
                    }
                    roots += 1;
                    // The shortest start of the rest that differs.
                    let works: Vec<usize> =
                        (from..cmds.len()).filter(|&c| cmds[c].works()).collect();
                    let (mut lo, mut hi) = (0, works.len() - 1);
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cut = Unit {
                            to: works[mid] + 1,
                            ..unit
                        };
                        self.put_back(&states)?;
                        if self
                            .trials(&reader, &cut, &states, s.runs, s.wait_all)?
                            .is_empty()
                        {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    let at = works[lo];
                    let cut = Unit { to: at + 1, ..unit };
                    self.put_back(&states)?;
                    let pieces = self.trials(&reader, &cut, &states, s.runs.max(6), s.wait_all)?;
                    say(format!(
                        "list {here} (submission {i}, its list {j}), command {at} of {}: {}",
                        cmds.len(),
                        self.describe(cmds, at)
                    ));
                    self.put_back(&states)?;
                    for line in self.compare(&reader, &cut, &states, &pieces, s.wait_all)? {
                        say(format!("    {line}"));
                    }
                    self.commit(&cut, &mut states)?;
                    from = at + 1;
                    if from >= cmds.len() {
                        break;
                    }
                }
            }
            states.decay();
            if s.lists.is_some_and(|(_, b)| global > b) {
                break;
            }
            if i % 5 == 4 {
                say(format!("submission {i} done"));
            }
        }
        self.restore_all()?;
        Ok(())
    }

    /// Looks for where the picture's variants are decided: at each list (or,
    /// of one list, each `step` commands in `commands`) the rest of the frame
    /// is run several times from the same memory. Where the runs' pictures
    /// agree from a point on but not from the one before, what lies between
    /// decides.
    pub fn diverge_picture(
        &mut self,
        s: &Search,
        commands: Option<(u32, u32)>,
        step: usize,
        say: &mut dyn FnMut(String),
    ) -> Result<()> {
        let mut report = Report::default();
        let submits = self.edited(&s.options, &mut report, &[]);
        let flat: Vec<(usize, usize, &Vec<Cmd>)> = submits
            .iter()
            .enumerate()
            .flat_map(|(i, lists)| lists.iter().enumerate().map(move |(j, c)| (i, j, c)))
            .collect();
        self.restore(true)?;
        self.fresh = false;
        let mut states = self.start_states();
        for (here, &(i, j, cmds)) in flat.iter().enumerate() {
            let inside = s.lists.is_some_and(|(a, b)| a == b && a as usize == here);
            let looks = s
                .lists
                .is_none_or(|(a, b)| (a as usize..=b as usize).contains(&here));
            let works: Vec<usize> = (0..cmds.len()).filter(|&c| cmds[c].works()).collect();
            // Where the list is cut.
            let mut cuts: Vec<usize> = vec![0];
            if inside {
                let (a, b) = commands.unwrap_or((0, u32::MAX));
                let some = works
                    .iter()
                    .map(|&c| c + 1)
                    .filter(|c| (a as usize..=b as usize).contains(c));
                cuts.extend(some.step_by(step.max(1)));
            }
            for (k, &from) in cuts.iter().enumerate() {
                if looks && !works.is_empty() {
                    let mut pictures: Vec<(f64, String, Vec<u8>)> = Vec::new();
                    for run in 0..s.runs {
                        if run > 0 {
                            self.put_back(&states)?;
                        }
                        let mut now = states.clone();
                        let first = Unit {
                            submit: i,
                            cmds,
                            from,
                            to: cmds.len(),
                            delta: j == 0 && from == 0,
                        };
                        self.go(&first, s.wait_all)?;
                        now = self.after(&first, &now);
                        for (n, &(i, j, cmds)) in flat.iter().enumerate().skip(here + 1) {
                            if flat[n - 1].0 != i {
                                now.decay();
                            }
                            let unit = Unit {
                                submit: i,
                                cmds,
                                from: 0,
                                to: cmds.len(),
                                delta: j == 0,
                            };
                            self.go(&unit, s.wait_all)?;
                            now = self.after(&unit, &now);
                        }
                        self.at = now;
                        if let Some((db, _, hash, rgb)) = self.picture_now()? {
                            pictures.push((db, hash, rgb));
                        }
                    }
                    self.put_back(&states)?;
                    let least = pictures
                        .iter()
                        .enumerate()
                        .flat_map(|(a, x)| pictures[a + 1..].iter().map(|y| gpu::psnr(&x.2, &y.2)))
                        .fold(f64::INFINITY, f64::min);
                    let kinds: HashSet<&String> = pictures.iter().map(|p| &p.1).collect();
                    let db: Vec<String> = pictures.iter().map(|p| format!("{:.1}", p.0)).collect();
                    say(format!(
                        "from list {here} (submission {i}, its list {j}), command {from}: {} pictures of {}, the least alike {least:.1} dB; against the recorded: {}",
                        kinds.len(),
                        pictures.len(),
                        db.join(" ")
                    ));
                }
                // On to the next cut.
                let to = cuts.get(k + 1).copied().unwrap_or(cmds.len());
                let unit = Unit {
                    submit: i,
                    cmds,
                    from,
                    to,
                    delta: j == 0 && from == 0,
                };
                self.go(&unit, s.wait_all)?;
                self.commit(&unit, &mut states)?;
                self.at = states.clone();
                if inside && looks {
                    let last = (from..to).rev().find(|&c| cmds[c].works());
                    if let Some(c) = last {
                        say(format!("  command {c}: {}", self.describe(cmds, c)));
                    }
                }
            }
            if flat.get(here + 1).is_none_or(|n| n.0 != i) {
                states.decay();
                self.at = states.clone();
            }
            if s.lists.is_some_and(|(_, b)| here >= b as usize) {
                break;
            }
        }
        Ok(())
    }
}
