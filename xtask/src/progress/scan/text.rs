use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

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
pub(super) fn collect_module_files(base: &Path) -> Vec<PathBuf> {
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
pub(super) fn read_module_sources(base: &Path) -> Result<String> {
    read_module_sources_inner(base, true)
}

pub(super) fn read_module_sources_raw(base: &Path) -> Result<String> {
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

pub(super) fn strip_line_comment(line: &str) -> &str {
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

pub(super) fn extract_bracket_body(src: &str, open_pos: usize) -> Option<&str> {
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

pub(super) fn extract_brace_body(src: &str, open_pos: usize) -> Option<&str> {
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

pub(super) fn collect_string_literals_in_slice(slice: &str) -> HashSet<String> {
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

pub(super) fn collect_match_string_arms(src: &str, anchor: &str) -> HashSet<String> {
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
