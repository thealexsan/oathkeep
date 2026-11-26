/// Squarified treemap layout (Bruls, Huizing, van Wijk 2000).

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone)]
pub struct Tile {
    pub index: usize,
    pub rect: Rect,
}

/// Lay out `weights` (each > 0, paired with its original index) inside `bounds`
/// using the squarified treemap algorithm.
pub fn squarify(bounds: Rect, items: &[(usize, usize)]) -> Vec<Tile> {
    let mut sorted: Vec<(usize, f64)> = items
        .iter()
        .filter(|&&(_, w)| w > 0)
        .map(|&(idx, w)| (idx, w as f64))
        .collect();
    if sorted.is_empty() || bounds.w <= 0.0 || bounds.h <= 0.0 {
        return Vec::new();
    }
    sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let total_weight: f64 = sorted.iter().map(|(_, w)| *w).sum();
    let area = bounds.w * bounds.h;
    let scale = area / total_weight;
    let scaled: Vec<(usize, f64)> = sorted.into_iter().map(|(i, w)| (i, w * scale)).collect();

    let mut out = Vec::with_capacity(scaled.len());
    let mut rem = bounds;
    let mut row: Vec<(usize, f64)> = Vec::new();
    let mut i = 0;

    while i < scaled.len() {
        let short = rem.w.min(rem.h).max(1e-6);
        let candidate = scaled[i];
        if row.is_empty() {
            row.push(candidate);
            i += 1;
            continue;
        }
        let cur_worst = worst_ratio(&row, short);
        let mut next_row = row.clone();
        next_row.push(candidate);
        let next_worst = worst_ratio(&next_row, short);

        if next_worst <= cur_worst {
            row.push(candidate);
            i += 1;
        } else {
            rem = layout_row(&row, rem, &mut out);
            row.clear();
        }
    }
    if !row.is_empty() {
        layout_row(&row, rem, &mut out);
    }

    out
}

fn worst_ratio(row: &[(usize, f64)], side: f64) -> f64 {
    let sum: f64 = row.iter().map(|(_, a)| *a).sum();
    if sum <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let s2 = sum * sum;
    let side2 = side * side;
    let mut max_a = 0.0f64;
    let mut min_a = f64::INFINITY;
    for &(_, a) in row {
        max_a = max_a.max(a);
        min_a = min_a.min(a);
    }
    if min_a <= 0.0 {
        return f64::INFINITY;
    }
    let r1 = (side2 * max_a) / s2;
    let r2 = s2 / (side2 * min_a);
    r1.max(r2)
}

fn layout_row(row: &[(usize, f64)], rem: Rect, out: &mut Vec<Tile>) -> Rect {
    let sum: f64 = row.iter().map(|(_, a)| *a).sum();
    if rem.w >= rem.h {
        // Place a vertical strip on the left edge.
        let strip_w = if rem.h > 0.0 {
            (sum / rem.h).min(rem.w)
        } else {
            0.0
        };
        let mut y = rem.y;
        for &(idx, a) in row {
            let h = if strip_w > 0.0 { a / strip_w } else { 0.0 };
            out.push(Tile {
                index: idx,
                rect: Rect {
                    x: rem.x,
                    y,
                    w: strip_w,
                    h,
                },
            });
            y += h;
        }
        Rect {
            x: rem.x + strip_w,
            y: rem.y,
            w: (rem.w - strip_w).max(0.0),
            h: rem.h,
        }
    } else {
        // Place a horizontal strip along the top edge.
        let strip_h = if rem.w > 0.0 {
            (sum / rem.w).min(rem.h)
        } else {
            0.0
        };
        let mut x = rem.x;
        for &(idx, a) in row {
            let w = if strip_h > 0.0 { a / strip_h } else { 0.0 };
            out.push(Tile {
                index: idx,
                rect: Rect {
                    x,
                    y: rem.y,
                    w,
                    h: strip_h,
                },
            });
            x += w;
        }
        Rect {
            x: rem.x,
            y: rem.y + strip_h,
            w: rem.w,
            h: (rem.h - strip_h).max(0.0),
        }
    }
}

/// Choose `(cols, rows)` for laying out `count` micro-cells inside `(w, h)` so each cell is as close to square as possible.
pub fn grid_dims(count: usize, w: f64, h: f64) -> (usize, usize) {
    if count <= 1 || w <= 0.0 || h <= 0.0 {
        return (1, 1);
    }
    let aspect = (w / h).clamp(0.05, 20.0);
    let ideal_cols = ((count as f64) * aspect).sqrt();
    let mut best = (1usize, count);
    let mut best_score = f64::INFINITY;

    let min_c = (ideal_cols.floor() as usize).saturating_sub(2).max(1);
    let max_c = ((ideal_cols.ceil() as usize) + 3).min(count);
    for cols in min_c..=max_c {
        let rows = count.div_ceil(cols);
        let cw = w / cols as f64;
        let ch = h / rows as f64;
        let ratio = (cw / ch).max(ch / cw);
        let waste = (cols * rows - count) as f64 / count as f64;
        let score = ratio + waste * 0.35;
        if score < best_score {
            best_score = score;
            best = (cols, rows);
        }
    }
    best
}
