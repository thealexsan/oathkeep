//! D3D12 resource, barrier, footprint, and swap-chain helpers for the replay test.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};

pub const SIZE: u32 = 128;

pub const COMMON: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COMMON;
pub const TARGET: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_RENDER_TARGET;
pub const READ: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
pub const UAV: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
pub const TO: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COPY_DEST;
pub const FROM: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COPY_SOURCE;
pub const ARGUMENTS: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;

pub fn buffer_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

pub fn texture_desc(
    size: u32,
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(size),
        Height: size,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: flags,
        ..Default::default()
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum In {
    Card,
    Upload,
    Back,
    /// The PC's memory, mapped, that shaders write too.
    Shared,
}

pub fn committed(
    device: &ID3D12Device,
    place: In,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> (ID3D12Resource, *mut u8) {
    let heap = match place {
        In::Card => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        },
        In::Upload => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        },
        In::Back => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        },
        In::Shared => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        },
    };
    let mut made: Option<ID3D12Resource> = None;
    let mut data = std::ptr::null_mut();
    // SAFETY: plain creation and mapping.
    unsafe {
        device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, desc, state, None, &mut made)
            .unwrap();
        let made = made.unwrap();
        if place != In::Card {
            made.Map(0, None, Some(&mut data)).unwrap();
        }
        (made, data.cast())
    }
}

pub fn moved(
    res: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer never released.
                pResource: unsafe { std::mem::transmute_copy(res) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
        ..Default::default()
    }
}

pub fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: ManuallyDrop::new(None),
            }),
        },
        ..Default::default()
    }
}

pub fn aliased(before: &ID3D12Resource, after: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Aliasing: ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                // SAFETY: borrowed pointers never released.
                pResourceBefore: unsafe { std::mem::transmute_copy(before) },
                // SAFETY: as above.
                pResourceAfter: unsafe { std::mem::transmute_copy(after) },
            }),
        },
        ..Default::default()
    }
}

pub fn sub_of(t: &ID3D12Resource, sub: u32) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer never released.
        pResource: unsafe { std::mem::transmute_copy(t) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: sub,
        },
    }
}

/// The 64 bytes of each row of a 16x16 texture.
pub fn rows(bytes: &[u8]) -> Vec<u8> {
    bytes.chunks(256).flat_map(|r| r[..64].to_vec()).collect()
}

pub fn footprint(at: u64, size: u32) -> D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: at,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            Width: size,
            Height: size,
            Depth: 1,
            RowPitch: (size * 4).next_multiple_of(256),
        },
    }
}

/// A swap chain of a window that is never shown.
pub fn swap_chain(queue: &ID3D12CommandQueue) -> (IDXGISwapChain3, HWND) {
    // SAFETY: plain creation.
    unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("replay test"),
            WS_POPUP,
            0,
            0,
            64,
            64,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 64,
            Height: 64,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let chain = factory
            .CreateSwapChainForHwnd(queue, hwnd, &desc, None, None)
            .unwrap()
            .cast()
            .unwrap();
        (chain, hwnd)
    }
}

pub fn table_of(device: &ID3D12Device) -> usize {
    // SAFETY: a live COM object starts with its table's address.
    unsafe { *device.as_raw().cast::<usize>() }
}
