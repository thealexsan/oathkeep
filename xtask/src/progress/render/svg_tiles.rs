use std::fmt::Write as _;

use super::super::scan::IsaFamily;
use super::super::treemap::{Rect, grid_dims};
use super::{BG, GRAY, GREEN, GRID_STROKE, esc, pct};

/// Renders one treemap tile with exact per-slot green/gray micro-squares (compressed with horizontal RLE runs
/// plus a single grid-line `<path>`) so the SVG stays tiny (< 80 KB) while preserving exact slot positions.
pub(super) fn render_cell_tile(out: &mut String, name: &str, cells: &[bool], rect: Rect) {
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
pub(super) fn build_missing_opcode_lines(families: &[IsaFamily]) -> Vec<(String, Vec<String>)> {
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
