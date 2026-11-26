//! Command description, buffer/view inspection, and descriptor-slot reporting.

use super::super::*;

impl Player {
    /// A command in words: its pipeline, what its list bound for it, and the
    /// barriers since the last command that worked.
    pub(super) fn describe(&self, cmds: &[Cmd], at: usize) -> String {
        let mut pso = NONE;
        let mut binds: BTreeMap<(bool, u32), String> = BTreeMap::new();
        let mut since: Vec<String> = Vec::new();
        let mut targets = String::new();
        for c in &cmds[..at] {
            match c {
                Cmd::Pipeline { pso: p } => pso = *p,
                Cmd::RootSignature { graphics, .. } => binds.retain(|k, _| k.0 != *graphics),
                Cmd::RootView {
                    graphics,
                    kind,
                    param,
                    va,
                } => {
                    let kind = ["", "cbv", "srv", "uav"]
                        .get(*kind as usize)
                        .copied()
                        .unwrap_or("");
                    let text = match va {
                        Va::At { res, offset } => {
                            format!("{kind} {} +{offset:#x}", self.called(*res))
                        }
                        _ => format!("{kind} null"),
                    };
                    binds.insert((*graphics, *param), text);
                }
                Cmd::Table {
                    graphics,
                    param,
                    heap,
                    slot,
                } => {
                    binds.insert(
                        (*graphics, *param),
                        format!("table heap {heap} slot {slot}"),
                    );
                }
                Cmd::Constants {
                    graphics,
                    param,
                    offset,
                    values,
                } => {
                    let text = format!("constants +{offset} {values:x?}");
                    binds
                        .entry((*graphics, *param))
                        .and_modify(|t| *t = format!("{t}, {text}"))
                        .or_insert(text);
                }
                Cmd::Targets { colors, depth } => {
                    let all: Vec<String> = colors
                        .iter()
                        .chain(depth)
                        .map(|s| self.called(s.res))
                        .collect();
                    targets = all.join(", ");
                }
                Cmd::Barriers { all } => since.extend(all.iter().map(|b| match b {
                    Barrier::Transition {
                        res, before, after, ..
                    } => format!("{} {before:#x}->{after:#x}", self.called(*res)),
                    Barrier::Uav { res: 0, .. } => "all uav".into(),
                    Barrier::Uav { res, .. } => format!("uav {}", self.called(*res)),
                    Barrier::Alias { .. } => "alias".into(),
                })),
                _ => {}
            }
            if c.works() {
                since.clear();
            }
        }
        let graphics = !matches!(cmds[at], Cmd::Dispatch { .. });
        let binds: Vec<String> = binds
            .iter()
            .filter(|(k, _)| k.0 == graphics)
            .map(|(k, t)| format!("[{}] {t}", k.1))
            .collect();
        let mut text = format!("{:?}", cmds[at]);
        if cmds[at].runs_shaders() {
            text += &format!(" of {}", self.pipeline_name(pso));
            text += &format!("\n    bound: {}", binds.join("; "));
            if graphics && !targets.is_empty() {
                text += &format!("\n    targets: {targets}");
            }
        }
        text + &format!(
            "\n    barriers since the command before: {}",
            since.join("; ")
        )
    }

    /// `len` bytes of a buffer from `at`.
    pub(super) fn slice(&mut self, id: u32, at: u64, len: u64) -> Result<Vec<u8>> {
        let Some(o) = self.objects.get(&id) else {
            return Ok(Vec::new());
        };
        let len = len.min(self.frame.resources[o.index].desc.width.saturating_sub(at));
        if len == 0 {
            return Ok(Vec::new());
        }
        let res = o.res.clone();
        let (back, data) = gpu::buffer(
            &self.device,
            D3D12_HEAP_TYPE_READBACK,
            len,
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        self.worker.run(|list| {
            // SAFETY: an open list; live buffers.
            unsafe { list.CopyBufferRegion(&back, 0, &res, at, len) };
        })?;
        // SAFETY: the mapping; the copy into it has run.
        Ok(unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec())
    }

    /// The words of a buffer view (raw or typed, of words).
    fn view_words(&mut self, s: &Slot, first: u64, n: u64) -> Result<Vec<u32>> {
        let at = u64::from_le_bytes([
            s.desc[16], s.desc[17], s.desc[18], s.desc[19], s.desc[20], s.desc[21], s.desc[22],
            s.desc[23],
        ]);
        let count = u64::from(u32::from_le_bytes([
            s.desc[24], s.desc[25], s.desc[26], s.desc[27],
        ]));
        let n = n.min(count.saturating_sub(first));
        let bytes = self.slice(s.res, (at + first) * 4, n * 4)?;
        Ok(bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap_or_default()))
            .collect())
    }

    /// What a mesh draw of the title's (with its usual constants) reads.
    pub(super) fn peek(
        &mut self,
        cmds: &[Cmd],
        at: usize,
        submit: usize,
        say: &mut dyn FnMut(String),
    ) -> Result<()> {
        let mut v = [0u32; 16];
        let mut heap = NONE;
        for c in &cmds[..at] {
            match c {
                Cmd::Constants {
                    graphics: true,
                    param: 0,
                    offset,
                    values,
                } => {
                    for (k, x) in values.iter().enumerate() {
                        if let Some(at) = v.get_mut(*offset as usize + k) {
                            *at = *x;
                        }
                    }
                }
                Cmd::Heaps { heaps } => heap = heaps.first().copied().unwrap_or(NONE),
                _ => {}
            }
        }
        say(format!("    constants {v:x?}"));
        let slot = |p: &Player, byte: u32| p.slot_at(heap, byte >> 5, submit);
        for (name, byte, n) in [
            ("y+0", v[9], 64u64),
            ("y+32 (matrices)", v[9] + 32, 64),
            ("y+64", v[9] + 64, 32),
            ("y+96 (instances)", v[9] + 96, 64),
            ("y+128 (meshlets)", v[9] + 128, 64),
            ("y+192 (visible)", v[9] + 192, 64),
            ("x+1600", v[8] + 1600, 0),
            ("x+2752", v[8] + 2752, 0),
            ("x+2848", v[8] + 2848, 0),
        ] {
            let s = slot(self, byte);
            let words = if s.kind == slot::SRV {
                self.view_words(&s, 0, n)?
            } else {
                Vec::new()
            };
            say(format!(
                "    {name}: slot {} {}: {words:x?}",
                byte >> 5,
                self.slot_text(&s)
            ));
        }
        // The list of visible meshlets, as the mesh shader finds it.
        let lists = slot(self, v[8] + 2752);
        let word = self.view_words(&lists, u64::from((v[13] & 0x07ff_ffff) & 0x07ff_ffff), 4)?;
        let typed = slot(self, v[8] + 1600);
        let index = self.view_words(&typed, 510, 1)?;
        say(format!(
            "    list words {word:x?} (count {}, first {}), typed view slot {index:x?}",
            word.first().map_or(0, |w| w >> 15),
            word.first().map_or(0, |w| w & 0x7fff)
        ));
        if let (Some(w), Some(i)) = (word.first(), index.first()) {
            let s = self.slot_at(heap, i & 0x07ff_ffff, submit);
            say(format!("    typed view: {}", self.slot_text(&s)));
            if s.kind == slot::SRV && s.res != NONE {
                let list =
                    self.view_words(&s, u64::from(w & 0x7fff) * 16, u64::from(w >> 15) + 16)?;
                say(format!("    visible meshlets: {list:x?}"));
            }
        }
        Ok(())
    }

    /// What the heap slots hold that a draw's constants name, as the title's
    /// shaders find them: byte offsets in the view heap among its first
    /// constants, a descriptor of 32 bytes each.
    pub fn slots(&self, list: u32, command: usize, say: &mut dyn FnMut(String)) -> Result<()> {
        let mut here = 0;
        for (i, s) in self.frame.submits.iter().enumerate() {
            for &l in s.lists.iter().filter(|&&l| l != MISSING) {
                if here == list {
                    let cmds = &self.commands[l as usize];
                    say(self.describe(cmds, command));
                    let mut values = [[0u32; 16]; 3];
                    let mut heap = NONE;
                    for c in &cmds[..command] {
                        match c {
                            Cmd::Constants {
                                param,
                                offset,
                                values: v,
                                ..
                            } => {
                                for (k, x) in v.iter().enumerate() {
                                    if let Some(all) = values.get_mut(*param as usize)
                                        && let Some(at) = all.get_mut(*offset as usize + k)
                                    {
                                        *at = *x;
                                    }
                                }
                            }
                            Cmd::Heaps { heaps } => heap = heaps.first().copied().unwrap_or(NONE),
                            _ => {}
                        }
                    }
                    say(format!("heap {heap}, constants {values:x?}"));
                    let mut bases: Vec<u32> = values
                        .iter()
                        .flatten()
                        .map(|v| v >> 5)
                        .filter(|v| *v > 64)
                        .collect();
                    bases.dedup();
                    for base in bases {
                        for k in 0..12 {
                            let s = self.slot_at(heap, base + k, i);
                            if s.kind != slot::EMPTY {
                                say(format!("  slot {base}+{k}: {}", self.slot_text(&s)));
                            } else {
                                say(format!("  slot {base}+{k}: never written"));
                            }
                        }
                    }
                }
                here += 1;
            }
        }
        Ok(())
    }

    /// A heap slot as it was for a submission.
    fn slot_at(&self, heap: u32, at: u32, submit: usize) -> Slot {
        let late = self.frame.submits[..=submit].iter().rev().find_map(|s| {
            s.views
                .iter()
                .rev()
                .find(|(h, i, _)| *h == heap && *i == at)
                .map(|v| v.2)
        });
        late.unwrap_or_else(|| {
            let runs = self
                .frame
                .descriptor_heaps
                .iter()
                .find(|h| h.id == heap)
                .map(|h| &h.runs);
            runs.and_then(|runs| {
                let k = runs.partition_point(|r| r.0 <= at).checked_sub(1)?;
                let (first, n, s) = &runs[k];
                (at - first < *n).then_some(*s)
            })
            .unwrap_or(Slot::EMPTY)
        })
    }

    fn slot_text(&self, s: &Slot) -> String {
        let word = |i: usize| {
            u32::from_le_bytes([
                s.desc[i * 4],
                s.desc[i * 4 + 1],
                s.desc[i * 4 + 2],
                s.desc[i * 4 + 3],
            ])
        };
        let kind = ["empty", "cbv", "srv", "uav", "sampler", "rtv", "dsv"]
            .get(s.kind as usize)
            .copied()
            .unwrap_or("?");
        let res = if s.res == NONE {
            "null".into()
        } else {
            self.called(s.res)
        };
        match s.kind {
            slot::CBV => format!("cbv {res} +{:#x} size {:#x}", s.address(), word(2)),
            _ => format!(
                "{kind} format {} dimension {} first {} count {} stride {} flags {}: {res}",
                word(0),
                word(1),
                word(if s.kind == slot::SRV { 4 } else { 2 }),
                word(if s.kind == slot::SRV { 6 } else { 4 }),
                word(if s.kind == slot::SRV { 7 } else { 5 }),
                word(if s.kind == slot::SRV { 8 } else { 8 }),
            ),
        }
    }

    pub(in super::super) fn called(&self, id: u32) -> String {
        let Some(o) = self.objects.get(&id) else {
            return format!("resource {id} (not there)");
        };
        let r = &self.frame.resources[o.index];
        let d = &r.desc;
        if d.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0 {
            let kind = match &r.place {
                Place::Committed { props, .. } => format!("{:?}", gpu::seen(props)),
                _ => "placed".into(),
            };
            format!("#{id} '{}' buffer {:#x} {kind}", r.name, d.width)
        } else {
            format!(
                "#{id} '{}' {}x{}x{} m{} f{}",
                r.name, d.width, d.height, d.depth, d.mips, d.format
            )
        }
    }
}
