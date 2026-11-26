use std::collections::HashSet;
use std::sync::Mutex;

use super::super::files;
use super::super::xgameruntime::HRESULT;
use super::{E_POINTER, UNSUPPORTED_SCHEME};

// The URL rule.

/// Where a resolver may read `url` from: `Ok` with the NUL-terminated
/// string to hand to the host, or `Err((scheme, host))` for anything that
/// is not a local file.
pub(super) fn local(url: &[u16]) -> Result<Vec<u16>, (String, String)> {
    let text = String::from_utf16_lossy(url);
    let drive = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    let host_of = |rest: &str| rest.split(['/', '\\']).next().unwrap_or("").to_string();
    // Two leading separators, of either kind: to the host, a share.
    let share = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 2 && b[..2].iter().all(|c| b"/\\".contains(c))
    };
    let path = if let Some(rest) = text.strip_prefix(r"\\?\") {
        if !drive(rest) {
            return Err(("unc".into(), host_of(rest.trim_start_matches("UNC\\"))));
        }
        text.clone()
    } else if let Some(rest) = text.strip_prefix(r"\??\") {
        // The same prefix as the object manager spells it; the host's file
        // calls take it too.
        if !drive(rest) {
            return Err(("unc".into(), host_of(rest.trim_start_matches("UNC\\"))));
        }
        return Ok(with_nul(url));
    } else if share(&text) {
        return Err(("unc".into(), host_of(&text[2..])));
    } else if drive(&text) {
        text.clone()
    } else if let Some((scheme, rest)) = text.split_once(':').filter(|(s, _)| {
        s.len() > 1
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
    }) {
        let scheme = scheme.to_ascii_lowercase();
        let authority = rest.strip_prefix("//");
        if scheme != "file" {
            return Err((scheme, authority.map(host_of).unwrap_or_default()));
        }
        let (path, this_machine) = match authority {
            Some(rest) => {
                let host = host_of(rest);
                if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                    return Err((scheme, host));
                }
                (&rest[host.len()..], !host.is_empty())
            }
            None => (rest, false),
        };
        let whole = percent_decode(path);
        let path = percent_decode(path.trim_start_matches(['/', '\\'])).replace('/', "\\");
        if !drive(&path) {
            // With no host named, a path that starts with two separators
            // names one: `file:////server/share` is `\\server\share`.
            if !this_machine && share(&whole) {
                return Err((scheme, host_of(whole.trim_start_matches(['/', '\\']))));
            }
            // A local file without a drive: the host resolves it as is.
            return Ok(with_nul(url));
        }
        path
    } else {
        // A relative path.
        return Ok(with_nul(url));
    };
    let wide: Vec<u16> = path.encode_utf16().collect();
    Ok(files::translate(&wide).unwrap_or_else(|| with_nul(&wide)))
}

fn with_nul(s: &[u16]) -> Vec<u16> {
    s.iter().copied().chain([0]).collect()
}

pub(super) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // `from_str_radix` accepts a leading sign, so `%+1` would decode as
        // a control byte. A percent escape is exactly two hex digits.
        let hex = b
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(|c| c.is_ascii_hexdigit()))
            .and_then(|h| std::str::from_utf8(h).ok());
        match (b[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The URL to give the host, or the refusal (logged once per scheme and
/// host).
pub(super) fn checked(url: *const u16) -> Result<Vec<u16>, HRESULT> {
    if url.is_null() {
        return Err(E_POINTER);
    }
    // SAFETY: a NUL-terminated wide string from the title.
    let len = (0..)
        .take_while(|&i| unsafe { url.add(i).read() } != 0)
        .count();
    // SAFETY: as above, `len` characters before the NUL.
    let wide = unsafe { std::slice::from_raw_parts(url, len) };
    local(wide).map_err(|(scheme, host)| {
        static REFUSED: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);
        let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
        if refused
            .get_or_insert_with(HashSet::new)
            .insert((scheme.clone(), host.clone()))
        {
            tracing::warn!(
                scheme,
                host,
                "media: refused a URL that is not a local file"
            );
        }
        UNSUPPORTED_SCHEME
    })
}
