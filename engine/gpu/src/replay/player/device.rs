//! D3D12 device setup, debug-layer collection, barrier builders, and file I/O helpers.

use super::*;

pub(super) fn rows_equal(p: &wire::Present, a: &[u8], b: &[u8]) -> bool {
    let texel = match p.format {
        9..=14 => 8,
        _ => 4,
    };
    (0..p.height as usize).all(|y| {
        let from = y * p.pitch as usize;
        let to = from + p.width as usize * texel;
        a.get(from..to) == b.get(from..to)
    })
}

pub(super) fn to_rect(r: [i32; 4]) -> RECT {
    RECT {
        left: r[0],
        top: r[1],
        right: r[2],
        bottom: r[3],
    }
}

/// # Safety
/// An open list; barriers of live resources.
pub(super) unsafe fn barriers(list: &ID3D12GraphicsCommandList, all: &[D3D12_RESOURCE_BARRIER]) {
    for some in all.chunks(1000) {
        // SAFETY: the caller's.
        unsafe { list.ResourceBarrier(some) };
    }
}

pub(super) fn uav_barrier(res: Option<&ID3D12Resource>) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                // SAFETY: a borrowed pointer (or none) never released.
                pResource: res.map_or(std::mem::ManuallyDrop::new(None), |r| unsafe {
                    std::mem::transmute_copy(r)
                }),
            }),
        },
    }
}

/// Any placed resource to any other: waits for all writes.
pub(super) fn alias() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                pResourceBefore: std::mem::ManuallyDrop::new(None),
                pResourceAfter: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

pub(super) fn read_at(file: &File, mut to: &mut [u8], mut at: u64) -> std::io::Result<()> {
    while !to.is_empty() {
        let n = file.seek_read(to, at)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        to = &mut to[n..];
        at += n as u64;
    }
    Ok(())
}

pub(super) fn make_device(
    frame: &Frame,
    debug: bool,
    breadcrumbs: bool,
) -> Result<(ID3D12Device, Option<ID3D12InfoQueue>)> {
    // SAFETY: plain creation and queries.
    unsafe {
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| {
                    l.EnableDebugLayer();
                    // Every shader access checked on the card: very slow.
                    if std::env::var_os("OATHKEEP_REPLAY_GBV").is_some()
                        && let Ok(l) = l.cast::<ID3D12Debug1>()
                    {
                        l.SetEnableGPUBasedValidation(true);
                    }
                }),
                Err(e) => tracing::warn!("no debug layer: {e}"),
            }
        }
        if breadcrumbs {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            D3D12GetDebugInterface(&mut dred)?;
            if let Some(dred) = dred {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let mut adapter = None;
        for i in 0.. {
            let Ok(a) = factory.EnumAdapters1(i) else {
                break;
            };
            let d = a.GetDesc1()?;
            if (d.VendorId, d.DeviceId) == (frame.vendor, frame.device) {
                adapter = Some(a);
                break;
            }
        }
        if adapter.is_none() {
            tracing::warn!(
                "the adapter recorded on ({}) is not here: the default one is used",
                frame.adapter
            );
        }
        let mut device: Option<ID3D12Device> = None;
        match &adapter {
            Some(a) => D3D12CreateDevice(a, D3D_FEATURE_LEVEL_12_0, &mut device)?,
            None => D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?,
        }
        let device = device.context("no device")?;
        let info = device.cast::<ID3D12InfoQueue>().ok();
        Ok((device, info))
    }
}

/// The debug layer's errors: how many of each kind, and its first text.
pub(super) fn debug_messages(q: &ID3D12InfoQueue) -> Vec<String> {
    let mut seen: BTreeMap<i32, (u32, String)> = BTreeMap::new();
    // SAFETY: messages read into buffers of the size asked for.
    unsafe {
        for i in 0..q.GetNumStoredMessages() {
            let mut size = 0;
            if q.GetMessage(i, None, &mut size).is_err() {
                continue;
            }
            let mut buffer = vec![0u64; size.div_ceil(8)];
            let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
            if q.GetMessage(i, Some(message), &mut size).is_err() {
                continue;
            }
            let m = &*message;
            if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
                let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                let text = || {
                    String::from_utf8_lossy(text)
                        .trim_end_matches('\0')
                        .to_owned()
                };
                seen.entry(m.ID.0).or_insert_with(|| (0, text())).0 += 1;
            }
        }
        q.ClearStoredMessages();
    }
    seen.into_iter()
        .map(|(id, (n, text))| format!("debug layer: {n} errors {id}, first: {text}"))
        .collect()
}
