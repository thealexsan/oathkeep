use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const README_START: &str = "<!-- oathkeep:progress:start -->";
const README_END: &str = "<!-- oathkeep:progress:end -->";
const LEVELS: [&str; 5] = ["#161b22", "#0e4429", "#006d32", "#26a641", "#39d353"];

#[derive(Deserialize)]
struct ProgressData {
    meta: Meta,
    subsystems: Vec<Subsystem>,
    milestones: Vec<Milestone>,
    runs: Vec<RunEntry>,
}

#[derive(Deserialize)]
struct Meta {
    updated: String,
    commit: String,
    branch: String,
    target_fps: String,
    current_fps: String,
    current_frame_ms: String,
    current_gpu_ms: String,
    current_preparing_ms: String,
    headline: String,
    pages_url: String,
}

#[derive(Deserialize)]
struct Subsystem {
    name: String,
    cells: Vec<u8>,
    note: String,
}

#[derive(Deserialize)]
struct Milestone {
    label: String,
    state: String,
    level: u8,
    detail: String,
}

#[derive(Deserialize)]
struct RunEntry {
    label: String,
    commit: String,
    fps: f32,
    frame_ms: f32,
    gpu_ms: String,
    preparing_ms: String,
    status: String,
}

pub fn run(root: &Path, check: bool) -> Result<()> {
    let source_path = root.join("docs/progress.toml");
    let raw = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let data: ProgressData = toml::from_str(&raw).context("parsing docs/progress.toml")?;

    let svg = render_svg(&data);
    let html = render_html(&data);
    let readme_path = root.join("README.md");
    let readme_old = fs::read_to_string(&readme_path).context("reading README.md")?;
    let readme_new = update_readme(&readme_old, &data)?;

    let outputs = [
        (root.join("docs/progress.svg"), svg),
        (root.join("docs/index.html"), html),
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
    println!(
        "progress updated: docs/progress.svg, docs/index.html, README.md ({}% overall)",
        overall_percent(&data)
    );
    Ok(())
}

fn normalize_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn subsystem_percent(s: &Subsystem) -> u32 {
    if s.cells.is_empty() {
        return 0;
    }
    let sum: u32 = s.cells.iter().map(|&c| u32::from(c.min(4))).sum();
    let max = (s.cells.len() as u32) * 4;
    (sum * 100 + max / 2) / max
}

fn overall_percent(data: &ProgressData) -> u32 {
    let mut sum = 0u32;
    let mut max = 0u32;
    for s in &data.subsystems {
        sum += s.cells.iter().map(|&c| u32::from(c.min(4))).sum::<u32>();
        max += (s.cells.len() as u32) * 4;
    }
    if max == 0 {
        0
    } else {
        (sum * 100 + max / 2) / max
    }
}

fn color_for(level: u8) -> &'static str {
    LEVELS[usize::from(level.min(4))]
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_svg(data: &ProgressData) -> String {
    let pct = overall_percent(data);
    let rows = data.subsystems.len();
    let row_h = 22usize;
    let grid_top = 86usize;
    let height = (grid_top + rows * row_h + 62).max(408);
    let width = 880usize;

    let mut out = String::with_capacity(8192);
    let _ = writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" role=\"img\" aria-label=\"Oathkeep Compatibility and Performance Progress\">"
    );
    let _ = writeln!(out, "  <defs>");
    let _ = writeln!(
        out,
        "    <style>text {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif; }} .mono {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }}</style>"
    );
    let _ = writeln!(out, "  </defs>");
    let _ = writeln!(
        out,
        "  <rect x=\"0.5\" y=\"0.5\" width=\"{}\" height=\"{}\" rx=\"8\" fill=\"#0d1117\" stroke=\"#30363d\"/>",
        width - 1,
        height - 1
    );

    // Header
    let _ = writeln!(
        out,
        "  <text x=\"24\" y=\"32\" fill=\"#f0f6fc\" font-size=\"16\" font-weight=\"600\">Oathkeep Progress Matrix</text>"
    );
    let _ = writeln!(
        out,
        "  <text x=\"24\" y=\"52\" fill=\"#8b949e\" font-size=\"12\">{}</text>",
        esc(&data.meta.headline)
    );
    let _ = writeln!(
        out,
        "  <text x=\"{}\" y=\"32\" text-anchor=\"end\" fill=\"#39d353\" font-size=\"16\" font-weight=\"700\" class=\"mono\">{pct}%</text>",
        width - 24
    );
    let _ = writeln!(
        out,
        "  <text x=\"{}\" y=\"52\" text-anchor=\"end\" fill=\"#8b949e\" font-size=\"11\" class=\"mono\">{} ({}) • {}</text>",
        width - 24,
        esc(&data.meta.commit),
        esc(&data.meta.branch),
        esc(&data.meta.updated)
    );
    let _ = writeln!(
        out,
        "  <line x1=\"24\" y1=\"66\" x2=\"{}\" y2=\"66\" stroke=\"#21262d\"/>",
        width - 24
    );

    // Left panel: Subsystem green-box matrix
    let _ = writeln!(
        out,
        "  <text x=\"24\" y=\"80\" fill=\"#c9d1d9\" font-size=\"11\" font-weight=\"600\">SUBSYSTEM CAPABILITY MATRIX</text>"
    );
    let cell_x0 = 200usize;
    let cell_sz = 13usize;
    let cell_gap = 4usize;

    for (i, sub) in data.subsystems.iter().enumerate() {
        let y = grid_top + 10 + i * row_h;
        let sub_pct = subsystem_percent(sub);
        let _ = writeln!(
            out,
            "  <text x=\"24\" y=\"{}\" fill=\"#c9d1d9\" font-size=\"12\">{}</text>",
            y + 10,
            esc(&sub.name)
        );
        for (j, &lvl) in sub.cells.iter().take(12).enumerate() {
            let x = cell_x0 + j * (cell_sz + cell_gap);
            let fill = color_for(lvl);
            let _ = writeln!(
                out,
                "  <rect x=\"{x}\" y=\"{y}\" width=\"{cell_sz}\" height=\"{cell_sz}\" rx=\"2.5\" fill=\"{fill}\" stroke=\"#21262d\" stroke-width=\"0.75\"/>"
            );
        }
        let pct_x = cell_x0 + 10 * (cell_sz + cell_gap) + 12;
        let _ = writeln!(
            out,
            "  <text x=\"{pct_x}\" y=\"{}\" fill=\"#8b949e\" font-size=\"11\" class=\"mono\">{sub_pct:>3}%</text>",
            y + 10
        );
    }

    // Divider between left and right columns
    let _ = writeln!(
        out,
        "  <line x1=\"452\" y1=\"72\" x2=\"452\" y2=\"{}\" stroke=\"#21262d\"/>",
        height - 42
    );

    // Right panel: Compatibility milestones
    let rx = 474usize;
    let _ = writeln!(
        out,
        "  <text x=\"{rx}\" y=\"80\" fill=\"#c9d1d9\" font-size=\"11\" font-weight=\"600\">PLAYABILITY MILESTONES</text>"
    );
    for (i, m) in data.milestones.iter().enumerate() {
        let y = grid_top + 10 + i * 24;
        let fill = color_for(m.level);
        let stroke = if m.state == "active" {
            "#39d353"
        } else {
            "#21262d"
        };
        let label_fill = match m.state.as_str() {
            "done" => "#f0f6fc",
            "active" => "#39d353",
            _ => "#8b949e",
        };
        let _ = writeln!(
            out,
            "  <rect x=\"{rx}\" y=\"{y}\" width=\"13\" height=\"13\" rx=\"2.5\" fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1\"/>"
        );
        let _ = writeln!(
            out,
            "  <text x=\"{}\" y=\"{}\" fill=\"{label_fill}\" font-size=\"12\" font-weight=\"500\">{}</text>",
            rx + 21,
            y + 10,
            esc(&m.label)
        );
        let _ = writeln!(
            out,
            "  <text x=\"{}\" y=\"{}\" text-anchor=\"end\" fill=\"#8b949e\" font-size=\"10.5\" class=\"mono\">{}</text>",
            width - 24,
            y + 10,
            esc(&m.detail)
        );
    }

    // Right-bottom: Live telemetry strip
    let card_y = grid_top + 18 + data.milestones.len() * 24;
    let card_w = width - rx - 24;
    let _ = writeln!(
        out,
        "  <rect x=\"{rx}\" y=\"{card_y}\" width=\"{card_w}\" height=\"62\" rx=\"6\" fill=\"#161b22\" stroke=\"#30363d\"/>"
    );
    let _ = writeln!(
        out,
        "  <text x=\"{}\" y=\"{}\" fill=\"#8b949e\" font-size=\"10.5\" font-weight=\"600\">LATEST WORLD TELEMETRY (RTX 3070 8 GB, 1080P)</text>",
        rx + 12,
        card_y + 18
    );
    let metrics = [
        ("FPS", data.meta.current_fps.as_str(), "#39d353"),
        ("FRAME", data.meta.current_frame_ms.as_str(), "#f0f6fc"),
        ("GPU", data.meta.current_gpu_ms.as_str(), "#f0f6fc"),
        ("PREP", data.meta.current_preparing_ms.as_str(), "#f0f6fc"),
    ];
    let col_w = (card_w - 24) / metrics.len();
    for (idx, (k, v, col)) in metrics.iter().enumerate() {
        let mx = rx + 12 + idx * col_w;
        let _ = writeln!(
            out,
            "  <text x=\"{mx}\" y=\"{}\" fill=\"#8b949e\" font-size=\"10\" class=\"mono\">{k}</text>",
            card_y + 34
        );
        let _ = writeln!(
            out,
            "  <text x=\"{mx}\" y=\"{}\" fill=\"{col}\" font-size=\"12.5\" font-weight=\"600\" class=\"mono\">{}</text>",
            card_y + 51,
            esc(v)
        );
    }

    // Footer legend
    let fy = height - 22;
    let _ = writeln!(
        out,
        "  <line x1=\"24\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#21262d\"/>",
        height - 38,
        width - 24,
        height - 38
    );
    let _ = writeln!(
        out,
        "  <text x=\"24\" y=\"{fy}\" fill=\"#8b949e\" font-size=\"11\">Target: {} • Clean-room Xbox Series X|S (GDK) layer</text>",
        esc(&data.meta.target_fps)
    );
    let lx0 = width - 168;
    let _ = writeln!(
        out,
        "  <text x=\"{}\" y=\"{fy}\" text-anchor=\"end\" fill=\"#8b949e\" font-size=\"11\">Less</text>",
        lx0 - 8
    );
    for (i, &col) in LEVELS.iter().enumerate() {
        let x = lx0 + i * 16;
        let _ = writeln!(
            out,
            "  <rect x=\"{x}\" y=\"{}\" width=\"12\" height=\"12\" rx=\"2.5\" fill=\"{col}\" stroke=\"#21262d\" stroke-width=\"0.75\"/>",
            fy - 10
        );
    }
    let _ = writeln!(
        out,
        "  <text x=\"{}\" y=\"{fy}\" fill=\"#8b949e\" font-size=\"11\">More</text>",
        lx0 + 5 * 16 + 4
    );

    let _ = writeln!(out, "</svg>");
    out
}

fn update_readme(readme: &str, data: &ProgressData) -> Result<String> {
    let pct = overall_percent(data);
    let mut block = String::new();
    let _ = writeln!(block, "{README_START}");
    let _ = writeln!(block, "## Compatibility & Performance Progress ({pct}%)");
    let _ = writeln!(block);
    let _ = writeln!(
        block,
        "Live dashboard: [thealexsan.github.io/oathkeep]({}) • Auto-generated from [docs/progress.toml](docs/progress.toml) via `cargo xtask progress`.",
        data.meta.pages_url
    );
    let _ = writeln!(block);
    let _ = writeln!(
        block,
        "![Oathkeep Compatibility and Performance Progress](docs/progress.svg)"
    );
    let _ = writeln!(block);
    let _ = writeln!(
        block,
        "| Run | Commit | FPS | Frame | GPU | Preparing | Status |"
    );
    let _ = writeln!(block, "| --- | --- | ---: | ---: | ---: | ---: | --- |");
    for r in data.runs.iter().rev().take(6) {
        let _ = writeln!(
            block,
            "| {} | `{}` | {:.1} | {:.1} ms | {} | {} | {} |",
            r.label, r.commit, r.fps, r.frame_ms, r.gpu_ms, r.preparing_ms, r.status
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

    // Insert right before `## Requirements` if markers do not exist yet.
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

fn render_html(data: &ProgressData) -> String {
    let pct = overall_percent(data);
    let mut sub_rows = String::new();
    for s in &data.subsystems {
        let sp = subsystem_percent(s);
        let mut boxes = String::new();
        for &lvl in &s.cells {
            let _ = write!(
                boxes,
                "<span class=\"box l{}\"></span>",
                lvl.min(4)
            );
        }
        let _ = writeln!(
            sub_rows,
            "<tr><td class=\"name\">{}</td><td class=\"boxes\">{}</td><td class=\"mono num\">{sp}%</td><td class=\"muted\">{}</td></tr>",
            esc(&s.name),
            boxes,
            esc(&s.note)
        );
    }

    let mut run_rows = String::new();
    for r in data.runs.iter().rev() {
        let _ = writeln!(
            run_rows,
            "<tr><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td class=\"mono num\">{:.1}</td><td class=\"mono num\">{:.1} ms</td><td class=\"mono num\">{}</td><td class=\"mono num\">{}</td><td>{}</td></tr>",
            esc(&r.label),
            esc(&r.commit),
            r.fps,
            r.frame_ms,
            esc(&r.gpu_ms),
            esc(&r.preparing_ms),
            esc(&r.status)
        );
    }

    let mut milestone_items = String::new();
    for m in &data.milestones {
        let _ = writeln!(
            milestone_items,
            "<div class=\"milestone\"><span class=\"box l{} {}\"></span><span class=\"mlabel\">{}</span><span class=\"mono muted\">{}</span></div>",
            m.level.min(4),
            if m.state == "active" { "active" } else { "" },
            esc(&m.label),
            esc(&m.detail)
        );
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Oathkeep — Progress Dashboard</title>
  <style>
    :root {{
      --bg: #0d1117;
      --surface: #161b22;
      --border: #30363d;
      --text: #f0f6fc;
      --muted: #8b949e;
      --l0: #161b22;
      --l1: #0e4429;
      --l2: #006d32;
      --l3: #26a641;
      --l4: #39d353;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      background: var(--bg);
      color: var(--text);
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
      line-height: 1.5;
    }}
    main {{
      max-width: 960px;
      margin: 0 auto;
      padding: 32px 20px 64px;
    }}
    header {{
      display: flex;
      justify-content: space-between;
      align-items: flex-end;
      gap: 16px;
      border-bottom: 1px solid var(--border);
      padding-bottom: 16px;
      margin-bottom: 24px;
      flex-wrap: wrap;
    }}
    h1 {{ margin: 0; font-size: 24px; font-weight: 600; }}
    h2 {{ margin: 28px 0 12px; font-size: 15px; font-weight: 600; color: #c9d1d9; text-transform: uppercase; letter-spacing: 0.04em; }}
    p.sub {{ margin: 4px 0 0; color: var(--muted); font-size: 14px; }}
    a {{ color: #58a6ff; text-decoration: none; }}
    a:hover {{ text-decoration: underline; }}
    .mono {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 12.5px; }}
    .muted {{ color: var(--muted); }}
    .stats {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
      gap: 12px;
      margin-bottom: 20px;
    }}
    .card {{
      background: var(--surface);
      border: 1px solid var(--border);
      border-radius: 8px;
      padding: 14px 16px;
    }}
    .card .k {{ color: var(--muted); font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em; }}
    .card .v {{ font-size: 20px; font-weight: 600; margin-top: 4px; }}
    .green {{ color: var(--l4); }}
    .svg-wrap {{
      background: var(--bg);
      border-radius: 8px;
      overflow-x: auto;
      margin-bottom: 24px;
    }}
    .svg-wrap img {{ display: block; width: 100%; height: auto; }}
    table {{ width: 100%; border-collapse: collapse; font-size: 13px; }}
    th, td {{ text-align: left; padding: 8px 10px; border-bottom: 1px solid #21262d; }}
    th {{ color: var(--muted); font-weight: 600; font-size: 11.5px; text-transform: uppercase; }}
    td.num, th.num {{ text-align: right; }}
    td.name {{ font-weight: 500; white-space: nowrap; }}
    .boxes {{ white-space: nowrap; }}
    .box {{
      display: inline-block;
      width: 12px;
      height: 12px;
      border-radius: 2.5px;
      margin-right: 3px;
      border: 1px solid #21262d;
      vertical-align: middle;
    }}
    .box.active {{ border-color: var(--l4); }}
    .l0 {{ background: var(--l0); }}
    .l1 {{ background: var(--l1); }}
    .l2 {{ background: var(--l2); }}
    .l3 {{ background: var(--l3); }}
    .l4 {{ background: var(--l4); }}
    .milestones {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
      gap: 10px;
    }}
    .milestone {{
      display: flex;
      align-items: center;
      gap: 10px;
      background: var(--surface);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 10px 12px;
    }}
    .milestone .mlabel {{ font-size: 13px; font-weight: 500; flex: 1; }}
    footer {{
      margin-top: 40px;
      padding-top: 16px;
      border-top: 1px solid var(--border);
      color: var(--muted);
      font-size: 12px;
      display: flex;
      justify-content: space-between;
      flex-wrap: wrap;
      gap: 8px;
    }}
  </style>
</head>
<body>
  <main>
    <header>
      <div>
        <h1>Oathkeep</h1>
        <p class="sub">{headline}</p>
      </div>
      <div class="mono muted">
        <a href="https://github.com/thealexsan/oathkeep">GitHub</a> ·
        <a href="https://discord.gg/PM6pr4Azzk">Discord</a> ·
        {commit} ({branch}) · {updated}
      </div>
    </header>

    <section class="stats">
      <div class="card"><div class="k">Overall Matrix</div><div class="v green mono">{pct}%</div></div>
      <div class="card"><div class="k">Latest World FPS</div><div class="v mono">{fps}</div></div>
      <div class="card"><div class="k">Frame / GPU</div><div class="v mono">{frame_ms} / {gpu_ms}</div></div>
      <div class="card"><div class="k">CPU Preparing</div><div class="v mono">{prep_ms}</div></div>
    </section>

    <div class="svg-wrap">
      <img src="progress.svg" alt="Oathkeep Progress Matrix">
    </div>

    <h2>Playability Milestones</h2>
    <section class="milestones">
      {milestone_items}
    </section>

    <h2>Subsystem Breakdown</h2>
    <div class="card" style="padding: 4px 12px; overflow-x: auto;">
      <table>
        <thead><tr><th>Subsystem</th><th>Progress</th><th class="num">Score</th><th>Notes</th></tr></thead>
        <tbody>{sub_rows}</tbody>
      </table>
    </div>

    <h2>Telemetry Progression</h2>
    <div class="card" style="padding: 4px 12px; overflow-x: auto;">
      <table>
        <thead><tr><th>Run</th><th>Commit</th><th class="num">FPS</th><th class="num">Frame</th><th class="num">GPU</th><th class="num">Preparing</th><th>Status</th></tr></thead>
        <tbody>{run_rows}</tbody>
      </table>
    </div>

    <footer>
      <span>Clean-room Xbox Series X|S (GDK) compatibility layer — GPL-3.0-or-later.</span>
      <span class="mono">Target: {target_fps}</span>
    </footer>
  </main>
</body>
</html>
"#,
        headline = esc(&data.meta.headline),
        commit = esc(&data.meta.commit),
        branch = esc(&data.meta.branch),
        updated = esc(&data.meta.updated),
        pct = pct,
        fps = esc(&data.meta.current_fps),
        frame_ms = esc(&data.meta.current_frame_ms),
        gpu_ms = esc(&data.meta.current_gpu_ms),
        prep_ms = esc(&data.meta.current_preparing_ms),
        milestone_items = milestone_items,
        sub_rows = sub_rows,
        run_rows = run_rows,
        target_fps = esc(&data.meta.target_fps),
    )
}
