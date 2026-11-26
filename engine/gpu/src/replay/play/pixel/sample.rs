//! Command-list instrumentation that copies watched texels after commands.

use super::super::edit::{PROBE, SCRATCH};
use super::super::*;
use super::format::{part_of, same_shape, texel, texel_bytes, transition};
use super::{MOST, OWN, Pixels, ROOM, Sample, Spot, Textures};

impl Player {
    pub(super) fn textures(&self, picture: (u32, u32)) -> Textures {
        let textures = || {
            self.frame
                .resources
                .iter()
                .filter(|r| self.texture(r.id).is_some())
        };
        let shaped = textures()
            .filter(|r| {
                let d = &r.desc;
                d.dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D.0
                    && d.flags & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
                    && d.width as u32 * 4 >= picture.0
                    && same_shape(
                        (d.width as f32, d.height as f32),
                        (picture.0 as f32, picture.1 as f32),
                    )
            })
            .map(|r| r.id)
            .collect();
        let copied_as = textures()
            .map(|r| {
                let f = gpu::footprints(&self.device, &gpu::to_desc(&r.desc), 0, 1).0;
                (r.id, f[0].Footprint.Format.0)
            })
            .collect();
        Textures {
            copied_as,
            shaped,
            drawn: HashMap::new(),
        }
    }

    /// The spots of a texture (of its subresource `sub`) that is written not
    /// as a target.
    fn spots_of(
        &self,
        p: &Pixels,
        t: &Textures,
        picture: (u32, u32),
        id: u32,
        sub: u32,
        out: &mut Vec<Spot>,
    ) {
        let Some(r) = self.texture(id) else { return };
        let depth = r.desc.flags & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0;
        for &(res, at, x, y) in &p.texels {
            if res == id && (sub == at || sub == ALL) {
                out.push(Spot {
                    res,
                    sub: at,
                    x,
                    y,
                    z: 0,
                    point: OWN,
                    format: r.desc.format,
                    depth,
                });
            }
        }
        if r.desc.dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D.0 || !(sub == 0 || sub == ALL) {
            return;
        }
        let size = (r.desc.width as u32, r.desc.height);
        let whole = [0.0, 0.0, size.0 as f32, size.1 as f32];
        let view = t.drawn.get(&size).copied().unwrap_or(whole);
        for (point, at) in p.points.iter().enumerate() {
            if let Some((x, y)) = texel(*at, picture, view, size) {
                out.push(Spot {
                    res: id,
                    sub: 0,
                    x,
                    y,
                    z: 0,
                    point,
                    format: r.desc.format,
                    depth,
                });
            }
        }
    }

    /// Whether a texture is the one in use of those that share its memory.
    fn in_use(&self, states: &States, id: u32) -> bool {
        self.objects.get(&id).is_some_and(|o| {
            !self.frame.resources[o.index].idle || states.used.contains_key(&id)
        })
    }

    /// The commands that copy the texels of `spots` to [`PROBE`] from `first`
    /// on; `copied`: a copy to them went before.
    #[allow(clippy::too_many_arguments)]
    fn take(
        &self,
        spots: &[Spot],
        first: u64,
        states: &States,
        t: &Textures,
        copied: bool,
        scratch: &mut u64,
        out: &mut Vec<Cmd>,
    ) {
        let (mut to, mut from, mut copies) = (Vec::new(), Vec::new(), Vec::new());
        let mut moved = HashSet::new();
        for (i, s) in spots.iter().enumerate() {
            let Some(r) = self.texture(s.res) else {
                continue;
            };
            let all = states.get(s.res).unwrap_or(vec![COMMON]);
            let mut state = all
                .get(s.sub as usize)
                .or(all.first())
                .copied()
                .unwrap_or(COMMON);
            if copied && state == COMMON {
                state = COPY_DEST;
            }
            if state & COPY_SOURCE == 0 && moved.insert((s.res, s.sub)) {
                to.push(transition(s.res, s.sub, state, COPY_SOURCE));
                from.push(transition(s.res, s.sub, COPY_SOURCE, state));
            }
            let offset = (first + i as u64) * ROOM;
            if !s.depth {
                copies.push(Cmd::CopyTexture {
                    dst: Loc::Placed {
                        res: PROBE,
                        offset,
                        format: t.copied_as.get(&s.res).copied().unwrap_or(r.desc.format),
                        size: [1, 1, 1],
                        pitch: 256,
                    },
                    at: [0; 3],
                    src: Loc::Sub {
                        res: s.res,
                        sub: s.sub,
                    },
                    part: Some([s.x, s.y, s.z, s.x + 1, s.y + 1, s.z + 1]),
                });
                continue;
            }
            // Depth is only copied whole.
            let mip = s.sub % u32::from(r.desc.mips.max(1));
            let w = (r.desc.width as u32 >> mip).max(1);
            let h = (r.desc.height >> mip).max(1);
            let bytes = texel_bytes(s.format) as u64;
            let pitch = (w * bytes as u32).next_multiple_of(256);
            *scratch = (*scratch).max(u64::from(pitch) * u64::from(h));
            copies.push(Cmd::CopyTexture {
                dst: Loc::Placed {
                    res: SCRATCH,
                    offset: 0,
                    format: t.copied_as.get(&s.res).copied().unwrap_or(r.desc.format),
                    size: [w, h, 1],
                    pitch,
                },
                at: [0; 3],
                src: Loc::Sub {
                    res: s.res,
                    sub: s.sub,
                },
                part: None,
            });
            copies.push(Cmd::Barriers {
                all: vec![transition(SCRATCH, ALL, COPY_DEST, COPY_SOURCE)],
            });
            copies.push(Cmd::CopyBuffer {
                dst: PROBE,
                dst_at: offset,
                src: SCRATCH,
                src_at: u64::from(pitch) * u64::from(s.y) + u64::from(s.x) * bytes,
                len: bytes,
            });
            copies.push(Cmd::Barriers {
                all: vec![transition(SCRATCH, ALL, COPY_SOURCE, COPY_DEST)],
            });
        }
        if !to.is_empty() {
            out.push(Cmd::Barriers { all: to });
        }
        out.extend(copies);
        if !from.is_empty() {
            out.push(Cmd::Barriers { all: from });
        }
    }

    /// The frame's lists with copies of texels after commands.
    pub(super) fn sampled(
        &mut self,
        p: &Pixels,
        submits: &[Vec<Vec<Cmd>>],
        picture: (u32, u32),
        counts: &HashMap<(u32, u32), u32>,
        samples: &mut Vec<Sample>,
    ) -> Result<Vec<Vec<Vec<Cmd>>>> {
        let strides: HashMap<u32, u32> = self
            .frame
            .signatures
            .iter()
            .map(|s| (s.id, s.stride))
            .collect();
        let dispatching: HashSet<u32> = self
            .frame
            .signatures
            .iter()
            .filter(|s| {
                s.args
                    .iter()
                    .any(|a| a[0] == D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH.0 as u32)
            })
            .map(|s| s.id)
            .collect();
        let mut t = self.textures(picture);
        let mut states = self.start_states();
        let mut known: HashSet<Spot> = HashSet::new();
        let mut made = Vec::new();
        let mut here = 0;
        let mut scratch = 0u64;
        for lists in submits {
            let mut out_lists = Vec::new();
            for cmds in lists {
                let wanted = p.lists.is_none_or(|(a, b)| (a..=b).contains(&here));
                let mut out = Vec::with_capacity(cmds.len());
                let mut pso = NONE;
                let mut targets: Vec<Slot> = Vec::new();
                let mut view: Option<[f32; 4]> = None;
                for (n, c) in cmds.iter().enumerate() {
                    match c {
                        Cmd::Pipeline { pso: id } => pso = *id,
                        Cmd::Targets { colors, depth } => {
                            targets = colors.iter().chain(depth).copied().collect();
                        }
                        Cmd::Viewports { all } => {
                            view = all.first().map(|v| [v[0], v[1], v[2], v[3]]);
                        }
                        _ => {}
                    }
                    let dispatches = matches!(c, Cmd::Dispatch { .. })
                        || matches!(c, Cmd::Indirect { signature, .. } if dispatching.contains(signature));
                    let draws = c.runs_shaders() && !dispatches;
                    let mut spots: Vec<Spot> = Vec::new();
                    let looked = wanted && c.works() && samples.len() < MOST;
                    for s in targets.iter().filter(|_| looked && draws) {
                        let Some((sub, z, size, format)) = self.seen_by(s) else {
                            continue;
                        };
                        let depth = s.kind == slot::DSV;
                        let spot = |x, y, point| Spot {
                            res: s.res,
                            sub,
                            x,
                            y,
                            z,
                            point,
                            format,
                            depth,
                        };
                        let view = view.unwrap_or([0.0, 0.0, size.0 as f32, size.1 as f32]);
                        for (point, at) in p.points.iter().enumerate() {
                            if let Some((x, y)) = texel(*at, picture, view, size) {
                                t.drawn.insert(size, view);
                                spots.push(spot(x, y, point));
                            }
                        }
                        for &(res, at, x, y) in &p.texels {
                            if (res, at) == (s.res, sub) {
                                spots.push(spot(x, y, OWN));
                            }
                        }
                    }
                    if looked && c.runs_shaders() {
                        let all = p.texels.iter().map(|t| t.0);
                        let some = p.written && (dispatches || !targets.is_empty());
                        let all: HashSet<u32> =
                            all.chain(t.shaped.iter().copied().filter(|_| some)).collect();
                        for id in all {
                            let writable = states
                                .get(id)
                                .is_some_and(|s| s.iter().any(|s| s & UNORDERED_ACCESS != 0));
                            if writable && self.in_use(&states, id) {
                                self.spots_of(p, &t, picture, id, ALL, &mut spots);
                            }
                        }
                    }
                    let mut copied = false;
                    if looked {
                        let mut to = None;
                        match c {
                            Cmd::CopyTexture {
                                dst: Loc::Sub { res, sub },
                                ..
                            } => to = Some((*res, *sub, true)),
                            Cmd::CopyResource { dst, .. } => to = Some((*dst, ALL, true)),
                            Cmd::Resolve { dst, dst_sub, .. }
                            | Cmd::ResolveRegion { dst, dst_sub, .. } => {
                                to = Some((*dst, *dst_sub, false));
                            }
                            Cmd::ClearTarget { view, .. } | Cmd::ClearDepth { view, .. } => {
                                to = self.seen_by(view).map(|s| (view.res, s.0, false));
                            }
                            _ => {}
                        }
                        if let Some((res, sub, is_copy)) = to {
                            copied = is_copy;
                            self.spots_of(p, &t, picture, res, sub, &mut spots);
                        }
                    }
                    let mut seen = HashSet::new();
                    spots.retain(|s| {
                        (p.depth || !s.depth) && texel_bytes(s.format) > 0 && seen.insert(*s)
                    });
                    let what = match c {
                        Cmd::Draw { .. } => "Draw",
                        Cmd::DrawIndexed { .. } => "DrawIndexed",
                        Cmd::DispatchMesh { .. } => "DispatchMesh",
                        Cmd::Dispatch { .. } => "Dispatch",
                        Cmd::Indirect { .. } if dispatches => "indirect Dispatch",
                        Cmd::Indirect { .. } => "indirect draw",
                        Cmd::ClearTarget { .. } | Cmd::ClearDepth { .. } => "clear",
                        Cmd::Resolve { .. } | Cmd::ResolveRegion { .. } => "resolve",
                        _ => "copy",
                    };
                    let runs = if c.runs_shaders() { pso } else { NONE };
                    let mut take = |before: bool, part: u32, spots: &[Spot], out: &mut Vec<Cmd>| {
                        let first = samples.len() as u64;
                        self.take(spots, first, &states, &t, copied && !before, &mut scratch, out);
                        samples.extend(spots.iter().map(|spot| Sample {
                            list: here,
                            command: n,
                            part,
                            before,
                            spot: *spot,
                            pso: runs,
                            what,
                        }));
                    };
                    // What was not seen before this command, as it is now.
                    let new: Vec<Spot> =
                        spots.iter().filter(|s| known.insert(**s)).copied().collect();
                    if !new.is_empty() {
                        take(true, 0, &new, &mut out);
                    }
                    match (counts.get(&(here, n as u32)), c) {
                        (Some(&count), Cmd::Indirect { signature, .. }) => {
                            let stride = strides.get(signature).copied().unwrap_or(0);
                            for i in 0..count {
                                out.push(part_of(c, i, stride));
                                take(false, i, &spots, &mut out);
                            }
                        }
                        _ => {
                            out.push(c.clone());
                            if !spots.is_empty() {
                                take(false, 0, &spots, &mut out);
                            }
                        }
                    }
                    let mut all = Vec::new();
                    states::changes(c, &mut all);
                    all.iter().for_each(|c| states.change(c));
                }
                out_lists.push(out);
                here += 1;
            }
            states.decay();
            made.push(out_lists);
        }
        if scratch > 0 {
            let buffer = gpu::buffer(
                &self.device,
                D3D12_HEAP_TYPE_DEFAULT,
                scratch,
                D3D12_RESOURCE_FLAG_NONE,
            )?;
            self.extra.insert(SCRATCH, buffer.0);
        }
        Ok(made)
    }
}
