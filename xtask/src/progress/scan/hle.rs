use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::LibraryEntry;
use super::text::{extract_brace_body, extract_bracket_body, read_module_sources, strip_line_comment};

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

pub(super) fn scan_hle_libraries(root: &Path) -> Result<Vec<LibraryEntry>> {
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
