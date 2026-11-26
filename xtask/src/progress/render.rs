use std::fmt::Write as _;

use super::ProgressData;
use super::scan::{CodeMetrics, IsaFamily, LibraryEntry};
use super::treemap::{Rect, grid_dims, squarify};

const BG: &str = "#0d0f12";
const GREEN: &str = "#2fa343";
const GRAY: &str = "#5a6068";
const GOLD: &str = "#c98f24";
const TRACK: &str = "#4a5058";
const GRID_STROKE: &str = "#14181d";
const TEXT_MAIN: &str = "#f0f3f6";
const TEXT_SUB: &str = "#c8ced6";
const TEXT_MUTED: &str = "#8b949e";

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn pct(done: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 * 100.0) / total as f64
    }
}

/// Render an `anyps5`-style code-counted SVG treemap dashboard.
pub fn render_svg(data: &ProgressData, metrics: &CodeMetrics) -> String {
    let width = 1120.0f64;
    let pad = 16.0f64;
    let col_gap = 16.0f64;
    let col_w = (width - pad * 2.0 - col_gap) / 2.0; // 536px

    // Partition libraries into Large (>= 32), Medium (16..32), Small (< 16)
    let large: Vec<&LibraryEntry> = metrics.libraries.iter().filter(|l| l.total() >= 32).collect();
    let medium: Vec<&LibraryEntry> = metrics
        .libraries
        .iter()
        .filter(|l| (16..32).contains(&l.total()))
        .collect();
    let small: Vec<&LibraryEntry> = metrics.libraries.iter().filter(|l| l.total() < 16).collect();

    let (lib_imp, lib_tot) = metrics.libraries_totals();
    let (lg_imp, lg_tot) = sum_libs(&large);
    let (md_imp, md_tot) = sum_libs(&medium);
    let (sm_imp, sm_tot) = sum_libs(&small);

    let (dec_imp, dec_tot) = metrics.isa_decoded_totals();
    let (low_imp, low_tot) = metrics.isa_lowered_totals();

    // Layout Y coordinates
    let mut y = 26.0f64;
    let top_header_y = y;
    y += 24.0;

    // Row 1: Large + Medium libraries side-by-side
    let row1_label_y = y;
    let row1_top = y + 8.0;
    let row1_h = 228.0f64;
    y = row1_top + row1_h + 26.0;

    // Row 2: Small libraries uniform mini-card grid
    let small_label_y = y;
    let small_top = y + 8.0;
    let small_cols = 8usize;
    let small_gap = 4.0f64;
    let small_card_w = (width - pad * 2.0 - small_gap * (small_cols as f64 - 1.0)) / small_cols as f64;
    let small_card_h = 28.0f64;
    let small_rows = small.len().div_ceil(small_cols).max(1);
    let small_h = small_rows as f64 * (small_card_h + small_gap) - small_gap;
    y = small_top + small_h + 28.0;

    // Row 3: RDNA2 decoded + RDNA2 -> DXIL lowered side-by-side
    let isa_label_y = y;
    let isa_top = y + 8.0;
    let isa_h = 252.0f64;
    y = isa_top + isa_h + 26.0;

    // Row 4: Left (Unsupported sites + DXGI formats) | Right (Instruction forms still needed)
    let bottom_top = y;
    let left_h = 24.0
        + metrics.unsupported_areas.len() as f64 * 22.0
        + 26.0
        + 24.0
        + metrics.format_bars.len() as f64 * 22.0;

    let missing_rows = build_missing_opcode_lines(&metrics.isa_families);
    let right_lines: usize = missing_rows.iter().map(|(_, lines)| lines.len()).sum();
    let right_h = 44.0 + right_lines as f64 * 13.5 + missing_rows.len() as f64 * 6.0;

    let bottom_h = left_h.max(right_h);
    let footer_y = bottom_top + bottom_h + 22.0;
    let height = (footer_y + 16.0).ceil() as usize;

    let mut out = String::with_capacity(64 * 1024);
    let _ = writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{height}\" viewBox=\"0 0 {} {height}\" role=\"img\" aria-label=\"Oathkeep Codebase Coverage Dashboard\">",
        width as usize, width as usize
    );
    let _ = writeln!(out, "  <defs>");
    let _ = writeln!(
        out,
        "    <style>text {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif; }} .mono {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }} .lbl {{ fill: {TEXT_MAIN}; font-size: 11px; font-weight: 500; paint-order: stroke; stroke: {BG}; stroke-width: 2.5px; stroke-linejoin: round; }}</style>"
    );
    let _ = writeln!(out, "  </defs>");
    let _ = writeln!(
        out,
        "  <rect width=\"{}\" height=\"{height}\" fill=\"{BG}\"/>",
        width as usize
    );

    // Top banner header
    let _ = writeln!(
        out,
        "  <text x=\"{pad}\" y=\"{top_header_y}\" fill=\"{TEXT_MAIN}\" font-size=\"15.5\" font-weight=\"600\">System &amp; Console Libraries*: {:.2}% ({lib_imp}/{lib_tot})</text>",
        pct(lib_imp, lib_tot)
    );
    let latest_fps = data
        .runs
        .last()
        .map(|r| format!("{:.1} fps ({:.1} ms/f)", r.fps, r.frame_ms))
        .unwrap_or_else(|| data.meta.current_fps.clone());
    let _ = writeln!(
        out,
        "  <text x=\"{}\" y=\"{top_header_y}\" text-anchor=\"end\" fill=\"{TEXT_MUTED}\" font-size=\"11\" class=\"mono\">World: {} · {}</text>",
        width - pad,
        esc(&latest_fps),
        esc(&data.meta.updated)
    );

    // Row 1: Large & Medium library treemaps
    let _ = writeln!(
        out,
        "  <text x=\"{pad}\" y=\"{row1_label_y}\" fill=\"{TEXT_MAIN}\" font-size=\"13.5\" font-weight=\"600\">Large libraries: {:.2}% ({lg_imp}/{lg_tot})</text>",
        pct(lg_imp, lg_tot)
    );
    let right_x = pad + col_w + col_gap;
    let _ = writeln!(
        out,
        "  <text x=\"{right_x}\" y=\"{row1_label_y}\" fill=\"{TEXT_MAIN}\" font-size=\"13.5\" font-weight=\"600\">Medium libraries: {:.2}% ({md_imp}/{md_tot})</text>",
        pct(md_imp, md_tot)
    );
    render_lib_treemap(
        &mut out,
        &large,
        Rect {
            x: pad,
            y: row1_top,
            w: col_w,
            h: row1_h,
        },
    );
    render_lib_treemap(
        &mut out,
        &medium,
        Rect {
            x: right_x,
            y: row1_top,
            w: col_w,
            h: row1_h,
        },
    );

    // Row 2: Small libraries uniform card grid
    let _ = writeln!(
        out,
        "  <text x=\"{pad}\" y=\"{small_label_y}\" fill=\"{TEXT_MAIN}\" font-size=\"13.5\" font-weight=\"600\">Small libraries: {:.2}% ({sm_imp}/{sm_tot})</text>",
        pct(sm_imp, sm_tot)
    );
    for (idx, lib) in small.iter().enumerate() {
        let col = idx % small_cols;
        let row = idx / small_cols;
        let rect = Rect {
            x: pad + col as f64 * (small_card_w + small_gap),
            y: small_top + row as f64 * (small_card_h + small_gap),
            w: small_card_w,
            h: small_card_h,
        };
        render_cell_tile(&mut out, &lib.name, &lib.cells, rect);
    }

    // Row 3: Side-by-side RDNA2 ISA treemaps
    let _ = writeln!(
        out,
        "  <text x=\"{pad}\" y=\"{isa_label_y}\" fill=\"{TEXT_MAIN}\" font-size=\"13.5\" font-weight=\"600\">RDNA2 (GFX10.3) opcodes decoded: {:.2}% ({dec_imp}/{dec_tot})</text>",
        pct(dec_imp, dec_tot)
    );
    let _ = writeln!(
        out,
        "  <text x=\"{right_x}\" y=\"{isa_label_y}\" fill=\"{TEXT_MAIN}\" font-size=\"13.5\" font-weight=\"600\">RDNA2 → DXIL lowered: {:.2}% ({low_imp}/{low_tot})</text>",
        pct(low_imp, low_tot)
    );
    render_isa_treemap(
        &mut out,
        &metrics.isa_families,
        false,
        Rect {
            x: pad,
            y: isa_top,
            w: col_w,
            h: isa_h,
        },
    );
    render_isa_treemap(
        &mut out,
        &metrics.isa_families,
        true,
        Rect {
            x: right_x,
            y: isa_top,
            w: col_w,
            h: isa_h,
        },
    );

    // Row 4 Left: Unsupported sites by area (gold bars) + DXGI / Image formats (green bars)
    let mut ly = bottom_top;
    let total_unsup = metrics.unsupported_total();
    let _ = writeln!(
        out,
        "  <text x=\"{pad}\" y=\"{ly}\" fill=\"{TEXT_MAIN}\" font-size=\"13.5\" font-weight=\"600\">Unsupported / stub sites by area ({total_unsup})</text>"
    );
    ly += 14.0;
    let label_col = 166.0f64;
    let count_col = 54.0f64;
    let bar_x = pad + label_col;
    let bar_w = col_w - label_col - count_col;
    let max_unsup = metrics
        .unsupported_areas
        .iter()
        .map(|a| a.count)
        .max()
        .unwrap_or(1)
        .max(1);

    for area in &metrics.unsupported_areas {
        let fill_w = (bar_w * area.count as f64 / max_unsup as f64).max(2.0);
        let _ = writeln!(
            out,
            "  <text x=\"{pad}\" y=\"{:.1}\" fill=\"{TEXT_SUB}\" font-size=\"11.5\">{}</text>",
            ly + 11.0,
            esc(&area.name)
        );
        let _ = writeln!(
            out,
            "  <rect x=\"{bar_x:.1}\" y=\"{ly:.1}\" width=\"{bar_w:.1}\" height=\"13\" fill=\"{TRACK}\"/>"
        );
        let _ = writeln!(
            out,
            "  <rect x=\"{bar_x:.1}\" y=\"{ly:.1}\" width=\"{fill_w:.1}\" height=\"13\" fill=\"{GOLD}\"/>"
        );
        let _ = writeln!(
            out,
            "  <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"{TEXT_MAIN}\" font-size=\"11.5\" class=\"mono\">{}</text>",
            pad + col_w,
            ly + 11.0,
            area.count
        );
        ly += 22.0;
    }

    ly += 20.0;
    let fmt_total = metrics.format_bars.first().map_or(123, |f| f.total);
    let _ = writeln!(
        out,
        "  <text x=\"{pad}\" y=\"{ly}\" fill=\"{TEXT_MAIN}\" font-size=\"13.5\" font-weight=\"600\">DXGI / Image formats (of {fmt_total})</text>"
    );
    ly += 14.0;
    for bar in &metrics.format_bars {
        let fill_w = if bar.total > 0 {
            (bar_w * bar.supported as f64 / bar.total as f64).max(2.0)
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "  <text x=\"{pad}\" y=\"{:.1}\" fill=\"{TEXT_SUB}\" font-size=\"11.5\">{}</text>",
            ly + 11.0,
            esc(&bar.name)
        );
        let _ = writeln!(
            out,
            "  <rect x=\"{bar_x:.1}\" y=\"{ly:.1}\" width=\"{bar_w:.1}\" height=\"13\" fill=\"{TRACK}\"/>"
        );
        let _ = writeln!(
            out,
            "  <rect x=\"{bar_x:.1}\" y=\"{ly:.1}\" width=\"{fill_w:.1}\" height=\"13\" fill=\"{GREEN}\"/>"
        );
        let _ = writeln!(
            out,
            "  <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"{TEXT_MAIN}\" font-size=\"11.5\" class=\"mono\">{}/{}</text>",
            pad + col_w,
            ly + 11.0,
            bar.supported,
            bar.total
        );
        ly += 22.0;
    }

    // Row 4 Right: Instruction forms still needed (monospace grouped list)
    let mut ry = bottom_top;
    let not_lowered = low_tot.saturating_sub(low_imp);
    let _ = writeln!(
        out,
        "  <text x=\"{right_x}\" y=\"{ry}\" fill=\"{TEXT_MAIN}\" font-size=\"13.5\" font-weight=\"600\">Instruction forms still needed</text>"
    );
    ry += 17.0;
    let _ = writeln!(
        out,
        "  <text x=\"{right_x}\" y=\"{ry}\" fill=\"{TEXT_SUB}\" font-size=\"11.5\">Instruction forms not lowered to DXIL ({not_lowered})</text>"
    );
    ry += 18.0;
    let list_x = right_x + 70.0;
    for (fam, lines) in &missing_rows {
        let _ = writeln!(
            out,
            "  <text x=\"{right_x}\" y=\"{ry:.1}\" fill=\"{TEXT_MAIN}\" font-size=\"10\" font-weight=\"600\" class=\"mono\">{}</text>",
            esc(fam)
        );
        for line in lines {
            let _ = writeln!(
                out,
                "  <text x=\"{list_x:.1}\" y=\"{ry:.1}\" fill=\"{TEXT_SUB}\" font-size=\"9.2\" class=\"mono\">{}</text>",
                esc(line)
            );
            ry += 13.5;
        }
        ry += 6.0;
    }

    // Footer
    let _ = writeln!(
        out,
        "  <text x=\"{pad}\" y=\"{footer_y:.1}\" fill=\"{TEXT_MUTED}\" font-size=\"10.5\">*HLE exports, runtime vtables, and D3D12.x/DXGI COM slots. Counted directly from the codebase — not from playing games.</text>"
    );
    let _ = writeln!(
        out,
        "  <text x=\"{}\" y=\"{footer_y:.1}\" text-anchor=\"end\" fill=\"{TEXT_MUTED}\" font-size=\"10.5\" class=\"mono\">cargo xtask progress</text>",
        width - pad
    );

    let _ = writeln!(out, "</svg>");
    out
}

fn sum_libs(libs: &[&LibraryEntry]) -> (usize, usize) {
    let imp = libs.iter().map(|l| l.implemented()).sum();
    let tot = libs.iter().map(|l| l.total()).sum();
    (imp, tot)
}

fn render_lib_treemap(out: &mut String, libs: &[&LibraryEntry], bounds: Rect) {
    let weights: Vec<(usize, usize)> = libs.iter().enumerate().map(|(i, l)| (i, l.total())).collect();
    let tiles = squarify(bounds, &weights);
    for tile in tiles {
        let lib = libs[tile.index];
        render_cell_tile(out, &lib.name, &lib.cells, tile.rect);
    }
}

fn render_isa_treemap(out: &mut String, families: &[IsaFamily], lowered: bool, bounds: Rect) {
    let weights: Vec<(usize, usize)> = families
        .iter()
        .enumerate()
        .map(|(i, f)| (i, f.total()))
        .collect();
    let tiles = squarify(bounds, &weights);
    for tile in tiles {
        let fam = &families[tile.index];
        let cells = if lowered { &fam.lowered } else { &fam.decoded };
        render_cell_tile(out, &fam.name, cells, tile.rect);
    }
}

/// Renders one treemap tile with exact per-slot green/gray micro-squares (compressed with horizontal RLE runs
/// plus a single grid-line `<path>`) so the SVG stays tiny (< 80 KB) while preserving exact slot positions.
fn render_cell_tile(out: &mut String, name: &str, cells: &[bool], rect: Rect) {
    let inset = 1.0f64;
    let x = rect.x + inset;
    let y = rect.y + inset;
    let w = (rect.w - inset * 2.0).max(1.0);
    let h = (rect.h - inset * 2.0).max(1.0);
    let n = cells.len();
    let done = cells.iter().filter(|&&b| b).count();
    let (cols, rows) = grid_dims(n, w, h);
    let cw = w / cols as f64;
    let ch = h / rows as f64;

    let _ = writeln!(out, "  <g>");
    let _ = writeln!(
        out,
        "    <title>{}: {done}/{n} ({:.1}%)</title>",
        esc(name),
        pct(done, n)
    );

    // Base fill: gray if mixed/empty, green if 100% implemented
    let base = if done == n && n > 0 { GREEN } else { GRAY };
    let _ = writeln!(
        out,
        "    <rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" fill=\"{base}\"/>"
    );

    // Paint exact green cell runs when partially implemented
    if done > 0 && done < n {
        let mut d = String::new();
        for r in 0..rows {
            let mut c = 0usize;
            while c < cols {
                let idx = r * cols + c;
                if idx >= n {
                    break;
                }
                if !cells[idx] {
                    c += 1;
                    continue;
                }
                let start_c = c;
                while c < cols && (r * cols + c) < n && cells[r * cols + c] {
                    c += 1;
                }
                let rx = x + start_c as f64 * cw;
                let ry = y + r as f64 * ch;
                let rw = (c - start_c) as f64 * cw;
                let _ = write!(d, "M{rx:.1} {ry:.1}h{rw:.1}v{ch:.1}h-{rw:.1}z");
            }
        }
        if !d.is_empty() {
            let _ = writeln!(out, "    <path d=\"{d}\" fill=\"{GREEN}\"/>");
        }
    }

    // Mask trailing unused cells on the last row with background color so cell count is exact
    let total_grid = cols * rows;
    if total_grid > n && rows > 1 {
        let used_last = n - (rows - 1) * cols;
        let rx = x + used_last as f64 * cw;
        let ry = y + (rows - 1) as f64 * ch;
        let rw = (cols - used_last) as f64 * cw;
        let _ = writeln!(
            out,
            "    <rect x=\"{rx:.1}\" y=\"{ry:.1}\" width=\"{rw:.1}\" height=\"{ch:.1}\" fill=\"{BG}\" opacity=\"0.55\"/>"
        );
    }

    // Crisp micro-grid lines (only when cells are large enough that lines don't drown the color)
    if cw >= 3.2 && ch >= 3.2 {
        let mut grid = String::new();
        for c in 1..cols {
            let gx = x + c as f64 * cw;
            let _ = write!(grid, "M{gx:.1} {y:.1}v{h:.1}");
        }
        for r in 1..rows {
            let gy = y + r as f64 * ch;
            let _ = write!(grid, "M{x:.1} {gy:.1}h{w:.1}");
        }
        if !grid.is_empty() {
            let sw = if cw >= 6.0 && ch >= 6.0 { "0.8" } else { "0.55" };
            let _ = writeln!(
                out,
                "    <path d=\"{grid}\" stroke=\"{GRID_STROKE}\" stroke-width=\"{sw}\" fill=\"none\"/>"
            );
        }
    }

    // Outer border for crisp tile separation
    let _ = writeln!(
        out,
        "    <rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" fill=\"none\" stroke=\"{BG}\" stroke-width=\"1\"/>"
    );

    // Tile label (clipped cleanly to tile width)
    if w >= 26.0 && h >= 13.0 {
        let max_chars = ((w - 8.0) / 6.3).floor().max(2.0) as usize;
        let display = truncate_label(name, max_chars);
        let _ = writeln!(
            out,
            "    <text x=\"{:.1}\" y=\"{:.1}\" class=\"lbl\">{}</text>",
            x + 4.0,
            y + 12.5,
            esc(&display)
        );
    }

    let _ = writeln!(out, "  </g>");
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        s.chars().take(max_chars).collect()
    }
}

/// Build grouped wrapped monospace lines for the top unlowered ISA families so the right column matches `anyps5`.
fn build_missing_opcode_lines(families: &[IsaFamily]) -> Vec<(String, Vec<String>)> {
    let mut missing: Vec<(&str, Vec<&str>)> = families
        .iter()
        .map(|f| (f.name.as_str(), f.unlowered_names()))
        .filter(|(_, ops)| !ops.is_empty())
        .collect();
    // Prioritize memory/shader-critical families with the most unlowered ops
    missing.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    let max_chars = 76usize;
    let mut rows = Vec::new();
    for (fam, ops) in missing.into_iter().take(6) {
        let total_missing = ops.len();
        let show_limit = 9usize;
        let mut tokens: Vec<String> = ops
            .iter()
            .take(show_limit)
            .map(|s| s.to_ascii_uppercase())
            .collect();
        if total_missing > show_limit {
            tokens.push(format!("... (+{} more)", total_missing - show_limit));
        }

        let mut lines = Vec::new();
        let mut current = String::new();
        for (i, tok) in tokens.iter().enumerate() {
            let sep = if i + 1 < tokens.len() && !tok.starts_with("...") {
                ", "
            } else {
                ""
            };
            let piece = format!("{tok}{sep}");
            if !current.is_empty() && current.len() + piece.len() > max_chars {
                lines.push(current.trim_end().to_owned());
                current.clear();
            }
            current.push_str(&piece);
        }
        if !current.is_empty() {
            lines.push(current.trim_end().to_owned());
        }
        rows.push((format!("{fam} ({total_missing})"), lines));
    }
    rows
}

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
