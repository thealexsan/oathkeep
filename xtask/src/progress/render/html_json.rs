use std::fmt::Write as _;

use super::super::ProgressData;
use super::super::scan::CodeMetrics;
use super::{esc, pct};

/// Render machine-readable `docs/progress.json` consumed by `discord-notify.mjs` and external tools.
pub fn render_json(data: &ProgressData, metrics: &CodeMetrics) -> String {
    let (lib_imp, lib_tot) = metrics.libraries_totals();
    let (dec_imp, dec_tot) = metrics.isa_decoded_totals();
    let (low_imp, low_tot) = metrics.isa_lowered_totals();
    let fmt_elem = metrics.format_bars.first().map_or((0, 123), |f| (f.supported, f.total));
    let fmt_tile = metrics.format_bars.get(1).map_or((0, 123), |f| (f.supported, f.total));

    let latest = data.runs.last();
    let fps = latest.map_or(0.0f32, |r| r.fps);
    let target_fps = 30.0f32;
    let fps_pct = ((fps / target_fps) * 100.0).clamp(0.0, 100.0);
    let frame_ms = latest.map_or(0.0f32, |r| r.frame_ms);
    let latest_run = latest.map_or("none", |r| r.label.as_str());
    let latest_commit = latest.map_or(data.meta.commit.as_str(), |r| r.commit.as_str());

    let json_esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    };

    format!(
        r#"{{
  "updated": "{updated}",
  "branch": "{branch}",
  "commit": "{commit}",
  "pages_url": "{pages_url}",
  "libraries": {{
    "implemented": {lib_imp},
    "total": {lib_tot},
    "pct": {lib_pct:.2}
  }},
  "rdna2_decoded": {{
    "implemented": {dec_imp},
    "total": {dec_tot},
    "pct": {dec_pct:.2}
  }},
  "rdna2_lowered": {{
    "implemented": {low_imp},
    "total": {low_tot},
    "pct": {low_pct:.2}
  }},
  "dxgi_formats": {{
    "element": {elem_s},
    "tiling": {tile_s},
    "total": {elem_t},
    "pct": {fmt_pct:.2}
  }},
  "unsupported_sites": {unsup_tot},
  "performance": {{
    "latest_run": "{latest_run}",
    "commit": "{latest_commit}",
    "fps": {fps:.1},
    "target_fps": {target_fps:.1},
    "playable_pct": {fps_pct:.1},
    "frame_ms": {frame_ms:.1}
  }}
}}
"#,
        updated = json_esc(&data.meta.updated),
        branch = json_esc(&data.meta.branch),
        commit = json_esc(&data.meta.commit),
        pages_url = json_esc(&data.meta.pages_url),
        lib_pct = pct(lib_imp, lib_tot),
        dec_pct = pct(dec_imp, dec_tot),
        low_pct = pct(low_imp, low_tot),
        elem_s = fmt_elem.0,
        tile_s = fmt_tile.0,
        elem_t = fmt_elem.1,
        fmt_pct = pct(fmt_elem.0, fmt_elem.1),
        unsup_tot = metrics.unsupported_total(),
        latest_run = json_esc(latest_run),
        latest_commit = json_esc(latest_commit),
    )
}

/// Render GitHub Pages `docs/index.html` matching the dark `anyps5` treemap aesthetic.
pub fn render_html(data: &ProgressData, metrics: &CodeMetrics) -> String {
    let (lib_imp, lib_tot) = metrics.libraries_totals();
    let (dec_imp, _dec_tot) = metrics.isa_decoded_totals();
    let (low_imp, low_tot) = metrics.isa_lowered_totals();
    let fmt_elem = metrics.format_bars.first().map_or((0, 123), |f| (f.supported, f.total));
    let latest = data.runs.last();
    let fps_str = latest
        .map(|r| format!("{:.1} fps", r.fps))
        .unwrap_or_else(|| "—".into());
    let frame_str = latest
        .map(|r| format!("{:.1} ms/f", r.frame_ms))
        .unwrap_or_else(|| "—".into());

    let mut run_rows = String::new();
    for r in data.runs.iter().rev().take(12) {
        let _ = writeln!(
            run_rows,
            "<tr><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td class=\"mono num\">{:.1}</td><td class=\"mono num\">{:.1} ms</td><td class=\"muted\">{}</td></tr>",
            esc(&r.label),
            esc(&r.commit),
            r.fps,
            r.frame_ms,
            esc(&r.status)
        );
    }

    let mut milestone_items = String::new();
    for m in &data.milestones {
        let dot_cls = match m.state.as_str() {
            "done" => "dot done",
            "active" => "dot active",
            _ => "dot",
        };
        let _ = writeln!(
            milestone_items,
            "<div class=\"milestone\"><span class=\"{dot_cls}\"></span><span class=\"mlabel\">{}</span><span class=\"mono muted\">{}</span></div>",
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
  <title>Oathkeep — Coverage &amp; Progress Dashboard</title>
  <style>
    :root {{
      --bg: #0d0f12;
      --surface: #151920;
      --border: #252b35;
      --text: #f0f3f6;
      --muted: #8b949e;
      --green: #2fa343;
      --gold: #c98f24;
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
      max-width: 1160px;
      margin: 0 auto;
      padding: 28px 20px 56px;
    }}
    header {{
      display: flex;
      justify-content: space-between;
      align-items: flex-end;
      gap: 16px;
      border-bottom: 1px solid var(--border);
      padding-bottom: 14px;
      margin-bottom: 20px;
      flex-wrap: wrap;
    }}
    h1 {{ margin: 0; font-size: 22px; font-weight: 600; }}
    h2 {{ margin: 28px 0 10px; font-size: 13px; font-weight: 600; color: #c8ced6; text-transform: uppercase; letter-spacing: 0.05em; }}
    p.sub {{ margin: 4px 0 0; color: var(--muted); font-size: 13.5px; }}
    a {{ color: #58a6ff; text-decoration: none; }}
    a:hover {{ text-decoration: underline; }}
    .mono {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 12.5px; }}
    .muted {{ color: var(--muted); }}
    .stats {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
      gap: 10px;
      margin-bottom: 18px;
    }}
    .card {{
      background: var(--surface);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 12px 14px;
    }}
    .card .k {{ color: var(--muted); font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em; }}
    .card .v {{ font-size: 19px; font-weight: 600; margin-top: 2px; }}
    .card .s {{ color: var(--muted); font-size: 11.5px; margin-top: 2px; }}
    .green {{ color: var(--green); }}
    .svg-wrap {{
      background: var(--bg);
      border: 1px solid var(--border);
      border-radius: 6px;
      overflow-x: auto;
      margin-bottom: 24px;
    }}
    .svg-wrap img {{ display: block; width: 100%; height: auto; }}
    .milestones {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
      gap: 8px;
    }}
    .milestone {{
      display: flex;
      align-items: center;
      gap: 10px;
      background: var(--surface);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 9px 12px;
    }}
    .milestone .mlabel {{ font-size: 13px; font-weight: 500; flex: 1; }}
    .dot {{
      width: 10px;
      height: 10px;
      border-radius: 2px;
      background: #4a5058;
      flex-shrink: 0;
    }}
    .dot.done {{ background: var(--green); }}
    .dot.active {{ background: var(--gold); }}
    table {{ width: 100%; border-collapse: collapse; font-size: 12.5px; }}
    th, td {{ text-align: left; padding: 8px 10px; border-bottom: 1px solid #1d222b; vertical-align: top; }}
    th {{ color: var(--muted); font-weight: 600; font-size: 11px; text-transform: uppercase; }}
    td.num, th.num {{ text-align: right; white-space: nowrap; }}
    footer {{
      margin-top: 36px;
      padding-top: 14px;
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
        {branch} · {updated}
      </div>
    </header>

    <section class="stats">
      <div class="card">
        <div class="k">System &amp; COM Libraries</div>
        <div class="v green mono">{lib_pct:.1}%</div>
        <div class="s mono">{lib_imp} / {lib_tot} implemented</div>
      </div>
      <div class="card">
        <div class="k">RDNA2 → DXIL Lowered</div>
        <div class="v green mono">{low_pct:.1}%</div>
        <div class="s mono">{low_imp} / {low_tot} ({dec_imp} decoded)</div>
      </div>
      <div class="card">
        <div class="k">DXGI Formats</div>
        <div class="v mono">{fmt_pct:.1}%</div>
        <div class="s mono">{fmt_s} / {fmt_t} element formats</div>
      </div>
      <div class="card">
        <div class="k">Latest In-World FPS</div>
        <div class="v mono">{fps_str}</div>
        <div class="s mono">{frame_str} · target 30 fps</div>
      </div>
    </section>

    <div class="svg-wrap">
      <img src="progress.svg" alt="Oathkeep Codebase Coverage Dashboard">
    </div>

    <h2>Playability Milestones</h2>
    <section class="milestones">
      {milestone_items}
    </section>

    <h2>Recent Benchmark Runs</h2>
    <div class="card" style="padding: 4px 12px; overflow-x: auto;">
      <table>
        <thead><tr><th>Run</th><th>Commit</th><th class="num">Peak FPS</th><th class="num">Frame</th><th>Summary</th></tr></thead>
        <tbody>{run_rows}</tbody>
      </table>
    </div>

    <footer>
      <span>Clean-room Xbox Series X|S (GDK) compatibility layer — GPL-3.0-or-later.</span>
      <span class="mono">Counted directly from the codebase via cargo xtask progress</span>
    </footer>
  </main>
</body>
</html>
"#,
        headline = esc(&data.meta.headline),
        branch = esc(&data.meta.branch),
        updated = esc(&data.meta.updated),
        lib_pct = pct(lib_imp, lib_tot),
        low_pct = pct(low_imp, low_tot),
        fmt_pct = pct(fmt_elem.0, fmt_elem.1),
        fmt_s = fmt_elem.0,
        fmt_t = fmt_elem.1,
        milestone_items = milestone_items,
        run_rows = run_rows,
    )
}
