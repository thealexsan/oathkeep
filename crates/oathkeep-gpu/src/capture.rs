use std::path::PathBuf;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub(crate) fn folder() -> Option<&'static PathBuf> {
    static FOLDER: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOLDER
        .get_or_init(|| std::env::var_os("OATHKEEP_CAPTURE").map(PathBuf::from))
        .as_ref()
}

pub(crate) fn listed(n: u64) -> bool {
    static LIST: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    let named = LIST
        .get_or_init(|| {
            let list = std::env::var("OATHKEEP_CAPTURE_FRAMES").unwrap_or_default();
            list.split(',')
                .filter_map(|f| f.trim().parse().ok())
                .collect()
        })
        .contains(&n);
    static ALSO: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    let also = ALSO.get_or_init(|| {
        let list = std::env::var("OATHKEEP_CAPTURE_WORLD_ALSO").unwrap_or_default();
        list.split(',')
            .filter_map(|f| f.trim().parse().ok())
            .collect()
    });
    let first = WORLD.load(std::sync::atomic::Ordering::Relaxed);
    let after = n.wrapping_sub(first);
    named || first != u64::MAX && ([2, 6, 30, 80].contains(&after) || also.contains(&after))
}

#[cfg(feature = "diagnostics")]
pub(crate) fn op(list: usize, what: impl FnOnce() -> String) {
    use std::io::Write;
    let frame = crate::queue::presented() + 1;
    if !listed(frame) {
        if world_on() && WORLD.load(std::sync::atomic::Ordering::Relaxed) == u64::MAX {
            static IN_FRAME: std::sync::Mutex<(u64, u32)> = std::sync::Mutex::new((0, 0));
            {
                let mut in_frame = IN_FRAME.lock().unwrap_or_else(|e| e.into_inner());
                if in_frame.0 != frame {
                    *in_frame = (frame, 0);
                }
                in_frame.1 += 1;
                if in_frame.1 > 300 {
                    return;
                }
            }
            let what = what();
            let mut ring = RING.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() >= RING_LINES {
                ring.pop_front();
            }
            ring.push_back(format!("frame {frame} list {list:#x} {what}"));
        }
        return;
    }
    let Some(folder) = folder() else {
        return;
    };
    static FILE: std::sync::Mutex<Option<(u64, u64, std::fs::File)>> = std::sync::Mutex::new(None);
    let what = what();
    let mut file = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if file.as_ref().is_none_or(|f| f.0 != frame) {
        let Ok(made) = std::fs::File::create(folder.join(format!("frame-{frame}-ops.txt"))) else {
            return;
        };
        *file = Some((frame, 0, made));
    }
    if let Some((_, n, f)) = file.as_mut() {
        *n += 1;
        let _ = writeln!(f, "{n} list {list:#x} {what}");
    }
}

#[cfg(feature = "diagnostics")]
static RING: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
#[cfg(feature = "diagnostics")]
const RING_LINES: usize = 6000;

fn world_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CAPTURE_WORLD").is_some())
}

static WORLD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

pub(crate) fn frame_drew(frame: u64, drawn: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let before = LAST.swap(drawn, Relaxed);
    if world_on()
        && drawn.saturating_sub(before) >= 500
        && WORLD
            .compare_exchange(u64::MAX, frame, Relaxed, Relaxed)
            .is_ok()
    {
        tracing::info!(
            frame,
            "the first frame of 500 draws or more: frames {} and {} are captured",
            frame + 2,
            frame + 6
        );
        #[cfg(feature = "diagnostics")]
        if let Some(folder) = folder() {
            let lines: Vec<String> =
                std::mem::take(&mut *RING.lock().unwrap_or_else(|e| e.into_inner())).into();
            let _ = std::fs::write(
                folder.join("before-world-ops.txt"),
                lines.join(
                    "
",
                ),
            );
        }
    }
}

pub(crate) fn wanted(n: u64) -> bool {
    let before_world = world_on()
        && n.is_multiple_of(16)
        && WORLD.load(std::sync::atomic::Ordering::Relaxed) == u64::MAX;
    let first = WORLD.load(std::sync::atomic::Ordering::Relaxed);
    let in_world = world_on() && first != u64::MAX && n > first && (n - first).is_multiple_of(8);
    folder().is_some() && (n.is_power_of_two() || listed(n) || before_world || in_world)
}

pub(crate) fn save(n: u64, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) {
    save_as(&format!("frame-{n}"), width, height, pitch, bgra, rows);
}

fn save_as(name: &str, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) {
    let Some(folder) = folder() else { return };
    let (w, h) = ((width / 2).max(1), (height / 2).max(1));
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rows[(2 * y).min(height as usize - 1) * pitch..];
        for x in 0..w as usize {
            let p = &row[(2 * x).min(width as usize - 1) * 4..][..3];
            if bgra {
                raw.extend([p[2], p[1], p[0]]);
            } else {
                raw.extend_from_slice(p);
            }
        }
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(w.to_be_bytes());
    header.extend(h.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    let mut compressed = vec![0u8; zlib_rs::compress_bound(raw.len())];
    let (idat, rc) = zlib_rs::compress_slice(&mut compressed, &raw, zlib_rs::DeflateConfig::new(3));
    debug_assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    chunk(&mut png, b"IDAT", idat);
    chunk(&mut png, b"IEND", &[]);
    let path = folder.join(format!("{name}.png"));
    match std::fs::write(&path, png) {
        Ok(()) => tracing::info!(path = %path.display(), "frame saved"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "frame not saved"),
    }
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

pub(crate) fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    read_back_of(device, list, texture, 0)
}

fn read_back_of(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    sub: u32,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: a live texture; outputs for one subresource.
    unsafe {
        let desc = texture.GetDesc();
        device.GetCopyableFootprints(
            &desc,
            sub,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
    }
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: total,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: sub,
            },
        };
        list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        Some((buffer, footprint))
    }
}

pub(crate) fn save_read_back(
    frame: u64,
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    let f = footprint.Footprint;
    let bgra = match f.Format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => false,
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => true,
        other => {
            tracing::warn!(format = other.0, "frame not saved: not 8-bit color");
            return;
        }
    };
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished (every submission is waited for); the
    // mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let len = f.RowPitch as usize * f.Height as usize;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), len);
        save(frame, f.Width, f.Height, f.RowPitch as usize, bgra, rows);
        buffer.Unmap(0, None);
    }
}

static TARGETS: std::sync::Mutex<Vec<(usize, std::sync::Arc<crate::resource::HostTexture>)>> =
    std::sync::Mutex::new(Vec::new());

fn dump_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && std::env::var_os("OATHKEEP_DUMP_TARGETS").is_some())
}

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

pub(crate) fn dump_targets(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let targets = std::mem::take(&mut *TARGETS.lock().unwrap_or_else(|e| e.into_inner()));
    let Some(host) = crate::host::host() else {
        return;
    };
    for (i, (resource, copy)) in targets.iter().enumerate() {
        let texture = &copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.SampleDesc.Count > 1 || d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
            continue;
        }
        let state = copy.state.load(Relaxed);
        dump_one(
            host,
            texture,
            state,
            frame,
            i,
            *resource as crate::object::This,
            &d,
        );
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
    let Some((buffer, footprint)) = read else {
        return;
    };
    let f = footprint.Footprint;
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let pitch = f.RowPitch as usize;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * f.Height as usize);
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
        if let Some(rgba) = to_rgba(
            f.Format.0 as u32,
            f.Width as usize,
            f.Height as usize,
            pitch,
            rows,
        ) {
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
    let (sign, exp, man) = (
        (h >> 15) as u32,
        ((h >> 10) & 0x1f) as i32,
        (h & 0x3ff) as f32,
    );
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

fn small_float(bits: u32, man: u32) -> f32 {
    let (exp, m) = ((bits >> man) & 0x1f, (bits & ((1 << man) - 1)) as f32);
    match exp {
        0 => m / (1 << man) as f32 * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + m / (1 << man) as f32) * 2f32.powi(e as i32 - 15),
    }
}

fn to_rgba(format: u32, w: usize, h: usize, pitch: usize, rows: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(w * h * 4);
    let px = |x: usize, y: usize, n: usize| &rows[y * pitch + x * n..][..n];
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
    let u32_at =
        |b: &[u8], i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap_or([0; 4]));
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
        87 | 88 | 90..=93 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[2], p[1], p[0], 255]);
                }
            }
            return Some(out);
        }
        23..=25 | 89 => (
            4,
            Box::new(move |b| {
                let v = u32_at(b, 0);
                [
                    (v & 0x3ff) as f32,
                    (v >> 10 & 0x3ff) as f32,
                    (v >> 20 & 0x3ff) as f32,
                ]
            }),
        ),
        26 => (
            4,
            Box::new(move |b| {
                let v = u32_at(b, 0);
                [
                    small_float(v & 0x7ff, 6),
                    small_float(v >> 11 & 0x7ff, 6),
                    small_float(v >> 22, 5),
                ]
            }),
        ),
        9..=10 => (
            8,
            Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), half(u16_at(b, 2))]),
        ),
        11..=14 => (
            8,
            Box::new(move |b| [0, 1, 2].map(|i| f32::from(u16_at(b, i)))),
        ),
        33..=34 => (
            4,
            Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), 0.0]),
        ),
        1..=2 => (
            16,
            Box::new(move |b| [0, 1, 2].map(|i| f32::from_bits(u32_at(b, i)))),
        ),
        15..=16 => (
            8,
            Box::new(move |b| {
                [
                    f32::from_bits(u32_at(b, 0)),
                    f32::from_bits(u32_at(b, 1)),
                    0.0,
                ]
            }),
        ),
        35..=38 => (
            4,
            Box::new(move |b| [f32::from(u16_at(b, 0)), f32::from(u16_at(b, 1)), 0.0]),
        ),
        48..=52 => (
            2,
            Box::new(move |b| [f32::from(b[0]), f32::from(b[1]), 0.0]),
        ),
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
    let above = values
        .iter()
        .filter(|v| v.iter().any(|c| c.is_finite() && *c > low))
        .count();
    let nonzero_percent = above as f32 * 100.0 / values.len().max(1) as f32;
    tracing::info!(format, low, high, nonzero_percent, "target values");
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
        tracing::info!(
            format,
            nonzero_bits_percent,
            packed_high,
            "target values as packed light"
        );
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

const GRID: u32 = 4;
const SLOT: u64 = 512;

pub(crate) fn probe(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<ID3D12Resource> {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    if !matches!(d.Format.0, 27..=29 | 87 | 90 | 91) {
        return None;
    }
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: SLOT * u64::from(GRID * GRID),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        for i in 0..GRID * GRID {
            let (x, y) = (
                (2 * (i % GRID) + 1) * d.Width as u32 / (2 * GRID),
                (2 * (i / GRID) + 1) * d.Height / (2 * GRID),
            );
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: SLOT * u64::from(i),
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: d.Format,
                            Width: 1,
                            Height: 1,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let pixel = D3D12_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + 1,
                bottom: y + 1,
                back: 1,
            };
            list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&pixel));
        }
        Some(buffer)
    }
}

pub(crate) fn black(buffer: &ID3D12Resource) -> bool {
    let mut data = std::ptr::null_mut();
    // SAFETY: the copies finished (every submission is waited for).
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return false;
        }
        let bytes =
            std::slice::from_raw_parts(data.cast::<u8>(), (SLOT * u64::from(GRID * GRID)) as usize);
        let level = bytes
            .chunks(SLOT as usize)
            .flat_map(|pixel| pixel[..3].iter().copied())
            .max()
            .unwrap_or(0);
        buffer.Unmap(0, None);
        let mut levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
        if levels.len() == 12 {
            levels.pop_front();
        }
        levels.push_back(level);
        level < 8
    }
}

static LEVELS: std::sync::Mutex<std::collections::VecDeque<u8>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

pub(crate) fn recent_levels() -> Vec<u8> {
    let levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    levels.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc_matches_the_png_check_value() {
        assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
    }

    #[test]
    fn unorm16_and_xr_bias_targets_decode_as_their_formats() {
        let unorm16 = [0x00, 0x80, 0x00, 0x40, 0, 0, 0xff, 0xff];
        assert_eq!(
            super::to_rgba(11, 1, 1, 8, &unorm16),
            Some(vec![255, 127, 0, 255])
        );
        let xr = (0x3ffu32 | 0x1ff << 10 | 3 << 30).to_le_bytes();
        assert_eq!(
            super::to_rgba(89, 1, 1, 4, &xr),
            Some(vec![255, 127, 0, 255])
        );
        assert_eq!(
            super::to_rgba(87, 1, 1, 4, &[1, 2, 3, 4]),
            Some(vec![3, 2, 1, 255])
        );
    }
}
