use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::text::{
    collect_module_files, extract_brace_body, extract_bracket_body, read_module_sources,
    read_module_sources_raw, strip_line_comment, strip_tests,
};
use super::{AreaCount, FormatBar, LibraryEntry};

pub(super) fn scan_gpu_com_interfaces(root: &Path) -> Result<Vec<LibraryEntry>> {
    let gpu_dir = root.join("engine/gpu/src");
    let specs: &[(&str, &str, &str)] = &[
        ("d3d12/device", "ID3D12Device", "static FUNCTIONS: Table<"),
        ("d3d12/command_list", "ID3D12CommandList", "const fn list_table("),
        ("d3d12/command_list", "ID3D12Allocator", "static ALLOCATOR_FUNCTIONS: Table<"),
        ("d3d12/command_queue", "ID3D12CommandQueue", "const fn queue_table("),
        ("d3d12/resource", "ID3D12Resource", "static RESOURCE_FUNCTIONS: Table<"),
        ("d3d12/resource", "ID3D12Heap", "static HEAP_FUNCTIONS: Table<"),
        ("d3d12/fence.rs", "ID3D12Fence", "static FUNCTIONS: Table<"),
        ("descriptors", "ID3D12DescHeap", "static FUNCTIONS: Table<"),
        ("d3d12/root", "ID3D12RootSig", "static FUNCTIONS: Table<"),
        ("d3d12/query.rs", "ID3D12QueryHeap", "static FUNCTIONS: Table<"),
        ("d3d12/signature.rs", "ID3D12CmdSig", "static FUNCTIONS: Table<"),
        ("d3d12/pipeline", "ID3D12Pipeline", "static FUNCTIONS: Table<"),
        ("d3d12/dxgi.rs", "IDXGIDevice", "static DEVICE_FUNCTIONS: Table<"),
        ("d3d12/dxgi.rs", "IDXGIAdapter", "static ADAPTER_FUNCTIONS: Table<"),
        ("d3d12/dxgi.rs", "IDXGIFactory", "static FACTORY_FUNCTIONS: Table<"),
        ("d3d12/dxgi.rs", "IDXGIOutput", "static OUTPUT_FUNCTIONS: Table<"),
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

pub(super) fn scan_unsupported_sites(root: &Path) -> Result<Vec<AreaCount>> {
    let buckets: &[(&str, &[&str])] = &[
        ("Shader / Emit & Lower", &["engine/shader/src/emit"]),
        (
            "Shader / Resource & IR",
            &[
                "engine/shader/src/resource",
                "engine/shader/src/ir",
                "engine/shader/src/io",
                "engine/shader/src/structure",
            ],
        ),
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

pub(super) fn scan_dxgi_formats(root: &Path) -> Result<Vec<FormatBar>> {
    // Full DXGI console format catalog: standard 1..=115 (115) + console extensions 116..=120 (5) + 189..=191 (3) = 123.
    const TOTAL_DXGI: usize = 123;

    let res_raw = read_module_sources(&root.join("engine/gpu/src/d3d12/resource"))?;
    let tex_raw = read_module_sources(&root.join("engine/formats/src/texture"))?;
    let host_raw = read_module_sources_raw(&root.join("engine/gpu/src/backend"))?;

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
