//! What draws a pixel: `--pixel`. After every command that can change a
//! texture, the texel under the pixel is copied out of it (of a draw: of its
//! targets, where the pixel lies in its viewport, and of the textures shaders
//! may write; of a dispatch: of those; of a copy, clear or resolve: of where
//! it goes), and commands that changed one are named with the values before
//! and after.

use super::edit::PROBE;
use super::*;

mod format;
mod sample;

use format::{texel_bytes, texel_text, transition};

/// What to look at.
#[derive(Clone, Debug, Default)]
pub struct Pixels {
    /// Points of the presented picture.
    pub points: Vec<(u32, u32)>,
    /// Texels of textures: resource, subresource, place.
    pub texels: Vec<(u32, u32, u32, u32)>,
    /// Only these lists are looked into.
    pub lists: Option<(u32, u32)>,
    /// Depth targets too (a copy of the whole target for each draw).
    pub depth: bool,
    /// Textures shaders may write too.
    pub written: bool,
    /// Indirect draws that are run one by one: list, command.
    pub split: Vec<(u32, u32)>,
}

/// Bytes a sample has.
const ROOM: u64 = 512;
/// The most samples of one run.
const MOST: usize = 1 << 20;

#[derive(Clone, Debug)]
struct Sample {
    list: u32,
    command: usize,
    /// Of a split draw: which one.
    part: u32,
    before: bool,
    spot: Spot,
    pso: u32,
    what: &'static str,
}

/// A texture's part that a sample can be taken from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Spot {
    res: u32,
    sub: u32,
    x: u32,
    y: u32,
    z: u32,
    /// The point of the picture it stands for (or [`OWN`]).
    point: usize,
    format: i32,
    depth: bool,
}

/// In [`Spot::point`]: a texel asked for by itself.
const OWN: usize = usize::MAX;

/// What is known of the frame's textures.
struct Textures {
    /// Formats copies have.
    copied_as: HashMap<u32, i32>,
    /// Textures shaders may write and that have the picture's shape.
    shaped: Vec<u32>,
    /// The viewport targets of a size were last drawn through.
    drawn: HashMap<(u32, u32), [f32; 4]>,
}

impl Player {
    fn texture(&self, id: u32) -> Option<&wire::Resource> {
        let o = self.objects.get(&id)?;
        let r = &self.frame.resources[o.index];
        (r.desc.dimension != D3D12_RESOURCE_DIMENSION_BUFFER.0 && r.desc.samples[0] <= 1)
            .then_some(r)
    }

    /// What a view of a target is of: subresource, depth slice, size, format.
    fn seen_by(&self, s: &Slot) -> Option<(u32, u32, (u32, u32), i32)> {
        let r = self.texture(s.res)?;
        let word = |i: usize| {
            u32::from_le_bytes([
                s.desc[i * 4],
                s.desc[i * 4 + 1],
                s.desc[i * 4 + 2],
                s.desc[i * 4 + 3],
            ])
        };
        let (mut mip, mut slice, mut z, mut format) = (0, 0, 0, 0);
        if s.described != 0 {
            format = word(0) as i32;
            match (s.kind, word(1) as i32) {
                // 2D, 2D array and 3D views.
                (slot::RTV, 4) => mip = word(2),
                (slot::RTV, 5) => (mip, slice) = (word(2), word(3)),
                (slot::RTV, 8) => (mip, z) = (word(2), word(3)),
                (slot::DSV, 3) => mip = word(3),
                (slot::DSV, 4) => (mip, slice) = (word(3), word(4)),
                _ => return None,
            }
        }
        if format == 0 {
            format = r.desc.format;
        }
        let mips = u32::from(r.desc.mips.max(1));
        let mip = mip.min(mips - 1);
        let size = (
            (r.desc.width as u32 >> mip).max(1),
            (r.desc.height >> mip).max(1),
        );
        Some((mip + slice * mips, z, size, format))
    }

    /// Width and height of the presented picture.
    pub fn picture_size(&self) -> Option<(u32, u32)> {
        self.frame.presents.last().map(|p| (p.width, p.height))
    }

    /// Says which commands change the texels under points of the picture.
    pub fn pixel(&mut self, o: &Options, p: &Pixels, say: &mut dyn FnMut(String)) -> Result<()> {
        let picture = self.picture_size().context("no picture was presented")?;
        self.set_merging(o.merge)?;
        let mut report = Report::default();
        let submits = self.edited(o, &mut report, &[]);
        let counts = self.counts_of(o, p, &submits)?;
        let mut samples: Vec<Sample> = Vec::new();
        let made = self.sampled(p, &submits, picture, &counts, &mut samples)?;
        let bytes = self.probed(o, &made, samples.len() as u64 * ROOM)?;
        say(format!(
            "{} texels taken; picture {}x{}; points {:?}",
            samples.len(),
            picture.0,
            picture.1,
            p.points
        ));
        report.notes.drain(..).for_each(&mut *say);
        let mut last: HashMap<(u32, u32, (u32, u32), usize), Vec<u8>> = HashMap::new();
        let mut said = None;
        for (i, s) in samples.iter().enumerate() {
            let t = &s.spot;
            let n = texel_bytes(t.format).max(1);
            let now = bytes[i * ROOM as usize..][..n].to_vec();
            let was = last.insert((t.res, t.sub, (t.x, t.y), t.point), now.clone());
            if was.as_ref() == Some(&now) {
                continue;
            }
            let point = match t.point {
                OWN => String::new(),
                n => format!("point {n} "),
            };
            let spot = format!(
                "    {point}{} sub {} at {},{}:",
                self.called(t.res),
                t.sub,
                t.x,
                t.y
            );
            let now = texel_text(t.format, &now);
            if s.before {
                // Seen for the first time, or changed by what is not looked
                // into.
                if was.is_some() {
                    say(format!("{spot} changed unseen to {now}"));
                }
                continue;
            }
            if said != Some((s.list, s.command, s.part)) {
                said = Some((s.list, s.command, s.part));
                let part = match p.split.contains(&(s.list, s.command as u32)) {
                    true => format!(" draw {}", s.part),
                    false => String::new(),
                };
                let of = match s.pso {
                    NONE => String::new(),
                    pso => format!(" of {pso} {}", self.pipeline_name(pso)),
                };
                say(format!("{}:{}{part} {}{of}", s.list, s.command, s.what));
            }
            let was = was.map_or("?".to_owned(), |w| texel_text(t.format, &w));
            say(format!("{spot} {was} -> {now}"));
        }
        Ok(())
    }

    /// Runs the frame with commands that copy to [`PROBE`], which is of
    /// `len` bytes; its bytes after that.
    fn probed(&mut self, o: &Options, submits: &[Vec<Vec<Cmd>>], len: u64) -> Result<Vec<u8>> {
        let (back, data) = gpu::buffer(
            &self.device,
            D3D12_HEAP_TYPE_READBACK,
            len.max(4),
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        self.extra.insert(PROBE, back);
        self.restore(o.restore)?;
        let built = self.build(submits, o)?;
        self.fresh = false;
        self.send(&built, o)?;
        // SAFETY: the mapping; the copies into it have run.
        let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
        self.extra.remove(&PROBE);
        Ok(bytes)
    }

    /// How many draws the calls to be split have.
    fn counts_of(
        &mut self,
        o: &Options,
        p: &Pixels,
        submits: &[Vec<Vec<Cmd>>],
    ) -> Result<HashMap<(u32, u32), u32>> {
        let mut counts = HashMap::new();
        if p.split.is_empty() {
            return Ok(counts);
        }
        const ARGS: u32 = 0x200;
        let mut made = submits.to_vec();
        let mut asked: Vec<(u32, u32)> = Vec::new();
        for (here, cmds) in made.iter_mut().flatten().enumerate() {
            let mut out = Vec::new();
            for (n, c) in std::mem::take(cmds).into_iter().enumerate() {
                let key = (here as u32, n as u32);
                if let Cmd::Indirect {
                    most,
                    count,
                    count_at,
                    ..
                } = c
                    && p.split.contains(&key)
                {
                    counts.insert(key, most);
                    if count != NONE {
                        out.push(Cmd::Barriers {
                            all: vec![transition(count, ALL, ARGS, COPY_SOURCE)],
                        });
                        out.push(Cmd::CopyBuffer {
                            dst: PROBE,
                            dst_at: asked.len() as u64 * 4,
                            src: count,
                            src_at: count_at,
                            len: 4,
                        });
                        out.push(Cmd::Barriers {
                            all: vec![transition(count, ALL, COPY_SOURCE, ARGS)],
                        });
                        asked.push(key);
                    }
                }
                out.push(c);
            }
            *cmds = out;
        }
        let bytes = self.probed(o, &made, asked.len() as u64 * 4)?;
        for (key, n) in asked.iter().zip(bytes.chunks_exact(4)) {
            if let Some(most) = counts.get_mut(key) {
                *most = (*most).min(u32::from_le_bytes([n[0], n[1], n[2], n[3]]));
            }
        }
        Ok(counts)
    }
}
