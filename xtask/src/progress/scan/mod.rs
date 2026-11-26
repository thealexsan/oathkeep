mod gpu;
mod hle;
mod isa;
mod text;

use std::path::Path;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct LibraryEntry {
    pub name: String,
    /// Ordered per-export / per-slot implementation flags (true = implemented, false = missing/stub).
    pub cells: Vec<bool>,
}

impl LibraryEntry {
    pub fn implemented(&self) -> usize {
        self.cells.iter().filter(|&&b| b).count()
    }

    pub fn total(&self) -> usize {
        self.cells.len()
    }
}

#[derive(Debug, Clone)]
pub struct IsaFamily {
    pub name: String,
    pub opcodes: Vec<String>,
    /// Same length as `opcodes`: whether decoded by `gfx10`.
    pub decoded: Vec<bool>,
    /// Same length as `opcodes`: whether lowered to DXIL by `emit/lower.rs` + `intersect.rs` (and accepted by `ir.rs`).
    pub lowered: Vec<bool>,
}

impl IsaFamily {
    pub fn total(&self) -> usize {
        self.opcodes.len()
    }

    pub fn decoded_count(&self) -> usize {
        self.decoded.iter().filter(|&&b| b).count()
    }

    pub fn lowered_count(&self) -> usize {
        self.lowered.iter().filter(|&&b| b).count()
    }

    pub fn unlowered_names(&self) -> Vec<&str> {
        self.opcodes
            .iter()
            .zip(&self.lowered)
            .filter(|(_, l)| !**l)
            .map(|(n, _)| n.as_str())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct AreaCount {
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct FormatBar {
    pub name: String,
    pub supported: usize,
    pub total: usize,
}

#[derive(Debug, Clone)]
pub struct CodeMetrics {
    pub libraries: Vec<LibraryEntry>,
    pub isa_families: Vec<IsaFamily>,
    pub unsupported_areas: Vec<AreaCount>,
    pub format_bars: Vec<FormatBar>,
}

impl CodeMetrics {
    pub fn libraries_totals(&self) -> (usize, usize) {
        let imp = self.libraries.iter().map(LibraryEntry::implemented).sum();
        let tot = self.libraries.iter().map(LibraryEntry::total).sum();
        (imp, tot)
    }

    pub fn isa_decoded_totals(&self) -> (usize, usize) {
        let imp = self.isa_families.iter().map(IsaFamily::decoded_count).sum();
        let tot = self.isa_families.iter().map(IsaFamily::total).sum();
        (imp, tot)
    }

    pub fn isa_lowered_totals(&self) -> (usize, usize) {
        let imp = self.isa_families.iter().map(IsaFamily::lowered_count).sum();
        let tot = self.isa_families.iter().map(IsaFamily::total).sum();
        (imp, tot)
    }

    pub fn unsupported_total(&self) -> usize {
        self.unsupported_areas.iter().map(|a| a.count).sum()
    }
}

pub fn scan_codebase(root: &Path) -> Result<CodeMetrics> {
    let mut libraries = Vec::new();
    libraries.extend(hle::scan_hle_libraries(root)?);
    libraries.extend(gpu::scan_gpu_com_interfaces(root)?);
    libraries.sort_by(|a, b| b.total().cmp(&a.total()).then_with(|| a.name.cmp(&b.name)));

    let isa_families = isa::scan_rdna2_isa(root)?;
    let unsupported_areas = gpu::scan_unsupported_sites(root)?;
    let format_bars = gpu::scan_dxgi_formats(root)?;

    Ok(CodeMetrics {
        libraries,
        isa_families,
        unsupported_areas,
        format_bars,
    })
}
