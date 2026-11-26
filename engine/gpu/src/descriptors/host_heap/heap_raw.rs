use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use super::{HostHeap, host};
use super::make::constant_bytes;
use super::staged::staged;
use super::null_views::null_buffer_view;

impl HostHeap {

    /// Makes slot `index` (a dynamic one) a raw buffer view of `len` bytes
    /// of `buffer` from byte `offset` (a multiple of 16): a root SRV with a
    /// size, whose reads past it return zeros instead of reaching whatever
    /// the host placed after the buffer.
    pub(crate) fn raw_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.at(index))
        };
        self.put(index);
        // (Bound by its handle, not read by index.)
        self.clear_twin(index);
        self.write_stride(index, 1);
        self.keep(index, Some(buffer.clone()));
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view that reads
    /// zeros.
    pub(crate) fn null_raw_at(&self, index: u32) {
        null_buffer_view(self.at(index));
        self.put(index);
        self.clear_twin(index);
        self.write_stride(index, 0);
        self.keep(index, None);
    }

    /// Makes slot `index` (a dynamic one) a raw unordered access view of
    /// `len` bytes of `buffer` from byte `offset` (a multiple of 16): a
    /// root UAV with a size, whose accesses past it do nothing instead of
    /// reaching whatever the host placed after the buffer.
    pub(crate) fn raw_uav_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device.CreateUnorderedAccessView(
                buffer,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.at(index),
            )
        };
        self.put(index);
        self.clear_twin(index);
        self.write_stride(index, 1);
        self.keep(index, Some(buffer.clone()));
    }

    /// Makes slot `index` (a dynamic one) a raw unordered access view of
    /// nothing: reads give zeros, writes are dropped.
    pub(crate) fn null_raw_uav_at(&self, index: u32) {
        let Some(host) = host() else { return };
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 1,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        // SAFETY: a slot of our heap; a null resource is allowed with a desc.
        unsafe {
            host.device.CreateUnorderedAccessView(
                None::<&ID3D12Resource>,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.at(index),
            )
        };
        self.put(index);
        self.clear_twin(index);
        self.write_stride(index, 0);
        self.keep(index, None);
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart().ptr };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start + u64::from(index) * u64::from(self.step),
        }
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory, `size` bytes at `gpu` of `buffer`, which the slot holds.
    ///
    /// The list holds it too, until it is reset; the slot shows it until it
    /// is made again, and a shader that picks the slot by an index it
    /// computes from wrong data reads it through there. The list's scratch
    /// chunks go back to the pool and are freed past its limit: session n02
    /// lost the device to a page fault in the memory of three freed ones.
    /// `OATHKEEP_SLOT_HOLDS=0`: only the list holds it, as before.
    pub(crate) fn constant_at(&self, index: u32, buffer: &ID3D12Resource, gpu: u64, size: u32) {
        static HOLDS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let holds = *HOLDS.get_or_init(|| std::env::var_os("OATHKEEP_SLOT_HOLDS").is_none_or(|v| v != "0"));
        let Some(host) = host() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: constant_bytes(size),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.at(index))
        };
        self.put(index);
        // A program that reads it as raw bytes reads its twin.
        let twin = self.twin(index).filter(|_| desc.SizeInBytes > 0);
        if let Some((twin, _)) = twin {
            // SAFETY: a live buffer that holds the range.
            let first = (gpu - unsafe { buffer.GetGPUVirtualAddress() }) / 4;
            let raw = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: first,
                        NumElements: desc.SizeInBytes / 4,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            // SAFETY: as above, and a slot of our heaps.
            unsafe { host.device.CreateShaderResourceView(buffer, Some(&raw), twin) };
        }
        self.twin_made(index, twin.is_some());
        self.write_stride(index, 0);
        // After the descriptor: what it showed is let go when nothing shows
        // it any more.
        self.keep(index, holds.then(|| buffer.clone()));
    }

    /// Where a descriptor for slot `index` is made, then [`HostHeap::put`]:
    /// a slot of this thread's in a heap the card does not see
    /// ([`staged`]), or the slot itself.
    pub(super) fn at(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        staged(self.sampler, 0).unwrap_or_else(|| self.cpu(index))
    }

    /// The descriptor this thread made last ([`HostHeap::at`]) goes to slot
    /// `index`.
    pub(super) fn put(&self, index: u32) {
        self.copy_to(index, 0);
    }

    /// The raw twin this thread made last goes to that of slot `index`.
    pub(super) fn put_twin(&self, index: u32) {
        if self.twin(index).is_some() {
            self.copy_to(self.raw + index, 1);
        }
    }

    pub(super) fn copy_to(&self, to: u32, which: u32) {
        #[cfg(feature = "diagnostics")]
        if let Some(written) = self.written.get(to as usize) {
            written.store(crate::backend::coarse_ms() as u32, std::sync::atomic::Ordering::Relaxed);
        }
        let (Some(from), Some(host)) = (staged(self.sampler, which), host()) else {
            return; // made in place
        };
        let kind = if self.sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        // SAFETY: a slot of our heap, and one of this thread's heap slots.
        unsafe { host.device.CopyDescriptorsSimple(1, self.cpu(to), from, kind) };
    }

    pub(super) fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }
}
