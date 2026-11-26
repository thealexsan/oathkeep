//! Render-target and depth/stencil dumping (`OATHKEEP_DUMP_TARGETS`).

use windows::Win32::Graphics::Direct3D12::*;

use super::readback::{read_back, read_back_of};
use super::{folder, listed, save_as};

/// With `OATHKEEP_DUMP_TARGETS` set: title textures the frames listed in
/// `OATHKEEP_CAPTURE_FRAMES` render to or write, in first-use order, saved
/// at that frame's present as `frame-<n>-<i>-<resource>-<w>x<h>-f<format>.png`
/// (what each holds at the end of the frame; formats normalized to show
/// structure: floats scaled by their largest value, depth by its range).
/// The host copies are held: the title may release a target before the
/// frame is presented.
static TARGETS: std::sync::Mutex<Vec<(usize, std::sync::Arc<crate::resource::HostTexture>)>> =
    std::sync::Mutex::new(Vec::new());

fn dump_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && crate::switch::on("OATHKEEP_DUMP_TARGETS"))
}

/// Notes a title texture the frame being built renders to or writes.
pub(crate) fn note_target(resource: crate::object::This) {
    if resource.is_null() || !dump_on() || !listed(crate::queue::presented() + 1) {
        return;
    }
    let mut targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    if !targets.iter().any(|&(r, _)| r == resource as usize)
        && let Some(copy) = crate::resource::host_copy(resource)
    {
        targets.push((resource as usize, copy));
    }
}

/// At the present of frame `frame`: saves the noted targets.
pub(crate) fn dump_targets(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let targets = std::mem::take(&mut *TARGETS.lock().unwrap_or_else(|e| e.into_inner()));
    let Some(host) = crate::host::host() else { return };
    for (i, (resource, copy)) in targets.iter().enumerate() {
        let texture = &copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.SampleDesc.Count > 1 || d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
            continue;
        }
        // Every submission has run; the dump's list moves it there and back.
        let state = copy.state.load(Relaxed);
        dump_one(host, texture, state, frame, i, *resource as crate::object::This, &d);
    }
}

fn dump_one(
    host: &crate::host::Host,
    texture: &ID3D12Resource,
    state: u32,
    frame: u64,
    i: usize,
    resource: crate::object::This,
    d: &D3D12_RESOURCE_DESC,
) {
    let Ok(list) = host.new_list() else { return };
    let state = D3D12_RESOURCE_STATES(crate::host::legal_state(texture, state) as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: an open list of ours and a live texture.
    let read = unsafe {
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, state, copy)]);
        }
        let read = read_back(&host.device, &list.list, texture);
        // The stencil plane of a depth-stencil target, which the title's
        // lighting reads (material class in the low bits; 0x08 "inside a
        // room", which the lamps of a room are multiplied by): the plane
        // after the depth plane of a one-level, one-slice texture.
        let stencil = (matches!(d.Format.0, 19..=22 | 44..=47)
            && d.MipLevels == 1
            && d.DepthOrArraySize == 1)
            .then(|| read_back_of(&host.device, &list.list, texture, 1))
            .flatten();
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, copy, state)]);
        }
        (read, stencil)
    };
    let (read, stencil) = read;
    list.close();
    host.execute(&[&list]);
    if let Some((buffer, footprint)) = stencil {
        stencil_values(&buffer, &footprint, frame, resource);
    }
    let Some((buffer, footprint)) = read else { return };
    let f = footprint.Footprint;
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let pitch = f.RowPitch as usize;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * f.Height as usize);
        // A 24-bit depth plane comes back as 32-bit words: which depth values
        // (low 24 bits) and which top bytes it holds, the most common first.
        if matches!(d.Format.0, 44..=47) {
            let mut depths = std::collections::HashMap::<u32, u32>::new();
            let mut tops = std::collections::HashMap::<u32, u32>::new();
            for y in 0..f.Height as usize {
                for x in 0..f.Width as usize {
                    let at = y * pitch + x * 4;
                    let w = u32::from_le_bytes(rows[at..at + 4].try_into().unwrap_or([0; 4]));
                    *depths.entry(w & 0xff_ffff).or_default() += 1;
                    *tops.entry(w >> 24).or_default() += 1;
                }
            }
            let most = |m: std::collections::HashMap<u32, u32>| {
                let mut v: Vec<(u32, u32)> = m.into_iter().collect();
                v.sort_by_key(|e| std::cmp::Reverse(e.1));
                (v.len(), v.into_iter().take(12).collect::<Vec<_>>())
            };
            tracing::info!(?resource, depths = ?most(depths), top_bytes = ?most(tops), "24-bit depth plane values (value, texels)");
        }
        if let Some(rgba) = to_rgba(f.Format.0 as u32, f.Width as usize, f.Height as usize, pitch, rows) {
            let name = format!(
                "frame-{frame}-{i:02}-{resource:?}-{}x{}-f{}",
                d.Width, d.Height, d.Format.0
            );
            save_as(&name, f.Width, f.Height, f.Width as usize * 4, false, &rgba);
        } else {
            tracing::info!(format = f.Format.0, ?resource, "target not dumped: format");
        }
        buffer.Unmap(0, None);
    }
}

/// Logs which values a stencil plane read back by [`read_back_of`] holds
/// (the most common first), how many texels have each of its bits, and the
/// box of the texels with bit 3 (0x08).
fn stencil_values(
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    frame: u64,
    resource: crate::object::This,
) {
    let f = footprint.Footprint;
    let (w, h, pitch) = (f.Width as usize, f.Height as usize, f.RowPitch as usize);
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows of
    // one byte a texel, the last row without its padding.
    unsafe {
        if h == 0 || w == 0 || buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * (h - 1) + w);
        let mut values = [0u32; 256];
        let mut bits = [0u32; 8];
        // Left, top, right, bottom of the texels with bit 3.
        let mut inside = [u32::MAX, u32::MAX, 0, 0];
        for y in 0..h {
            for (x, &v) in rows[y * pitch..y * pitch + w].iter().enumerate() {
                values[usize::from(v)] += 1;
                for (bit, count) in bits.iter_mut().enumerate() {
                    *count += u32::from(v >> bit & 1);
                }
                if v & 8 != 0 {
                    inside = [
                        inside[0].min(x as u32),
                        inside[1].min(y as u32),
                        inside[2].max(x as u32),
                        inside[3].max(y as u32),
                    ];
                }
            }
        }
        let mut most: Vec<(usize, u32)> = values
            .iter()
            .copied()
            .enumerate()
            .filter(|v| v.1 != 0)
            .collect();
        most.sort_by_key(|v| std::cmp::Reverse(v.1));
        most.truncate(16);
        tracing::info!(
            frame,
            ?resource,
            width = w,
            height = h,
            values = ?most,
            texels_with_bit = ?bits,
            bit3_box = ?inside,
            "stencil plane values (value, texels)"
        );
        buffer.Unmap(0, None);
    }
}

fn half(h: u16) -> f32 {
    let (sign, exp, man) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

/// A float packed in `bits` with a 5-bit exponent and `man` mantissa bits
/// (the 11- and 10-bit floats of R11G11B10).
fn small_float(bits: u32, man: u32) -> f32 {
    let (exp, m) = ((bits >> man) & 0x1f, (bits & ((1 << man) - 1)) as f32);
    match exp {
        0 => m / (1 << man) as f32 * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + m / (1 << man) as f32) * 2f32.powi(e as i32 - 15),
    }
}

/// Rows of a host format as 8-bit RGBA: 8-bit color as is, everything else
/// as channel values scaled so the largest finite one is white (single
/// channels: lowest black, highest white, as grey). `None` for formats not
/// handled.
pub(super) fn to_rgba(format: u32, w: usize, h: usize, pitch: usize, rows: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(w * h * 4);
    let px = |x: usize, y: usize, n: usize| &rows[y * pitch + x * n..][..n];
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap_or([0; 4]));
    // Channel values as floats, then scaled.
    type Decode = Box<dyn Fn(&[u8]) -> [f32; 3]>;
    let (n, decode): (usize, Decode) = match format {
        27..=32 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[0], p[1], p[2], 255]);
                }
            }
            return Some(out);
        }
        // B8G8R8A8 and B8G8R8X8 (89 is R10G10B10_XR_BIAS_A2, below).
        87 | 88 | 90..=93 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[2], p[1], p[0], 255]);
                }
            }
            return Some(out);
        }
        23..=25 | 89 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [(v & 0x3ff) as f32, (v >> 10 & 0x3ff) as f32, (v >> 20 & 0x3ff) as f32]
        })),
        26 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [small_float(v & 0x7ff, 6), small_float(v >> 11 & 0x7ff, 6), small_float(v >> 22, 5)]
        })),
        9..=10 => (8, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), half(u16_at(b, 2))])),
        11..=14 => (8, Box::new(move |b| [0, 1, 2].map(|i| f32::from(u16_at(b, i))))),
        33..=34 => (4, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), 0.0])),
        1..=2 => (16, Box::new(move |b| {
            [0, 1, 2].map(|i| f32::from_bits(u32_at(b, i)))
        })),
        15..=16 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)), f32::from_bits(u32_at(b, 1)), 0.0])),
        35..=38 => (4, Box::new(move |b| [f32::from(u16_at(b, 0)), f32::from(u16_at(b, 1)), 0.0])),
        48..=52 => (2, Box::new(move |b| [f32::from(b[0]), f32::from(b[1]), 0.0])),
        // Single channels (and depth planes): shown as grey.
        39..=43 => (4, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        19..=22 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        44..=47 => (4, Box::new(move |b| [(u32_at(b, 0) & 0xff_ffff) as f32; 3])),
        53..=54 => (2, Box::new(move |b| [half(u16_at(b, 0)); 3])),
        55..=59 => (2, Box::new(move |b| [f32::from(u16_at(b, 0)); 3])),
        60..=65 => (1, Box::new(move |b| [f32::from(b[0]); 3])),
        _ => return None,
    };
    let values: Vec<[f32; 3]> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| decode(px(x, y, n)))
        .collect();
    let grey = matches!(format, 19..=22 | 39..=47 | 53..=65);
    let finite = values.iter().flatten().copied().filter(|v| v.is_finite());
    let (low, high) = if grey {
        finite.fold((f32::MAX, f32::MIN), |(l, h), v| (l.min(v), h.max(v)))
    } else {
        (0.0, finite.fold(0.0f32, f32::max))
    };
    let range = (high - low).max(1e-20);
    // How much of the target holds anything: a lit scene and one bright
    // lamp on black have the same highest value.
    let above = values.iter().filter(|v| v.iter().any(|c| c.is_finite() && *c > low)).count();
    let nonzero_percent = above as f32 * 100.0 / values.len().max(1) as f32;
    tracing::info!(format, low, high, nonzero_percent, "target values");
    // Small float colour targets (the 1x1 luminance and exposure targets):
    // every texel's four channels as numbers. The picture has no alpha and
    // eight bits a channel.
    if matches!(format, 1..=2 | 9..=10) && w * h <= 16 {
        for y in 0..h {
            for x in 0..w {
                let b = px(x, y, n);
                let texel: [f32; 4] = if n == 16 {
                    [0, 1, 2, 3].map(|i| f32::from_bits(u32_at(b, i)))
                } else {
                    [0, 1, 2, 3].map(|i| half(u16_at(b, i)))
                };
                tracing::info!(format, x, y, ?texel, "target texel");
            }
        }
    }
    // R32_UINT targets hold packed light (the title reads them back as
    // R9G9B9E5): as floats their low, high and share above the lowest say
    // nothing. How many texels hold any bit, and the highest channel value
    // of the packed reading.
    if format == 42 {
        let (mut set, mut packed_high) = (0usize, 0f32);
        for y in 0..h {
            for x in 0..w {
                let v = u32_at(px(x, y, 4), 0);
                set += usize::from(v != 0);
                let scale = 2f32.powi((v >> 27) as i32 - 24);
                for k in 0..3 {
                    packed_high = packed_high.max((v >> (9 * k) & 0x1ff) as f32 * scale);
                }
            }
        }
        let nonzero_bits_percent = set as f32 * 100.0 / (w * h).max(1) as f32;
        tracing::info!(format, nonzero_bits_percent, packed_high, "target values as packed light");
        // The same words as plain numbers, by class. Tables of indices and
        // masks say nothing as floats or as packed light: in the title's
        // shadow-mask table 0 is a block in full shadow, a word up to 255
        // one value for the block, above that a block of the atlas, and all
        // ones a texel never written.
        let mut classes = [0usize; 6];
        let mut highest_below_ones = 0u32;
        for y in 0..h {
            for x in 0..w {
                let v = u32_at(px(x, y, 4), 0);
                classes[match v {
                    0 => 0,
                    1..=15 => 1,
                    16..=255 => 2,
                    256..=0x00ff_ffff => 3,
                    u32::MAX => 5,
                    _ => 4,
                }] += 1;
                if v != u32::MAX {
                    highest_below_ones = highest_below_ones.max(v);
                }
            }
        }
        tracing::info!(
            format,
            width = w,
            height = h,
            zero = classes[0],
            one_to_15 = classes[1],
            to_255 = classes[2],
            to_24_bits = classes[3],
            larger = classes[4],
            all_ones = classes[5],
            highest_below_ones,
            "target values as words"
        );
    }
    // Float colours (scene light) go through a cube root: scaled linearly,
    // a night scene under one lamp of 50 units came out all black.
    let curved = matches!(format, 1..=2 | 9..=10 | 26);
    for v in values {
        let c = v.map(|c| {
            let t = ((c - low) / range).clamp(0.0, 1.0);
            ((if curved { t.cbrt() } else { t }) * 255.0) as u8
        });
        out.extend([c[0], c[1], c[2], 255]);
    }
    Some(out)
}
