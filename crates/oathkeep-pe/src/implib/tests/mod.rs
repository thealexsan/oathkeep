mod errors;
mod parsing;

use super::*;
use crate::MACHINE_AMD64;

/// `Sig1`, which must be `IMAGE_FILE_MACHINE_UNKNOWN` (not a machine).
const SIG1: u16 = 0;
/// `Sig2` of a short-format import object (`IMPORT_OBJECT_HDR_SIG2`).
const SIG2: u16 = 0xFFFF;

/// `Type` field of the header, a 2-bit value at bit 0.
pub(super) mod import_type {
    pub const CODE: u16 = 0;
    pub const DATA: u16 = 1;
    pub const CONST: u16 = 2;
}

/// `NameType` field of the header, a 3-bit value at bit 2.
pub(super) mod name_type {
    pub const ORDINAL: u16 = 0;
    pub const NAME: u16 = 1;
    pub const NAME_NO_PREFIX: u16 = 2;
    pub const NAME_UNDECORATE: u16 = 3;
    pub const NAME_EXPORTAS: u16 = 4;
}

/// One short-format import object: the 20-byte header followed by the
/// NUL-terminated symbol, DLL and (for `NAME_EXPORTAS`) export names.
pub(super) fn import_object(
    import_type: u16,
    name_type: u16,
    ordinal_or_hint: u16,
    symbol: &str,
    dll: &str,
    export_as: Option<&str>,
) -> Vec<u8> {
    let mut strings = Vec::new();
    for s in [Some(symbol), Some(dll), export_as].into_iter().flatten() {
        strings.extend_from_slice(s.as_bytes());
        strings.push(0);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&SIG1.to_le_bytes());
    out.extend_from_slice(&SIG2.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // Version
    out.extend_from_slice(&MACHINE_AMD64.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
    out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    out.extend_from_slice(&ordinal_or_hint.to_le_bytes());
    out.extend_from_slice(&(import_type | name_type << 2).to_le_bytes());
    out.extend_from_slice(&strings);
    out
}

/// One archive member: the 60-byte header, the data, and the pad byte the
/// format adds so the next header starts even.
pub(super) fn archive_member(name: &str, data: &[u8]) -> Vec<u8> {
    assert!(name.len() <= 16, "an archive member name is 16 bytes");
    let mut out = vec![b' '; 60];
    out[..name.len()].copy_from_slice(name.as_bytes());
    let size = data.len().to_string();
    out[48..48 + size.len()].copy_from_slice(size.as_bytes());
    out[58] = b'`';
    out[59] = b'\n';
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(b'\n');
    }
    out
}

pub(super) fn archive(members: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in members {
        out.extend_from_slice(&archive_member(name, &data));
    }
    out
}
