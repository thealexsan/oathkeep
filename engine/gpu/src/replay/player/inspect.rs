//! Capture discovery, shader dumping, and offline command listing.

use super::*;

/// The folder of a capture: `folder`, or the last `capture-<n>` in it that
/// is complete.
pub fn find(folder: &Path) -> Result<PathBuf> {
    if folder.join("frame.bin").exists() {
        return Ok(folder.to_owned());
    }
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(folder)
        .with_context(|| format!("{}", folder.display()))?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(n) = name.strip_prefix("capture-").and_then(|n| n.parse().ok()) else {
            continue;
        };
        if entry.path().join("frame.bin").exists() && best.as_ref().is_none_or(|b| b.0 < n) {
            best = Some((n, entry.path()));
        }
    }
    best.map(|b| b.1)
        .ok_or_else(|| anyhow!("no capture (frame.bin) in {}", folder.display()))
}

/// The stage and hash by which the runtime names each of the title's shaders
/// in a capture.
pub fn title_shaders(folder: &Path) -> Result<std::collections::BTreeSet<(String, String)>> {
    let frame = Frame::read(&find(folder)?.join("frame.bin"))?;
    let mut all = std::collections::BTreeSet::new();
    for p in &frame.pipelines {
        for part in &p.parts {
            if let wire::Part::Code { blob, .. } = part
                && let Some(hash) = title_hash(p, *blob)
            {
                let at = p.name.find(hash).unwrap_or(0);
                let stage = p.name[..at].trim_end().rsplit(['[', ' ']).next().unwrap_or("");
                all.insert((stage.to_owned(), hash.to_owned()));
            }
        }
    }
    Ok(all)
}

/// The hash by which the runtime names the title's shader that code blob
/// `blob` of pipeline `p` is the translation of.
pub(super) fn title_hash(p: &wire::Pipeline, blob: u32) -> Option<&str> {
    let kind = p.parts.iter().find_map(|part| match part {
        wire::Part::Code { kind, blob: b } if *b == blob => Some(*kind),
        _ => None,
    })?;
    let stage = match D3D12_PIPELINE_STATE_SUBOBJECT_TYPE(kind) {
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS => "vertex ",
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS => "pixel ",
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS => "mesh ",
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS => "compute ",
        _ => return None,
    };
    let at = p.name.find(stage)? + stage.len();
    p.name.get(at..at + 16)
}

/// Writes the DXIL of the pipelines whose name has `text` in it to `shaders`
/// in the capture, and the pipelines' descriptions; what was written.
pub fn dump(folder: &Path, text: &str) -> Result<Vec<String>> {
    let folder = find(folder)?;
    let frame = Frame::read(&folder.join("frame.bin"))?;
    let file = File::open(folder.join("shaders.bin"))?;
    let to = folder.join("shaders");
    std::fs::create_dir_all(&to)?;
    let mut out = Vec::new();
    for p in frame.pipelines.iter().filter(|p| p.name.contains(text)) {
        let mut line = format!("pipeline {} {}:", p.id, p.name);
        for part in &p.parts {
            match part {
                wire::Part::Code { kind, blob } => {
                    let c = &frame.codes[*blob as usize];
                    let mut bytes = vec![0; c.len as usize];
                    read_at(&file, &mut bytes, c.at)?;
                    let name: String = c.hash[..8].iter().map(|b| format!("{b:02x}")).collect();
                    std::fs::write(to.join(format!("{name}.dxil")), bytes)?;
                    let _ = write!(line, " {kind}={name}.dxil");
                }
                wire::Part::Root { root } => {
                    if let Some((_, blob)) = frame.roots.iter().find(|r| r.0 == *root) {
                        std::fs::write(to.join(format!("root-{root}.bin")), &blob.0)?;
                    }
                    let _ = write!(line, " root-{root}.bin");
                }
                other => {
                    let _ = write!(line, " {other:?}");
                }
            }
        }
        out.push(line);
    }
    Ok(out)
}

/// Writes, one line each, the commands of lists (by their place in the
/// frame), and before them the frame's buffers: `--listing`.
pub fn listing(folder: &Path, lists: (u32, u32), line: &mut dyn FnMut(&str)) -> Result<()> {
    let frame = Frame::read(&find(folder)?.join("frame.bin"))?;
    let pipelines: HashMap<u32, &str> =
        frame.pipelines.iter().map(|p| (p.id, p.name.as_str())).collect();
    for r in &frame.resources {
        if r.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0 {
            let place = match &r.place {
                Place::Committed { props, .. } => format!("{:?}", gpu::seen(props)),
                other => format!("{other:?}"),
            };
            line(&format!("buffer {} {} bytes, {place}: {}", r.id, r.desc.width, r.name));
        } else {
            let d = &r.desc;
            line(&format!(
                "texture {} {}x{}x{} mips {} f{}: {}",
                r.id, d.width, d.height, d.depth, d.mips, d.format, r.name
            ));
        }
    }
    let mut place = 0;
    for (i, s) in frame.submits.iter().enumerate() {
        for &l in s.lists.iter().filter(|&&l| l != MISSING) {
            let here = place;
            place += 1;
            if here < lists.0 || here > lists.1 {
                continue;
            }
            line(&format!("list {here} of submission {i}"));
            for (n, c) in Cmd::all(&frame.lists[l as usize].commands.0)?.iter().enumerate() {
                match c {
                    Cmd::Pipeline { pso } => {
                        line(&format!(
                            "{n} Pipeline {pso} {}",
                            pipelines.get(pso).copied().unwrap_or_default()
                        ));
                    }
                    c => line(&format!("{n} {c:?}")),
                }
            }
        }
    }
    Ok(())
}
