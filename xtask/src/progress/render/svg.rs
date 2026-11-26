use std::fmt::Write as _;

use super::super::ProgressData;
use super::super::scan::{CodeMetrics, IsaFamily, LibraryEntry};
use super::super::treemap::{Rect, squarify};
use super::svg_tiles::{build_missing_opcode_lines, render_cell_tile};
use super::{BG, GOLD, GREEN, TEXT_MAIN, TEXT_MUTED, TEXT_SUB, TRACK, esc, pct};

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
