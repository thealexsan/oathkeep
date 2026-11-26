//! COM lifetime watcher and texture descriptor address query.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use super::check::{Names, decode};
use super::with;

#[repr(C)]
struct Notice {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    id: u64,
    object: usize,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}
static NOTICE: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    HRESULT(0x8000_4002_u32 as i32)
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Notice>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Notice>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Notice>());
            let now = crate::host::now_ms();
            with(|b| b.remove(gone.id, gone.object, now));
        }
        left
    }
}

/// Has the book told when `object` goes.
pub(super) fn watch(object: &ID3D12Object, id: u64) {
    const KEY: GUID = GUID::from_u128(0x2c7d1a55_9e0b_46f2_8c11_5b8e70a4d913);
    let raw = object.as_raw() as usize;
    let notice = Box::into_raw(Box::new(Notice {
        methods: &NOTICE.0,
        references: AtomicU32::new(1),
        id,
        object: raw,
    }));
    // SAFETY: a COM object of ours with one reference, which this drops; the
    // object takes its own.
    unsafe {
        let notice = IUnknown::from_raw(notice.cast());
        if object.SetPrivateDataInterface(&KEY, &notice).is_err() {
            std::mem::forget(notice);
        }
    }
}

/// A view format for a typeless one (its first with a plain type).
fn viewable(format: i32) -> Option<i32> {
    Some(match format {
        1 => 2,
        5 => 6,
        9 => 11,
        15 => 16,
        19 => 21,
        23 => 24,
        27 => 28,
        33 => 35,
        39 => 41,
        44 => 46,
        48 => 49,
        53 => 56,
        60 => 61,
        70 | 73 | 76 | 79 | 82 | 94 | 97 => format + 1,
        90 => 87,
        92 => 88,
        20 | 40 | 45 | 55 => return None, // typed depth: no shader view
        f => f,
    })
}

/// The address of a texture: the base its descriptor has.
pub(super) fn texture_address(texture: &ID3D12Resource, desc: &D3D12_RESOURCE_DESC) -> Option<u64> {
    struct Slot(ID3D12DescriptorHeap, usize);
    // SAFETY: a descriptor heap, used under the lock.
    unsafe impl Send for Slot {}
    static SLOT: Mutex<Option<Slot>> = Mutex::new(None);
    if desc.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 != 0 {
        return None;
    }
    let host = crate::host::made_host()?;
    let mut slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        // SAFETY: plain heap creation.
        let heap: ID3D12DescriptorHeap = unsafe {
            host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                ..Default::default()
            })
        }
        .ok()?;
        // SAFETY: a live heap.
        let at = unsafe { heap.GetCPUDescriptorHandleForHeapStart().ptr };
        *slot = Some(Slot(heap, at));
    }
    let at = slot.as_ref()?.1;
    let format = viewable(desc.Format.0)?;
    let mut view = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT(format),
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        ..Default::default()
    };
    match desc.Dimension {
        D3D12_RESOURCE_DIMENSION_TEXTURE1D => {
            view.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE1D;
            view.Anonymous.Texture1D.MipLevels = 1;
        }
        D3D12_RESOURCE_DIMENSION_TEXTURE3D => {
            view.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE3D;
            view.Anonymous.Texture3D.MipLevels = 1;
        }
        _ if desc.SampleDesc.Count > 1 => view.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DMS,
        _ => view.Anonymous.Texture2D.MipLevels = 1,
    }
    let mut words = [0u8; 32];
    // SAFETY: a slot of ours, a live texture; the handle is the descriptor's
    // memory or the read fails.
    unsafe {
        host.device
            .CreateShaderResourceView(texture, Some(&view), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: at });
    }
    if !crate::host::read_into(at as *const u8, &mut words) {
        return None;
    }
    match decode(&words) {
        Names::Texture { base, .. } => Some(base),
        _ => None,
    }
}
