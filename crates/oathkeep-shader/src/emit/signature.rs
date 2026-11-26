use crate::ShaderError;

use super::container;

/// The title's vertex output interpolation lives in PSV0. A float packed
/// beside an integer can be explicitly constant, as can a float on its own.
/// OSG1 supplies the physical rows and columns used to match both tables.
pub(super) fn vertex_output_interpolation(
    mut data: &[u8],
    outputs: &mut [container::SigElement],
) -> Result<(), ShaderError> {
    const TRUNCATED: ShaderError =
        ShaderError::Malformed("truncated vertex pipeline validation data");
    fn take<'a>(data: &mut &'a [u8], len: usize) -> Result<&'a [u8], ShaderError> {
        let (head, tail) = data.split_at_checked(len).ok_or(TRUNCATED)?;
        *data = tail;
        Ok(head)
    }
    fn size(data: &mut &[u8]) -> Result<usize, ShaderError> {
        Ok(u32::from_le_bytes(take(data, 4)?.try_into().unwrap()) as usize)
    }
    let info_size = size(&mut data)?;
    let info = take(&mut data, info_size)?;
    // PSVRuntimeInfo0 has no signature. PSVRuntimeInfo1 and later share
    // these stage and signature count fields.
    if info.len() < 36 || info[24] != 1 || info[29] == 0 {
        return Ok(());
    }
    let resources = size(&mut data)?;
    if resources != 0 {
        let stride = size(&mut data)?;
        take(&mut data, resources.checked_mul(stride).ok_or(TRUNCATED)?)?;
    }
    let strings = size(&mut data)?;
    take(&mut data, strings)?;
    let indices = size(&mut data)?;
    take(&mut data, indices.checked_mul(4).ok_or(TRUNCATED)?)?;
    let stride = size(&mut data)?;
    if stride < 16 {
        return Err(TRUNCATED);
    }
    let count = usize::from(info[28]) + usize::from(info[29]);
    let elements = take(&mut data, count.checked_mul(stride).ok_or(TRUNCATED)?)?;
    for record in elements.chunks_exact(stride).skip(usize::from(info[28])) {
        let packed = record[10];
        if packed & 0x40 == 0 {
            continue;
        }
        if record[13] > 7 {
            return Err(ShaderError::Malformed("invalid vertex output interpolation"));
        }
        let first = u32::from(record[9]);
        let rows = first..first + u32::from(record[8]);
        for output in outputs.iter_mut().filter(|e| {
            e.row.is_some_and(|r| rows.contains(&r))
                && e.col == ((packed >> 4) & 3)
                && e.cols == (packed & 15)
                && e.kind == record[11]
        }) {
            output.interp = record[13];
        }
    }
    Ok(())
}

/// The rows of an `ISG1`/`OSG1` part.
pub(super) fn signature(part: &[u8]) -> Result<Vec<container::SigElement>, ShaderError> {
    use crate::dxbc::u32_at;
    let count = u32_at(part, 0)? as usize;
    let base = u32_at(part, 4)? as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = base + 32 * i;
        let name_at = u32_at(part, at + 4)? as usize;
        let name = part
            .get(name_at..)
            .and_then(|s| s.split(|&b| b == 0).next())
            .ok_or(ShaderError::Malformed("signature name out of bounds"))?;
        let mask = *part
            .get(at + 24)
            .ok_or(ShaderError::Malformed("truncated"))?;
        if u32_at(part, at + 28)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a minimum-precision signature element",
            });
        }
        if u32_at(part, at)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a signature element of another stream",
            });
        }
        let kind = match u32_at(part, at + 12)? {
            0 => 0,
            1 => 3,
            2 => 6,
            3 => 7,
            4 => 4,
            5 => 5,
            6 => 1,
            7 => 10,
            8 => 2,
            9 => 13,
            10 => 12,
            64 => 16,
            65 => 17,
            66 => 14,
            67 => 19,
            68 => 18,
            69 => 20,
            70 => 15,
            23 => 28, // barycentrics
            25 => 30, // cull primitive
            _ => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: "a system value in the signature",
                });
            }
        };
        // 16-bit elements are declared 32-bit; reading or writing one
        // converts (16-bit values travel packed on the console).
        let raw = u32_at(part, at + 16)?;
        let comp = match (kind, raw) {
            // DXIL declares the front face a uint (the validator insists).
            (13, _) => 5,
            (_, 1 | 4) => 5,
            (_, 2 | 5) => 4,
            (_, 3 | 6) => 9,
            (_, c) => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: match c {
                        0 => "a signature element of unknown type",
                        _ => "a 64-bit signature element",
                    },
                });
            }
        };
        let register = u32_at(part, at + 20)?;
        out.push(container::SigElement {
            name: String::from_utf8_lossy(name).into_owned(),
            index: u32_at(part, at + 8)?,
            kind,
            comp,
            interp: 0,
            row: (register != u32::MAX).then_some(register),
            col: mask.trailing_zeros().min(3) as u8,
            cols: mask.count_ones().max(1) as u8,
            usage: 0,
            half: (4..=6).contains(&raw),
        });
    }
    Ok(out)
}

