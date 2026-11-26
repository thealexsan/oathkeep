mod html_json;
mod svg;
mod svg_tiles;

pub use html_json::{render_html, render_json};
pub use svg::render_svg;

pub(super) const BG: &str = "#0d0f12";
pub(super) const GREEN: &str = "#2fa343";
pub(super) const GRAY: &str = "#5a6068";
pub(super) const GOLD: &str = "#c98f24";
pub(super) const TRACK: &str = "#4a5058";
pub(super) const GRID_STROKE: &str = "#14181d";
pub(super) const TEXT_MAIN: &str = "#f0f3f6";
pub(super) const TEXT_SUB: &str = "#c8ced6";
pub(super) const TEXT_MUTED: &str = "#8b949e";

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
