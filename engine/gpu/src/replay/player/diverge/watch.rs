//! Buffer word watching and per-draw divergence checks.

use super::super::*;
use super::{Search, Unit};

impl Player {
    /// Says which lists (of the list in `s.lists`: which commands) change
    /// words of a buffer, and to what.
    pub fn watch(
        &mut self,
        s: &Search,
        parts: &[(u32, u64, u64)],
        say: &mut dyn FnMut(String),
    ) -> Result<()> {
        let mut report = Report::default();
        let submits = self.edited(&s.options, &mut report, &[]);
        self.restore(true)?;
        self.fresh = false;
        let words = |p: &mut Player| -> Result<Vec<Vec<u32>>> {
            parts
                .iter()
                .map(|&(id, at, len)| {
                    let bytes = p.slice(id, at, len)?;
                    Ok(bytes
                        .chunks_exact(4)
                        .map(|c| u32::from_le_bytes(c.try_into().unwrap_or_default()))
                        .collect())
                })
                .collect()
        };
        let mut was = words(self)?;
        say(format!("at the start: {was:x?}"));
        let mut global = 0u32;
        for (i, lists) in submits.iter().enumerate() {
            for (j, cmds) in lists.iter().enumerate() {
                let here = global;
                global += 1;
                let inside = s.lists.is_some_and(|(a, b)| (a..=b).contains(&here));
                let mut cuts: Vec<usize> = vec![0];
                if inside {
                    cuts.extend((0..cmds.len()).filter(|&c| cmds[c].works()).map(|c| c + 1));
                }
                if *cuts.last().unwrap_or(&0) != cmds.len() {
                    cuts.push(cmds.len());
                }
                for k in 0..cuts.len() - 1 {
                    let unit = Unit {
                        submit: i,
                        cmds,
                        from: cuts[k],
                        to: cuts[k + 1],
                        delta: j == 0 && k == 0,
                    };
                    self.go(&unit, s.wait_all)?;
                    let now = words(self)?;
                    if now != was {
                        if inside {
                            let last = (unit.from..unit.to)
                                .rev()
                                .find(|&c| cmds[c].works())
                                .unwrap_or(unit.to - 1);
                            say(format!(
                                "list {here} (submission {i}, its list {j}) command {last}: {}",
                                self.describe(cmds, last)
                            ));
                        } else {
                            say(format!("list {here} (submission {i}, its list {j})"));
                        }
                        for (n, (a, b)) in was.iter().zip(&now).enumerate() {
                            let changed: Vec<String> = a
                                .iter()
                                .zip(b)
                                .enumerate()
                                .filter(|(_, (a, b))| a != b)
                                .take(40)
                                .map(|(w, (a, b))| format!("+{:#x}: {a:x} -> {b:x}", w * 4))
                                .collect();
                            if !changed.is_empty() {
                                say(format!("    part {n}: {}", changed.join(", ")));
                            }
                        }
                        was = now;
                    }
                }
                if s.options.only.is_some_and(|(_, b)| here >= b) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Each draw of one list whose pipeline's name has `text` in it, run
    /// alone (with what prepares its arguments) from the memory at the list's
    /// start, `runs` times: how many different contents its targets get.
    pub fn diverge_draws(
        &mut self,
        s: &Search,
        text: &str,
        say: &mut dyn FnMut(String),
    ) -> Result<()> {
        let mut report = Report::default();
        let submits = self.edited(&s.options, &mut report, &[]);
        let flat: Vec<(usize, usize, &Vec<Cmd>)> = submits
            .iter()
            .enumerate()
            .flat_map(|(i, lists)| lists.iter().enumerate().map(move |(j, c)| (i, j, c)))
            .collect();
        let wanted = s.lists.map_or(0, |l| l.0 as usize);
        self.restore(true)?;
        self.fresh = false;
        let mut states = self.start_states();
        for (here, &(i, j, cmds)) in flat.iter().enumerate() {
            if here < wanted {
                let unit = Unit {
                    submit: i,
                    cmds,
                    from: 0,
                    to: cmds.len(),
                    delta: j == 0,
                };
                self.go(&unit, s.wait_all)?;
                self.commit(&unit, &mut states)?;
                if flat.get(here + 1).is_none_or(|n| n.0 != i) {
                    states.decay();
                }
                self.at = states.clone();
                continue;
            }
            let mut last = 0;
            let mut pso = NONE;
            let mut targets: Vec<u32> = Vec::new();
            for (c, cmd) in cmds.iter().enumerate() {
                match cmd {
                    Cmd::Pipeline { pso: p } => pso = *p,
                    Cmd::Targets { colors, depth } => {
                        targets = colors
                            .iter()
                            .chain(depth)
                            .map(|s| s.res)
                            .filter(|r| *r != NONE)
                            .collect()
                    }
                    _ => {}
                }
                if !matches!(
                    cmd,
                    Cmd::Indirect { .. }
                        | Cmd::Draw { .. }
                        | Cmd::DrawIndexed { .. }
                        | Cmd::DispatchMesh { .. }
                ) {
                    continue;
                }
                let from = std::mem::replace(&mut last, c + 1);
                if !self.pipeline_name(pso).contains(text) {
                    continue;
                }
                targets.sort_unstable();
                targets.dedup();
                let reader = self.reader(&targets)?;
                let unit = Unit {
                    submit: i,
                    cmds,
                    from,
                    to: c + 1,
                    delta: false,
                };
                // Barriers before it in the list are made up for: textures
                // are in the states the list has them in here.
                let mut before = states.clone();
                let mut all = Vec::new();
                cmds[..from]
                    .iter()
                    .for_each(|c| states::changes(c, &mut all));
                all.iter().for_each(|c| before.change(c));
                let after = self.after(&unit, &before);
                let mut kinds: HashMap<Vec<Vec<u64>>, u32> = HashMap::new();
                for _ in 0..s.runs {
                    let at = self.at.clone();
                    self.copy_masters(&at, &before, false)?;
                    self.at = before.clone();
                    self.go(&unit, s.wait_all)?;
                    self.at = after.clone();
                    *kinds.entry(self.hashes_now(&reader, &after)?).or_default() += 1;
                }
                let at = self.at.clone();
                self.copy_masters(&at, &before, false)?;
                self.at = before.clone();
                if kinds.len() > 1 || std::env::var_os("OATHKEEP_REPLAY_PEEK").is_some() {
                    self.peek(cmds, c, i, say)?;
                }
                let mut counts: Vec<u32> = kinds.values().copied().collect();
                counts.sort_unstable();
                say(format!(
                    "command {c} (from {from}) {:?} of {}: {} outcomes {counts:?}",
                    cmd,
                    self.pipeline_name(pso),
                    counts.len()
                ));
            }
            break;
        }
        Ok(())
    }
}
