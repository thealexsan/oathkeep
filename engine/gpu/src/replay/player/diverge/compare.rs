//! Written-resource verification and detailed byte comparison between divergent runs.

use super::super::*;
use super::{Pieces, Reader, Unit};

impl Player {
    /// Says which resources that are not put back before a run are changed
    /// by one all the same.
    pub fn check_written(&mut self, say: &mut dyn FnMut(String)) -> Result<()> {
        // What is put back, is as it was.
        {
            let ids: Vec<u32> = self.masters.iter().map(|m| m.0).collect();
            let reader = self.reader(&ids)?;
            let start = self.start_states();
            let before = self.hashes_now(&reader, &start)?;
            let mut again = self.hashes_now(&reader, &start)?;
            say(format!(
                "read twice, {} parts differ",
                before.iter().zip(&again).filter(|(a, b)| a != b).count()
            ));
            for i in 0..3 {
                self.run(&Options {
                    loops: 1,
                    ..Options::default()
                })?;
                let ran = {
                    let end: Vec<(u32, Vec<u32>)> =
                        self.end.iter().map(|(k, v)| (*k, v.clone())).collect();
                    let mut s = start.clone();
                    for (id, v) in end {
                        for (sub, st) in v.iter().enumerate() {
                            let sub = if v.len() == 1 { ALL } else { sub as u32 };
                            s.change(&states::Change {
                                res: id,
                                sub,
                                after: *st,
                                promoted: false,
                                alias: false,
                            });
                        }
                    }
                    self.hashes_now(&reader, &s)?
                };
                self.restore(true)?;
                again = self.hashes_now(&reader, &start)?;
                let differ: Vec<usize> = (0..before.len())
                    .filter(|&i| before[i] != again[i])
                    .collect();
                let changed = (0..before.len()).filter(|&i| before[i] != ran[i]).count();
                say(format!(
                    "run {i}: {changed} parts changed by the frame, {} not put back as they were",
                    differ.len()
                ));
                for &d in differ.iter().take(20) {
                    let pieces = before[d]
                        .iter()
                        .zip(&again[d])
                        .filter(|(a, b)| a != b)
                        .count();
                    say(format!(
                        "    {} {:?}: {pieces} of {} pieces",
                        self.called(reader.parts[d].id),
                        reader.parts[d].sub.map(|s| s.0),
                        before[d].len()
                    ));
                }
            }
        }
        let put_back: HashSet<u32> = self.masters.iter().map(|m| m.0).collect();
        let others: Vec<u32> = self
            .objects
            .keys()
            .copied()
            .filter(|id| !put_back.contains(id))
            .collect();
        let reader = self.reader(&others)?;
        self.restore(true)?;
        let start = self.start_states();
        let before = self.hashes_now(&reader, &start)?;
        let o = Options {
            loops: 1,
            ..Options::default()
        };
        self.run(&o)?;
        self.restore(true)?;
        let after = self.hashes_now(&reader, &start)?;
        let mut n = 0;
        for (i, (a, b)) in before.iter().zip(&after).enumerate() {
            let pieces = a.iter().zip(b).filter(|(a, b)| a != b).count();
            if pieces > 0 {
                n += 1;
                let p = &reader.parts[i];
                let r = &self.frame.resources[self.objects[&p.id].index];
                say(format!(
                    "not put back, but changed: {} {:?} idle {} late {}: {pieces} pieces of {}",
                    self.called(p.id),
                    r.place,
                    r.idle,
                    r.late,
                    a.len()
                ));
            }
        }
        say(format!(
            "{} parts of {} resources looked at, {n} changed",
            reader.parts.len(),
            others.len()
        ));
        Ok(())
    }

    /// Runs the unit until two runs differ, and says how.
    pub(super) fn compare(
        &mut self,
        reader: &Reader,
        unit: &Unit,
        states: &States,
        pieces: &Pieces,
        wait: bool,
    ) -> Result<Vec<String>> {
        let after = self.after(unit, states);
        let parts: std::collections::BTreeSet<usize> = pieces.iter().map(|p| p.0).collect();
        let mut seen: Vec<(Vec<Vec<u64>>, Vec<Vec<u8>>)> = Vec::new();
        for run in 0..8 {
            if run > 0 {
                self.put_back_from(&after, states)?;
            }
            self.go(unit, wait)?;
            let hashes = self.hashes_now(reader, &after)?;
            if seen.iter().any(|(h, _)| *h == hashes) {
                continue;
            }
            let data = parts
                .iter()
                .map(|&p| reader.bytes(&reader.parts[p]).to_vec())
                .collect();
            seen.push((hashes, data));
            if seen.len() == 2 {
                break;
            }
        }
        let mut out = Vec::new();
        // The arguments it ran with.
        if let Some(Cmd::Indirect { args, at, .. }) = unit.cmds.get(unit.to - 1)
            && let Some((p, part)) = reader.parts.iter().enumerate().find(|(_, p)| p.id == *args)
        {
            let bytes = &reader.bytes(part)[*at as usize..];
            let words: Vec<u32> = bytes
                .chunks_exact(4)
                .take(8)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap_or_default()))
                .collect();
            let each: HashSet<&Vec<u64>> = seen.iter().map(|s| &s.0[p]).collect();
            out.push(format!(
                "arguments {words:x?} (the same in all runs: {})",
                each.len() == 1
            ));
        }
        self.at = after;
        let [(_, a), (_, b)] = &seen[..] else {
            out.push("it did not differ when looked at again".into());
            return Ok(out);
        };
        // And without the command.
        let before = Unit {
            to: unit.to - 1,
            ..*unit
        };
        self.put_back(states)?;
        self.go(&before, wait)?;
        self.at = self.after(&before, states);
        let now = self.at.clone();
        self.hashes_now(reader, &now)?;
        let without: Vec<Vec<u8>> = parts
            .iter()
            .map(|&p| reader.bytes(&reader.parts[p]).to_vec())
            .collect();
        let to = self.folder.join("diverge");
        let _ = std::fs::create_dir_all(&to);
        for (k, &p) in parts.iter().enumerate().take(12) {
            let part = &reader.parts[p];
            // Both contents, to look at.
            let sub = part.sub.map_or(0, |s| s.0);
            let _ = std::fs::write(to.join(format!("{}-{sub}-0.bin", part.id)), &without[k]);
            let changed = |v: &[u8]| {
                v.chunks_exact(4)
                    .zip(without[k].chunks_exact(4))
                    .filter(|(x, y)| x != y)
                    .count()
            };
            out.push(format!(
                "it changes {} dwords in one run, {} in another",
                changed(&a[k]),
                changed(&b[k])
            ));
            let _ = std::fs::write(to.join(format!("{}-{sub}-a.bin", part.id)), &a[k]);
            let _ = std::fs::write(to.join(format!("{}-{sub}-b.bin", part.id)), &b[k]);
            let words = |v: &[u8]| -> Vec<u32> {
                v.chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap_or_default()))
                    .collect()
            };
            let (x, y) = (words(&a[k]), words(&b[k]));
            let differ: Vec<usize> = (0..x.len().min(y.len()))
                .filter(|&i| x[i] != y[i])
                .collect();
            if differ.is_empty() {
                continue;
            }
            // The same words in another order: an order, not a value.
            let sorted = |v: &[u32]| {
                let mut s: Vec<u32> = differ.iter().map(|&i| v[i]).collect();
                s.sort_unstable();
                s
            };
            let order = if sorted(&x) == sorted(&y) {
                " (the same values in another order)"
            } else {
                ""
            };
            let sub = part.sub.map_or(String::new(), |(s, f)| {
                format!(" subresource {s} pitch {}", f.Footprint.RowPitch)
            });
            let sample: Vec<String> = differ
                .iter()
                .take(8)
                .map(|&i| format!("+{:#x}: {:08x} {:08x}", i * 4, x[i], y[i]))
                .collect();
            out.push(format!(
                "writes {}{sub}: {} of {} dwords differ, {:#x}..{:#x}{order}: {}",
                self.called(part.id),
                differ.len(),
                x.len(),
                differ[0] * 4,
                differ[differ.len() - 1] * 4 + 4,
                sample.join(", ")
            ));
        }
        if parts.len() > 12 {
            out.push(format!("and {} more parts", parts.len() - 12));
        }
        Ok(out)
    }
}
