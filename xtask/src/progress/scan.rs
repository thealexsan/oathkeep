use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use walkdir::WalkDir;

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
    libraries.extend(scan_hle_libraries(root)?);
    libraries.extend(scan_gpu_com_interfaces(root)?);
    libraries.sort_by(|a, b| b.total().cmp(&a.total()).then_with(|| a.name.cmp(&b.name)));

    let isa_families = scan_rdna2_isa(root)?;
    let unsupported_areas = scan_unsupported_sites(root)?;
    let format_bars = scan_dxgi_formats(root)?;

    Ok(CodeMetrics {
        libraries,
        isa_families,
        unsupported_areas,
        format_bars,
    })
}

/// Strip inline `#[cfg(test)] mod tests { ... }` tail from a Rust source file so tests never skew static counts.
pub(crate) fn strip_tests(src: &str) -> &str {
    for marker in [
        "#[cfg(test)]\nmod tests {",
        "#[cfg(test)]\r\nmod tests {",
        "#[cfg(test)]\nmod tests{",
        "#[cfg(test)]\r\nmod tests{",
    ] {
        if let Some(pos) = src.find(marker) {
            return &src[..pos];
        }
    }
    src
}

fn is_test_source(path: &Path) -> bool {
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if fname == "tests.rs" || fname.ends_with("_tests.rs") {
        return true;
    }
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        == Some("tests")
}

/// Collect all non-test `.rs` files belonging to a Rust module at `base`
/// (accepts `path/to/foo.rs`, `path/to/foo`, or a directory `path/to/dir`).
/// Orders `<foo>.rs` and `<foo>/mod.rs` first, followed by submodules sorted by path.
fn collect_module_files(base: &Path) -> Vec<std::path::PathBuf> {
    let stem_path = if base.extension().and_then(|s| s.to_str()) == Some("rs") {
        base.with_extension("")
    } else {
        base.to_path_buf()
    };
    let file_path = stem_path.with_extension("rs");
    let mut head = Vec::new();
    let mut rest = Vec::new();

    if file_path.is_file() && !is_test_source(&file_path) {
        head.push(file_path);
    }
    if stem_path.is_dir() {
        let mod_rs = stem_path.join("mod.rs");
        for entry in WalkDir::new(&stem_path).into_iter().filter_map(Result::ok) {
            let p = entry.path();
            if !p.is_file()
                || p.extension().and_then(|s| s.to_str()) != Some("rs")
                || is_test_source(p)
            {
                continue;
            }
            if p == mod_rs {
                head.push(p.to_path_buf());
            } else {
                rest.push(p.to_path_buf());
            }
        }
    }
    rest.sort();
    head.extend(rest);
    head.dedup();
    head
}

/// Read and concatenate all non-test `.rs` sources belonging to a module (single file or directory).
fn read_module_sources(base: &Path) -> Result<String> {
    read_module_sources_inner(base, true)
}

fn read_module_sources_raw(base: &Path) -> Result<String> {
    read_module_sources_inner(base, false)
}

fn read_module_sources_inner(base: &Path, strip_inline_tests: bool) -> Result<String> {
    let files = collect_module_files(base);
    if files.is_empty() {
        anyhow::bail!("no Rust sources found for module {}", base.display());
    }
    let mut out = String::new();
    for path in files {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        out.push_str(if strip_inline_tests {
            strip_tests(&raw)
        } else {
            &raw
        });
        out.push('\n');
    }
    Ok(out)
}

fn strip_line_comment(line: &str) -> &str {
    let mut in_str = false;
    let mut escaped = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

fn extract_bracket_body(src: &str, open_pos: usize) -> Option<&str> {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = open_pos;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'[' => depth += 1,
                b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(&src[open_pos + 1..i]);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn extract_brace_body(src: &str, open_pos: usize) -> Option<&str> {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = open_pos;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(&src[open_pos + 1..i]);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn hle_display_name(stem: &str) -> String {
    match stem {
        "apu" => "XApu".into(),
        "audio" => "AcpHal".into(),
        "d3d12x" => "D3D12X".into(),
        "display" => "XDisplay".into(),
        "dsp" => "XDsp".into(),
        "dstorage" => "DirectStorage".into(),
        "files" => "Win32Files".into(),
        "gameinput" => "GameInput".into(),
        "kernel" => "Kernel32".into(),
        "lifecycle" => "Lifecycle".into(),
        "media" => "MFPlat".into(),
        "net" => "WinSockNet".into(),
        "rpcrt4" => "RpcRt4".into(),
        "speech" => "XSpeech".into(),
        "storage" => "LocalStorage".into(),
        "xasync" => "XAsync".into(),
        "xg" => "XGTexture".into(),
        "xgame" => "XGame".into(),
        "xgameruntime" => "XGameRuntime".into(),
        "xgamesave" => "XGameSave".into(),
        "xgameui" => "XGameUI".into(),
        "xmem" => "XMem".into(),
        "xnetworking" => "XNetworking".into(),
        "xpackage" => "XPackage".into(),
        "xstore" => "XStore".into(),
        "xsystem" => "XSystem".into(),
        "xuser" => "XUser".into(),
        other => other.to_owned(),
    }
}

fn scan_hle_libraries(root: &Path) -> Result<Vec<LibraryEntry>> {
    let dir = root.join("engine/hle/src/modules");
    let mut entries = Vec::new();
    let mut stems = BTreeSet::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
    {
        let p = entry.path();
        if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("rs") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && stem != "mod"
            {
                stems.insert(stem.to_owned());
            }
        } else if p.is_dir() && p.join("mod.rs").is_file() {
            if let Some(stem) = p.file_name().and_then(|s| s.to_str()) {
                stems.insert(stem.to_owned());
            }
        }
    }

    for stem in stems {
        let raw = read_module_sources(&dir.join(&stem))?;
        let src = raw.as_str();
        let mut cells = Vec::new();

        // 1. Parse `static ...: Methods<N> = Methods([ ... ]);`, `Table<N> = Table([ ... ]);`, `Vtable([ ... ]);`
        // Skip `#[cfg(not(windows))]` fallbacks so we count the real Windows host layout once.
        let mut offset = 0usize;
        while let Some(rel) = src[offset..].find("static ") {
            let start = offset + rel;
            let line_start = src[..start].rfind('\n').map_or(0, |p| p + 1);
            let prev_region = &src[line_start.saturating_sub(80)..start];
            let is_non_windows = prev_region.contains("#[cfg(not(windows))]");
            let line_end = src[start..].find('\n').map_or(src.len(), |p| start + p);
            let header = &src[start..line_end];

            if !is_non_windows
                && (header.contains("= Methods([")
                    || header.contains("= Table([")
                    || header.contains("= Vtable(["))
                && let Some(bracket_rel) = src[start..].find('[')
            {
                let open = start + bracket_rel;
                if let Some(body) = extract_bracket_body(src, open) {
                    for item in body.split(',') {
                        let clean = item
                            .lines()
                            .map(strip_line_comment)
                            .collect::<Vec<_>>()
                            .join(" ");
                        let trimmed = clean.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let unimpl = trimmed.contains("missing::<")
                            || trimmed.contains("unsupported::<")
                            || trimmed.starts_with("unused ")
                            || trimmed.starts_with("nothing ");
                        cells.push(!unimpl);
                    }
                    offset = open + body.len() + 1;
                    continue;
                }
            }
            offset = line_end;
        }

        // 2. Named-field vtables in `audio.rs` (`AcpMethods`) and `media.rs` (`static METHODS: Methods = Methods { ... }`)
        for marker in [
            "static ACP_METHODS: AcpMethods = AcpMethods {",
            "static METHODS: Methods = Methods {",
        ] {
            if let Some(pos) = src.find(marker)
                && let Some(brace_rel) = src[pos..].find('{')
                && let Some(body) = extract_brace_body(src, pos + brace_rel)
            {
                for line in body.lines() {
                    let clean = strip_line_comment(line).trim();
                    if clean.contains(':') && !clean.is_empty() {
                        cells.push(!clean.contains("missing") && !clean.contains("E_NOTIMPL"));
                    }
                }
            }
        }

        // 3. Exported functions in `fn module() -> HleModule` (`hle_export!(Fn)`, `HleExport { ... }`, `ordinal(...)`)
        if let Some(mod_pos) = src.find("fn module() -> HleModule")
            && let Some(brace_rel) = src[mod_pos..].find('{')
            && let Some(body) = extract_brace_body(src, mod_pos + brace_rel)
        {
            for line in body.lines() {
                let clean = strip_line_comment(line).trim();
                if let Some(idx) = clean.find("hle_export!(") {
                    let rest = &clean[idx + "hle_export!(".len()..];
                    if let Some(end) = rest.find(')') {
                        let fn_name = rest[..end].trim();
                        let is_stub = is_function_notimpl_stub(src, fn_name);
                        cells.push(!is_stub);
                    }
                } else if clean.contains("HleExport {") {
                    cells.push(true);
                } else if clean.starts_with("ordinal(") {
                    cells.push(true);
                }
            }
        }

        if !cells.is_empty() {
            entries.push(LibraryEntry {
                name: hle_display_name(&stem),
                cells,
            });
        }
    }

    Ok(entries)
}

/// Returns true only when an exported function directly returns `E_NOTIMPL` as an unimplemented stub.
fn is_function_notimpl_stub(src: &str, fn_name: &str) -> bool {
    let needle = format!("fn {fn_name}(");
    let Some(pos) = src.find(&needle) else {
        return false;
    };
    let Some(brace_rel) = src[pos..].find('{') else {
        return false;
    };
    let Some(body) = extract_brace_body(src, pos + brace_rel) else {
        return false;
    };
    let trimmed = body
        .lines()
        .map(strip_line_comment)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    // Only mark as stub if the body is essentially a trivial E_NOTIMPL return (no real logic).
    trimmed.len() < 160 && trimmed.contains("E_NOTIMPL") && !trimmed.contains("S_OK")
}

fn scan_gpu_com_interfaces(root: &Path) -> Result<Vec<LibraryEntry>> {
    let gpu_dir = root.join("engine/gpu/src");
    let specs: &[(&str, &str, &str)] = &[
        ("device.rs", "ID3D12Device", "static FUNCTIONS: Table<"),
        ("list.rs", "ID3D12CommandList", "const fn list_table("),
        ("list.rs", "ID3D12Allocator", "static ALLOCATOR_FUNCTIONS: Table<"),
        ("queue.rs", "ID3D12CommandQueue", "const fn queue_table("),
        ("resource.rs", "ID3D12Resource", "static RESOURCE_FUNCTIONS: Table<"),
        ("resource.rs", "ID3D12Heap", "static HEAP_FUNCTIONS: Table<"),
        ("fence.rs", "ID3D12Fence", "static FUNCTIONS: Table<"),
        ("descriptor.rs", "ID3D12DescHeap", "static FUNCTIONS: Table<"),
        ("root.rs", "ID3D12RootSig", "static FUNCTIONS: Table<"),
        ("query.rs", "ID3D12QueryHeap", "static FUNCTIONS: Table<"),
        ("signature.rs", "ID3D12CmdSig", "static FUNCTIONS: Table<"),
        ("pipeline.rs", "ID3D12Pipeline", "static FUNCTIONS: Table<"),
        ("dxgi.rs", "IDXGIDevice", "static DEVICE_FUNCTIONS: Table<"),
        ("dxgi.rs", "IDXGIAdapter", "static ADAPTER_FUNCTIONS: Table<"),
        ("dxgi.rs", "IDXGIFactory", "static FACTORY_FUNCTIONS: Table<"),
        ("dxgi.rs", "IDXGIOutput", "static OUTPUT_FUNCTIONS: Table<"),
    ];

    let mut file_cache: BTreeMap<&str, String> = BTreeMap::new();
    let mut entries = Vec::new();

    for &(file, label, anchor) in specs {
        let src = if let Some(cached) = file_cache.get(file) {
            cached.as_str()
        } else {
            let text = read_module_sources(&gpu_dir.join(file))?;
            file_cache.insert(file, text);
            file_cache.get(file).expect("just inserted").as_str()
        };
        let Some(pos) = src.find(anchor) else {
            continue;
        };
        let Some(brace_rel) = src[pos..].find('{') else {
            continue;
        };
        let Some(body) = extract_brace_body(src, pos + brace_rel) else {
            continue;
        };

        // Determine total slot count N from `missing_table::<..., N>()`
        let Some(n) = parse_missing_table_len(body) else {
            continue;
        };
        let mut slots = vec![false; n];
        // `missing_table` populates AddRef (1) and Release (2).
        if n > 2 {
            slots[1] = true;
            slots[2] = true;
        }
        // `d3d12_object` populates GetPrivateData (3), SetPrivateData (4), SetName (6).
        if body.contains("d3d12_object(") && n > 6 {
            slots[3] = true;
            slots[4] = true;
            slots[6] = true;
        }

        // Direct `f[idx] = ...;` assignments inside the block.
        for line in body.lines() {
            let clean = strip_line_comment(line).trim();
            if let Some(rest) = clean.strip_prefix("f[")
                && let Some((idx_str, rhs)) = rest.split_once("] =")
                && let Ok(idx) = idx_str.trim().parse::<usize>()
                && idx < n
            {
                let _ = rhs;
                slots[idx] = true;
            }
        }

        // Tuple method tables (`METHODS`, `MORE_METHODS`, `NEWER_METHODS`) used by `queue_table` and `list_table`.
        if anchor == "const fn queue_table(" || anchor == "const fn list_table(" {
            for arr_name in ["const METHODS:", "const MORE_METHODS:", "const NEWER_METHODS:"] {
                if let Some(apos) = src.find(arr_name)
                    && let Some(eq_rel) = src[apos..].find("= [")
                    && let Some(arr_body) = extract_bracket_body(src, apos + eq_rel + 2)
                {
                    for line in arr_body.lines() {
                        let clean = strip_line_comment(line).trim();
                        if let Some(rest) = clean.strip_prefix('(')
                            && let Some((idx_str, _)) = rest.split_once(',')
                            && let Ok(idx) = idx_str.trim().parse::<usize>()
                            && idx < n
                        {
                            slots[idx] = true;
                        }
                    }
                }
            }
        }

        entries.push(LibraryEntry {
            name: label.to_owned(),
            cells: slots,
        });
    }

    Ok(entries)
}

fn parse_missing_table_len(body: &str) -> Option<usize> {
    let pos = body.find("missing_table::<")?;
    let rest = &body[pos + "missing_table::<".len()..];
    let close = rest.find(">()")?;
    let inner = &rest[..close];
    let (_, n_str) = inner.rsplit_once(',')?;
    n_str.trim().parse::<usize>().ok()
}

fn scan_rdna2_isa(root: &Path) -> Result<Vec<IsaFamily>> {
    let shader_src = root.join("engine/shader/src");
    let opcodes_src = read_module_sources(&shader_src.join("gfx10/opcodes"))?;
    let gfx10_mod = read_module_sources(&shader_src.join("gfx10/mod.rs"))?;
    let ir_raw = read_module_sources(&shader_src.join("ir"))?;
    let lower_raw = read_module_sources(&shader_src.join("emit/lower"))?;
    let intersect_raw = if collect_module_files(&shader_src.join("emit/intersect")).is_empty() {
        String::new()
    } else {
        read_module_sources(&shader_src.join("emit/intersect"))?
    };

    let ir_src = ir_raw.as_str();
    let lower_src = lower_raw.as_str();
    let intersect_src = intersect_raw.as_str();

    // 1. Parse all 19 opcode tables from `gfx10/opcodes.rs`
    let mut raw_families: Vec<(String, Vec<String>)> = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = opcodes_src[pos..].find("pub(super) const ") {
        let start = pos + rel + "pub(super) const ".len();
        let Some(colon) = opcodes_src[start..].find(':') else {
            break;
        };
        let fam_name = opcodes_src[start..start + colon].trim().to_owned();
        let Some(eq_bracket) = opcodes_src[start + colon..].find("= &[") else {
            break;
        };
        let open = start + colon + eq_bracket + 3;
        let Some(body) = extract_bracket_body(&opcodes_src, open) else {
            break;
        };
        let mut ops = Vec::new();
        for line in body.lines() {
            let clean = strip_line_comment(line);
            if let Some(q1) = clean.find('"') {
                let after = &clean[q1 + 1..];
                if let Some(q2) = after.find('"') {
                    ops.push(after[..q2].to_owned());
                }
            }
        }
        raw_families.push((fam_name, ops));
        pos = open + body.len() + 1;
    }
    // Plus `EXP` which is decoded inline in `gfx10/mod.rs` (`Encoding::Exp => return Some("exp")`).
    if gfx10_mod.contains("Encoding::Exp") {
        raw_families.push(("EXP".into(), vec!["exp".into()]));
    }

    // 2. Collect exact string literals from `lower.rs` (excluding explicit error/rejection lines) + `intersect.rs`
    let lower_literals = collect_lowered_literals(lower_src, intersect_src);
    let ir_sopp_accepted = collect_sopp_accepted(ir_src);
    let packed16_ops = collect_match_string_arms(lower_src, "fn packed16(");
    let ds_atomic_ops = collect_match_string_arms(lower_src, "let rmw = match op.as_str()");
    let buf_atomic_ops = collect_match_string_arms(lower_src, "match name.strip_prefix(\"buffer_atomic_\")");
    let img_atomic_ops = collect_match_string_arms(lower_src, "if let Some(op) = name.strip_prefix(\"image_atomic_\")");
    let glob_atomic_ops = collect_match_string_arms(lower_src, "let simple = match op");
    let glob_load_suffixes = collect_match_string_arms(lower_src, "let (n, sub) = match rest");
    let glob_store_suffixes = collect_match_string_arms(lower_src, "let count = |rest: &str| match rest");

    // 3. Evaluate decoded and lowered flags per opcode per family
    let mut families = Vec::new();
    for (fam, ops) in raw_families {
        let fam_decoded = fam == "EXP" || gfx10_mod.contains(&format!("t::{fam}"));
        let decoded = vec![fam_decoded; ops.len()];
        let lowered = ops
            .iter()
            .map(|op| {
                is_opcode_lowered(
                    &fam,
                    op,
                    &lower_literals,
                    &ir_sopp_accepted,
                    &packed16_ops,
                    &ds_atomic_ops,
                    &buf_atomic_ops,
                    &img_atomic_ops,
                    &glob_atomic_ops,
                    &glob_load_suffixes,
                    &glob_store_suffixes,
                )
            })
            .collect();
        families.push(IsaFamily {
            name: fam,
            opcodes: ops,
            decoded,
            lowered,
        });
    }

    Ok(families)
}

fn collect_string_literals_in_slice(slice: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for line in slice.lines() {
        let clean = strip_line_comment(line);
        let mut rest = clean;
        while let Some(q1) = rest.find('"') {
            let after = &rest[q1 + 1..];
            let Some(q2) = after.find('"') else { break };
            let lit = &after[..q2];
            if !lit.is_empty() && lit.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                set.insert(lit.to_owned());
            }
            rest = &after[q2 + 1..];
        }
    }
    set
}

fn collect_lowered_literals(lower_src: &str, intersect_src: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for src in [lower_src, intersect_src] {
        for line in src.lines() {
            let clean = strip_line_comment(line).trim();
            // Skip explicit rejection lines & diagnostic strings
            if clean.starts_with("return Err(")
                || clean.starts_with("Err(")
                || clean.contains("Val::Missing(")
                || clean.contains(".op(")
            {
                continue;
            }
            set.extend(collect_string_literals_in_slice(clean));
        }
    }
    // Remove explicitly rejected SOP/IR opcodes even though their literal names appear in the rejection `matches!`.
    for rejected in [
        "s_setreg_imm32_b32",
        "s_setreg_b32",
        "s_swappc_b64",
        "s_version",
        "s_call_b64",
        "s_rfe_b64",
    ] {
        set.remove(rejected);
    }
    set
}

fn collect_sopp_accepted(ir_src: &str) -> HashSet<String> {
    let Some(pos) = ir_src.find("Fields::Sopp { .. } => match name {") else {
        return HashSet::new();
    };
    let Some(brace_rel) = ir_src[pos..].find("match name {") else {
        return HashSet::new();
    };
    let open = pos + brace_rel + "match name ".len();
    let Some(body) = extract_brace_body(ir_src, open) else {
        return HashSet::new();
    };
    collect_string_literals_in_slice(body)
        .into_iter()
        .filter(|s| s.starts_with("s_"))
        .collect()
}

fn collect_match_string_arms(src: &str, anchor: &str) -> HashSet<String> {
    let Some(pos) = src.find(anchor) else {
        return HashSet::new();
    };
    let Some(brace_rel) = src[pos..].find('{') else {
        return HashSet::new();
    };
    let Some(body) = extract_brace_body(src, pos + brace_rel) else {
        return HashSet::new();
    };
    let mut out = HashSet::new();
    for line in body.lines() {
        let clean = strip_line_comment(line).trim();
        if clean.contains("return Err(") {
            continue;
        }
        // Only capture literals before `=>` (match patterns)
        if let Some((lhs, _)) = clean.split_once("=>") {
            out.extend(collect_string_literals_in_slice(lhs));
        } else if clean.starts_with('|') || clean.starts_with('"') {
            out.extend(collect_string_literals_in_slice(clean));
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn is_opcode_lowered(
    family: &str,
    op: &str,
    lower_literals: &HashSet<String>,
    ir_sopp_accepted: &HashSet<String>,
    packed16_ops: &HashSet<String>,
    ds_atomic_ops: &HashSet<String>,
    buf_atomic_ops: &HashSet<String>,
    img_atomic_ops: &HashSet<String>,
    glob_atomic_ops: &HashSet<String>,
    glob_load_suffixes: &HashSet<String>,
    glob_store_suffixes: &HashSet<String>,
) -> bool {
    match family {
        "EXP" => true,
        // `Fields::Flat` in `lower.rs:6219-6220` only handles `global_*`; `flat_*` and `scratch_*` return `Err("flat memory")`.
        "FLAT" | "SCRATCH" => false,
        "SOPP" => ir_sopp_accepted.contains(op),
        "SOP1" | "SOP2" | "SOPK" | "SOPC" => {
            if op.starts_with("s_movrel")
                || op.starts_with("s_subvector_loop")
                || op.starts_with("s_waitcnt")
                || matches!(
                    op,
                    "s_setreg_imm32_b32" | "s_setreg_b32" | "s_swappc_b64" | "s_version" | "s_call_b64" | "s_rfe_b64"
                )
            {
                return false;
            }
            if op.starts_with("s_bitcmp") {
                return true;
            }
            if let Some(rest) = op.strip_prefix("s_cmp_") {
                if rest.ends_with("_u64") {
                    return rest.starts_with("eq") || rest.starts_with("lg");
                }
                if rest.ends_with("_i32") || rest.ends_with("_u32") {
                    return matches!(&rest[..2], "eq" | "lg" | "gt" | "ge" | "lt" | "le");
                }
                return false;
            }
            if let Some(rest) = op.strip_prefix("s_cmpk_") {
                return (rest.ends_with("_i32") || rest.ends_with("_u32"))
                    && matches!(&rest[..2], "eq" | "lg" | "gt" | "ge" | "lt" | "le");
            }
            lower_literals.contains(op)
        }
        "SMEM" => {
            matches!(
                op,
                "s_load_dword"
                    | "s_load_dwordx2"
                    | "s_load_dwordx4"
                    | "s_load_dwordx8"
                    | "s_load_dwordx16"
                    | "s_buffer_load_dword"
                    | "s_buffer_load_dwordx2"
                    | "s_buffer_load_dwordx4"
                    | "s_buffer_load_dwordx8"
                    | "s_buffer_load_dwordx16"
            )
        }
        "VINTRP" => lower_literals.contains(op) || op.starts_with("v_interp_"),
        "VOPC" => {
            let Some(rest) = op
                .strip_prefix("v_cmp_")
                .or_else(|| op.strip_prefix("v_cmpx_"))
            else {
                return lower_literals.contains(op);
            };
            let Some((pred, ty)) = rest.rsplit_once('_') else {
                return false;
            };
            match ty {
                "f32" | "f16" => {
                    pred == "class"
                        || matches!(
                            pred,
                            "f" | "lt"
                                | "eq"
                                | "le"
                                | "gt"
                                | "lg"
                                | "ge"
                                | "o"
                                | "u"
                                | "nge"
                                | "nlg"
                                | "ngt"
                                | "nle"
                                | "neq"
                                | "nlt"
                                | "tru"
                        )
                }
                "u64" | "i64" => matches!(pred, "eq" | "ne" | "lg" | "lt" | "le" | "gt" | "ge"),
                "u32" | "i32" | "u16" | "i16" => {
                    matches!(pred, "eq" | "ne" | "lg" | "lt" | "le" | "gt" | "ge" | "f" | "t")
                }
                _ => false,
            }
        }
        "VOP3P" => {
            if let Some(rest) = op.strip_prefix("v_pk_") {
                packed16_ops.contains(rest)
            } else {
                lower_literals.contains(op)
            }
        }
        "VOP1" | "VOP2" | "VOP3" => {
            if op.starts_with("v_interp_") {
                return true;
            }
            lower_literals.contains(op)
        }
        "DS" => {
            if op == "ds_swizzle_b32" || op == "ds_bpermute_b32" {
                return true;
            }
            let base = op.trim_end_matches("_hi").trim_end_matches("_d16");
            if matches!(
                base,
                "ds_read_u8" | "ds_read_i8" | "ds_read_u16" | "ds_read_i16" | "ds_write_b8" | "ds_write_b16"
            ) && (base == op
                || (op.ends_with("_d16") && base.starts_with("ds_read"))
                || op.ends_with("_d16_hi"))
            {
                return true;
            }
            if let Some(rest) = op.strip_prefix("ds_read")
                && (rest.starts_with('2') || matches!(rest, "_b32" | "_b64" | "_b96" | "_b128"))
            {
                return true;
            }
            if let Some(rest) = op.strip_prefix("ds_write")
                && (rest.starts_with('2') || matches!(rest, "_b32" | "_b64" | "_b96" | "_b128"))
            {
                return true;
            }
            if let Some(rest) = op.strip_prefix("ds_") {
                let norm = rest.replace("_rtn", "");
                return ds_atomic_ops.contains(&norm);
            }
            false
        }
        "MUBUF" => {
            if op == "buffer_gl0_inv" || op == "buffer_gl1_inv" {
                return true;
            }
            if let Some(atom) = op.strip_prefix("buffer_atomic_") {
                return buf_atomic_ops.contains(atom);
            }
            if matches!(
                op,
                "buffer_load_ubyte"
                    | "buffer_load_sbyte"
                    | "buffer_load_ushort"
                    | "buffer_load_sshort"
                    | "buffer_store_byte"
                    | "buffer_store_short"
            ) {
                return true;
            }
            let plain = op.replace("_format_d16_", "_format_");
            if let Some(rest) = plain
                .strip_prefix("buffer_load_")
                .or_else(|| plain.strip_prefix("buffer_store_"))
            {
                return matches!(
                    rest,
                    "dword"
                        | "dwordx2"
                        | "dwordx3"
                        | "dwordx4"
                        | "format_x"
                        | "format_xy"
                        | "format_xyz"
                        | "format_xyzw"
                );
            }
            false
        }
        "MTBUF" => {
            let plain = op.replace("_format_d16_", "_format_");
            plain
                .strip_prefix("tbuffer_load_")
                .is_some_and(|rest| {
                    matches!(rest, "format_x" | "format_xy" | "format_xyz" | "format_xyzw")
                })
        }
        "MIMG" => {
            if op == "image_bvh_intersect_ray" || op == "image_bvh64_intersect_ray" {
                return true;
            }
            if let Some(atom) = op.strip_prefix("image_atomic_") {
                return img_atomic_ops.contains(atom);
            }
            lower_literals.contains(op)
        }
        "GLOBAL" => {
            if let Some(rest) = op.strip_prefix("global_load_") {
                return glob_load_suffixes.contains(rest);
            }
            if let Some(rest) = op.strip_prefix("global_store_") {
                return glob_store_suffixes.contains(rest);
            }
            if let Some(atom) = op.strip_prefix("global_atomic_") {
                return glob_atomic_ops.contains(atom);
            }
            false
        }
        _ => lower_literals.contains(op),
    }
}

fn scan_unsupported_sites(root: &Path) -> Result<Vec<AreaCount>> {
    let buckets: &[(&str, &[&str])] = &[
        ("Shader / Emit & Lower", &["engine/shader/src/emit"]),
        ("Shader / Resource & IR", &[
            "engine/shader/src/resource",
            "engine/shader/src/ir",
            "engine/shader/src/io",
            "engine/shader/src/structure",
        ]),
        ("HLE / Runtime Stubs", &["engine/hle/src"]),
        ("GPU / D3D12.x Host", &["engine/gpu/src"]),
        ("Formats / Tiling & Audio", &["engine/formats/src"]),
    ];

    let mut out = Vec::new();
    for &(area, paths) in buckets {
        let mut count = 0usize;
        let mut seen = BTreeSet::new();
        for rel in paths {
            for p in collect_module_files(&root.join(rel)) {
                if seen.insert(p.clone()) {
                    count += count_unsupported_in_file(&p)?;
                }
            }
        }
        if count > 0 {
            out.push(AreaCount {
                name: area.to_owned(),
                count,
            });
        }
    }
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

fn count_unsupported_in_file(path: &Path) -> Result<usize> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let src = strip_tests(&raw);
    let mut n = 0usize;
    for line in src.lines() {
        let clean = strip_line_comment(line).trim();
        if clean.is_empty() {
            continue;
        }
        // Skip constant/enum definitions
        if clean.starts_with("pub(crate) const E_NOTIMPL")
            || clean.starts_with("pub const E_NOTIMPL")
            || clean.starts_with("const E_NOTIMPL")
            || clean.starts_with("Unsupported {")
            || clean.starts_with("Unsupported,")
            || clean.starts_with("use ")
        {
            continue;
        }
        if clean.contains("ShaderError::Unsupported")
            || clean.contains("Error::Unsupported")
            || clean.contains("Err(unsupported(")
            || clean.contains("bail!(")
            || clean.contains("E_NOTIMPL")
            || clean.contains("missing::<")
            || clean.contains("unsupported::<")
        {
            n += 1;
        }
    }
    Ok(n)
}

fn scan_dxgi_formats(root: &Path) -> Result<Vec<FormatBar>> {
    // Full DXGI console format catalog: standard 1..=115 (115) + console extensions 116..=120 (5) + 189..=191 (3) = 123.
    const TOTAL_DXGI: usize = 123;

    let res_raw = read_module_sources(&root.join("engine/gpu/src/resource"))?;
    let tex_raw = read_module_sources(&root.join("engine/formats/src/texture"))?;
    let host_raw = read_module_sources_raw(&root.join("engine/gpu/src/host"))?;

    let (elem_all, elem_uncompressed) = parse_resource_element_formats(&res_raw);
    let tex_tiling = parse_match_u32_set(&tex_raw, "pub fn format(dxgi: u32)");
    let swap_fmts = parse_match_u32_set(&host_raw, "fn swap_format(");

    // Typed buffer views in `descriptor/host.rs` accept non-block-compressed non-typeless formats from `resource::element`.
    // Count exact uncompressed formats parsed from `resource::element` (block == 1).
    let typed_count = elem_uncompressed.len().min(TOTAL_DXGI);

    Ok(vec![
        FormatBar {
            name: "Sampled & Element".into(),
            supported: elem_all.len().min(TOTAL_DXGI),
            total: TOTAL_DXGI,
        },
        FormatBar {
            name: "Console Tiling".into(),
            supported: tex_tiling.len().min(TOTAL_DXGI),
            total: TOTAL_DXGI,
        },
        FormatBar {
            name: "Typed Buffer".into(),
            supported: typed_count,
            total: TOTAL_DXGI,
        },
        FormatBar {
            name: "Swapchain Present".into(),
            supported: swap_fmts.len().min(TOTAL_DXGI),
            total: TOTAL_DXGI,
        },
    ])
}

fn parse_resource_element_formats(src: &str) -> (BTreeSet<u32>, BTreeSet<u32>) {
    let mut all = BTreeSet::new();
    let mut uncompressed = BTreeSet::new();
    let Some(pos) = src.find("pub(crate) fn element(format: u32)") else {
        return (all, uncompressed);
    };
    let Some(brace_rel) = src[pos..].find('{') else {
        return (all, uncompressed);
    };
    let Some(body) = extract_brace_body(src, pos + brace_rel) else {
        return (all, uncompressed);
    };
    for line in body.lines() {
        let clean = strip_line_comment(line).trim();
        let Some((lhs, rhs)) = clean.split_once("=>") else {
            continue;
        };
        if rhs.contains("return None") {
            continue;
        }
        let ids = parse_int_pattern_list(lhs);
        let is_uncompressed = rhs.contains(", 1)");
        for id in ids {
            all.insert(id);
            if is_uncompressed {
                uncompressed.insert(id);
            }
        }
    }
    (all, uncompressed)
}

fn parse_match_u32_set(src: &str, fn_anchor: &str) -> BTreeSet<u32> {
    let mut set = BTreeSet::new();
    let Some(pos) = src.find(fn_anchor) else {
        return set;
    };
    let Some(brace_rel) = src[pos..].find('{') else {
        return set;
    };
    let Some(body) = extract_brace_body(src, pos + brace_rel) else {
        return set;
    };
    for line in body.lines() {
        let clean = strip_line_comment(line).trim();
        let Some((lhs, rhs)) = clean.split_once("=>") else {
            continue;
        };
        if rhs.contains("return None") {
            continue;
        }
        for id in parse_int_pattern_list(lhs) {
            set.insert(id);
        }
    }
    set
}

fn parse_int_pattern_list(lhs: &str) -> Vec<u32> {
    let mut ids = Vec::new();
    for part in lhs.split('|') {
        let p = part.trim();
        if let Some((a, b)) = p.split_once("..=") {
            if let (Ok(start), Ok(end)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                ids.extend(start..=end);
            }
        } else if let Some((a, b)) = p.split_once("..") {
            if let (Ok(start), Ok(end)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                ids.extend(start..end);
            }
        } else if let Ok(v) = p.parse::<u32>() {
            ids.push(v);
        }
    }
    ids
}
