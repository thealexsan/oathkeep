use object::LittleEndian as LE;
use object::Object;
use object::pe::{self, ImageNtHeaders64};
use object::read::pe::{Import, ImportTable, ImportThunkList, PeFile64};

use super::{ExportInfo, ImportEntry, ImportModule, ImportSymbol, MAX_TLS_CALLBACKS, TlsInfo};
use crate::PeError;
use crate::layout;

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn convert_import(import: Import<'_>) -> ImportSymbol {
    match import {
        Import::Ordinal(o) => ImportSymbol::Ordinal(o),
        Import::Name(hint, name) => ImportSymbol::Name {
            hint,
            name: utf8(name),
        },
    }
}

pub(super) fn parse_imports(
    table: &ImportTable<'_>,
    size_of_image: u32,
) -> Result<Vec<ImportModule>, PeError> {
    let mut modules = Vec::new();
    let mut descriptors = table.descriptors()?;
    while let Some(desc) = descriptors.next()? {
        let iat_rva = desc.first_thunk.get(LE);
        // Prefer the unbound name table; fall back to the IAT for old linkers.
        let int_rva = match desc.original_first_thunk.get(LE) {
            0 => iat_rva,
            rva => rva,
        };
        modules.push(ImportModule {
            dll: utf8(table.name(desc.name.get(LE))?),
            entries: entries(table.thunks(int_rva)?, iat_rva, size_of_image, |t| {
                table.import::<ImageNtHeaders64>(t)
            })?,
        });
    }
    Ok(modules)
}

pub(super) fn parse_delay_imports(
    file: &PeFile64<'_>,
    size_of_image: u32,
) -> Result<Vec<ImportModule>, PeError> {
    let Some(table) = file.delay_load_import_table()? else {
        return Ok(Vec::new());
    };
    let mut modules = Vec::new();
    let mut descriptors = table.descriptors()?;
    while let Some(desc) = descriptors.next()? {
        modules.push(ImportModule {
            dll: utf8(table.name(desc.dll_name_rva.get(LE))?),
            entries: entries(
                table.thunks(desc.import_name_table_rva.get(LE))?,
                desc.import_address_table_rva.get(LE),
                size_of_image,
                |t| table.import::<ImageNtHeaders64>(t),
            )?,
        });
    }
    Ok(modules)
}

/// Pairs each name-table entry with its IAT slot. The IAT RVA is not checked
/// by `object`, so every slot is checked to lie inside the image here.
fn entries<'data>(
    mut thunks: ImportThunkList<'data>,
    iat_rva: u32,
    size_of_image: u32,
    import: impl Fn(pe::ImageThunkData64) -> object::read::Result<Import<'data>>,
) -> Result<Vec<ImportEntry>, PeError> {
    let mut entries = Vec::new();
    while let Some(thunk) = thunks.next::<ImageNtHeaders64>()? {
        let iat_slot_rva = u32::try_from(entries.len() * 8)
            .ok()
            .and_then(|offset| iat_rva.checked_add(offset))
            .filter(|&rva| rva.checked_add(8).is_some_and(|end| end <= size_of_image))
            .ok_or_else(|| {
                PeError::Malformed(format!("IAT at {iat_rva:#x} runs outside the image"))
            })?;
        entries.push(ImportEntry {
            symbol: convert_import(import(thunk)?),
            iat_slot_rva,
        });
    }
    Ok(entries)
}

pub(super) fn parse_exports(file: &PeFile64<'_>, image_base: u64) -> Result<Vec<ExportInfo>, PeError> {
    use object::read::{ExportFlags, ExportTarget as ReadTarget, NameOrOrdinal};

    // The PE-specific table keeps one name per address-table slot, so two
    // names sharing an ordinal lose all but the last one. The generic
    // iterator yields every name, then the ordinal-only entries.
    file.exports()
        .map_err(|e| PeError::Malformed(format!("export table: {e}")))?
        .map(|e| {
            let e = e.map_err(|e| PeError::Malformed(format!("export: {e}")))?;
            let ordinal = match e.flags() {
                ExportFlags::Pe { ordinal } => ordinal.0,
                _ => 0,
            };
            let name = match e.name() {
                NameOrOrdinal::Name(name) => Some(utf8(name)),
                NameOrOrdinal::Ordinal(_) => None,
            };
            let (rva, forward) = match e.target() {
                // The generic iterator adds the image base to an address.
                ReadTarget::Address { address } => {
                    (Some(address.wrapping_sub(image_base) as u32), None)
                }
                ReadTarget::Reexport { library, name } => {
                    let symbol = match name {
                        NameOrOrdinal::Name(name) => utf8(name),
                        NameOrOrdinal::Ordinal(ordinal) => format!("#{ordinal}"),
                    };
                    (None, Some(format!("{}.{}", utf8(library), symbol)))
                }
                _ => (None, None),
            };
            Ok(ExportInfo {
                name,
                ordinal,
                rva,
                forward,
            })
        })
        .collect()
}

pub(super) fn parse_tls(
    file: &PeFile64<'_>,
    data: &[u8],
    image_base: u64,
) -> Result<Option<TlsInfo>, PeError> {
    let Some(dd) = file.data_directory(pe::IMAGE_DIRECTORY_ENTRY_TLS) else {
        return Ok(None);
    };
    let directory_rva = dd.virtual_address.get(LE);
    if directory_rva == 0 {
        return Ok(None);
    }
    let sections = file.section_table();
    let tls = sections
        .pe_data_at(data, directory_rva)
        .ok_or_else(|| PeError::Malformed("TLS directory outside of sections".into()))?;
    let callbacks_va = layout::read_u64(tls, layout::TLS64_ADDRESS_OF_CALLBACKS)
        .ok_or_else(|| PeError::Malformed("truncated TLS directory".into()))?;

    let mut callback_rvas = Vec::new();
    if callbacks_va != 0 {
        let array_rva = va_to_rva(callbacks_va, image_base)?;
        let array = sections
            .pe_data_at(data, array_rva)
            .ok_or_else(|| PeError::Malformed("TLS callback array outside of sections".into()))?;
        for slot in array.as_chunks::<8>().0.iter().take(MAX_TLS_CALLBACKS) {
            let va = u64::from_le_bytes(*slot);
            if va == 0 {
                break;
            }
            callback_rvas.push(va_to_rva(va, image_base)?);
        }
    }
    Ok(Some(TlsInfo {
        directory_rva,
        callbacks_va,
        callback_rvas,
    }))
}

fn va_to_rva(va: u64, image_base: u64) -> Result<u32, PeError> {
    va.checked_sub(image_base)
        .and_then(|rva| u32::try_from(rva).ok())
        .ok_or_else(|| PeError::Malformed(format!("VA 0x{va:x} is outside the image")))
}
