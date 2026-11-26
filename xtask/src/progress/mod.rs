mod render;
mod scan;
mod treemap;

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const README_START: &str = "<!-- oathkeep:progress:start -->";
const README_END: &str = "<!-- oathkeep:progress:end -->";

#[derive(Deserialize)]
pub(crate) struct ProgressData {
    pub(crate) meta: Meta,
    pub(crate) milestones: Vec<Milestone>,
    pub(crate) runs: Vec<RunEntry>,
}

#[derive(Deserialize)]
pub(crate) struct Meta {
    pub(crate) updated: String,
    pub(crate) commit: String,
    pub(crate) branch: String,
    pub(crate) current_fps: String,
    pub(crate) headline: String,
    pub(crate) pages_url: String,
}

#[derive(Deserialize)]
pub(crate) struct Milestone {
    pub(crate) label: String,
    pub(crate) state: String,
    pub(crate) detail: String,
}

#[derive(Deserialize)]
pub(crate) struct RunEntry {
    pub(crate) label: String,
    pub(crate) commit: String,
    pub(crate) fps: f32,
    pub(crate) frame_ms: f32,
    pub(crate) status: String,
}

pub fn run(root: &Path, check: bool) -> Result<()> {
    let source_path = root.join("docs/progress.toml");
    let raw = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let data: ProgressData = toml::from_str(&raw).context("parsing docs/progress.toml")?;
    let metrics = scan::scan_codebase(root).context("scanning codebase coverage")?;

    let svg = render::render_svg(&data, &metrics);
    let html = render::render_html(&data, &metrics);
    let json = render::render_json(&data, &metrics);
    let readme_path = root.join("README.md");
    let readme_old = fs::read_to_string(&readme_path).context("reading README.md")?;
    let readme_new = update_readme(&readme_old, &data, &metrics)?;

    let outputs = [
        (root.join("docs/progress.svg"), svg),
        (root.join("docs/index.html"), html),
        (root.join("docs/progress.json"), json),
        (readme_path, readme_new),
    ];

    if check {
        let mut stale = Vec::new();
        for (path, expected) in &outputs {
            let actual = fs::read_to_string(path).unwrap_or_default();
            if normalize_lf(&actual) != normalize_lf(expected) {
                stale.push(path.display().to_string());
            }
        }
        if !stale.is_empty() {
            bail!(
                "progress artifacts out of date ({}). Run `cargo xtask progress` and stage the updated files.",
                stale.join(", ")
            );
        }
        println!("progress artifacts: up to date");
        return Ok(());
    }

    for (path, content) in &outputs {
        fs::write(path, normalize_lf(content))
            .with_context(|| format!("writing {}", path.display()))?;
    }

    let (lib_imp, lib_tot) = metrics.libraries_totals();
    let (dec_imp, dec_tot) = metrics.isa_decoded_totals();
    let (low_imp, low_tot) = metrics.isa_lowered_totals();
    println!(
        "progress updated: docs/progress.svg, docs/index.html, docs/progress.json, README.md (libraries {:.1}% [{lib_imp}/{lib_tot}], RDNA2 decoded {:.1}% [{dec_imp}/{dec_tot}], DXIL lowered {:.1}% [{low_imp}/{low_tot}])",
        render::pct(lib_imp, lib_tot),
        render::pct(dec_imp, dec_tot),
        render::pct(low_imp, low_tot),
    );
    Ok(())
}

fn normalize_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn update_readme(
    readme: &str,
    data: &ProgressData,
    metrics: &scan::CodeMetrics,
) -> Result<String> {
    let (lib_imp, lib_tot) = metrics.libraries_totals();
    let (dec_imp, dec_tot) = metrics.isa_decoded_totals();
    let (low_imp, low_tot) = metrics.isa_lowered_totals();
    let latest_fps = data
        .runs
        .last()
        .map(|r| format!("{:.1} fps ({:.1} ms/f)", r.fps, r.frame_ms))
        .unwrap_or_else(|| data.meta.current_fps.clone());

    let mut block = String::new();
    let _ = writeln!(block, "{README_START}");
    let _ = writeln!(block, "## Compatibility & Code Coverage Progress");
    let _ = writeln!(block);
    let _ = writeln!(
        block,
        "Live dashboard: [thealexsan.github.io/oathkeep]({}) · Libraries: **{:.1}%** (`{lib_imp}/{lib_tot}`) · RDNA2 decoded: **{:.1}%** (`{dec_imp}/{dec_tot}`) · RDNA2 → DXIL lowered: **{:.1}%** (`{low_imp}/{low_tot}`) · Latest world: **{latest_fps}**.",
        data.meta.pages_url,
        render::pct(lib_imp, lib_tot),
        render::pct(dec_imp, dec_tot),
        render::pct(low_imp, low_tot),
    );
    let _ = writeln!(block);
    let _ = writeln!(
        block,
        "![Oathkeep Codebase Coverage Dashboard](docs/progress.svg)"
    );
    let _ = writeln!(block);
    let _ = writeln!(block, "| Run | Commit | Peak FPS | Frame | Summary |");
    let _ = writeln!(block, "| --- | --- | ---: | ---: | --- |");
    for r in data.runs.iter().rev().take(6) {
        let summary = if r.status.len() > 140 {
            format!("{}…", &r.status[..139])
        } else {
            r.status.clone()
        };
        let _ = writeln!(
            block,
            "| {} | `{}` | {:.1} | {:.1} ms | {} |",
            r.label, r.commit, r.fps, r.frame_ms, summary
        );
    }
    let _ = write!(block, "{README_END}");

    if let (Some(start), Some(end)) = (readme.find(README_START), readme.find(README_END)) {
        let mut out = String::with_capacity(readme.len() + block.len());
        out.push_str(&readme[..start]);
        out.push_str(&block);
        out.push_str(&readme[end + README_END.len()..]);
        return Ok(out);
    }

    if let Some(pos) = readme.find("## Requirements") {
        let mut out = String::with_capacity(readme.len() + block.len() + 2);
        out.push_str(&readme[..pos]);
        out.push_str(&block);
        out.push_str("\n\n");
        out.push_str(&readme[pos..]);
        return Ok(out);
    }

    bail!("README.md missing progress markers and `## Requirements` heading")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_real_codebase_counts() {
        let root = crate::workspace_root();
        let metrics = scan::scan_codebase(&root).unwrap();
        let (lib_imp, lib_tot) = metrics.libraries_totals();
        let (dec_imp, dec_tot) = metrics.isa_decoded_totals();
        let (low_imp, low_tot) = metrics.isa_lowered_totals();

        assert!(metrics.libraries.len() >= 35, "expected HLE + D3D12.x COM libraries");
        assert!(lib_imp > 0 && lib_imp < lib_tot);
        assert!(dec_tot >= 1200, "expected all 19 RDNA2 opcode tables + EXP");
        assert_eq!(dec_imp, dec_tot);
        assert!(low_imp > 200 && low_imp < low_tot);
        assert!(metrics.unsupported_total() > 20);
        assert_eq!(metrics.format_bars.len(), 4);
        assert!(metrics.format_bars.iter().all(|b| b.supported > 0));
    }

    #[test]
    fn squarify_fills_bounds_cleanly() {
        let bounds = treemap::Rect {
            x: 0.0,
            y: 0.0,
            w: 500.0,
            h: 250.0,
        };
        let items = vec![(0, 160), (1, 160), (2, 77), (3, 45), (4, 32)];
        let tiles = treemap::squarify(bounds, &items);
        assert_eq!(tiles.len(), items.len());
        let area: f64 = tiles.iter().map(|t| t.rect.w * t.rect.h).sum();
        assert!((area - 500.0 * 250.0).abs() < 1e-3);
    }
}
