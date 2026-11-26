use std::sync::Mutex;
use windows::Win32::Graphics::Direct3D12::*;
use super::{DYNAMIC, HostHeap, dynamic_slots, host, probe_view_limit};
use super::diag::GPU_RANGES;
use super::null_views::{null_buffer_view, null_view};
use super::sets::{Made, Slots};
use super::spans::{PLACED_SPANS, Spans};

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let title_count = count;
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        // A view heap ends at the null slot shaders clamp view indices to
        // (`probe_view_limit`), whatever the title's own count: the title's
        // slots, then the dynamic ones, then null up to and with that slot.
        if !sampler {
            probe_view_limit();
        }
        let null = if sampler {
            0
        } else {
            oathkeep_shader::emit::view_heap_limit()
        };
        let raw = if sampler { 0 } else { oathkeep_shader::emit::raw_views() };
        let extra = if sampler { 0 } else { dynamic_slots(count, null) };
        let (heap, count, size) = if null == 0 {
            match make(count + extra) {
                Ok(heap) => (heap, count, count + extra),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else {
            let size = if raw == 0 { null + 1 } else { 2 * raw };
            let fitted = if count.saturating_add(extra) <= null {
                count
            } else {
                // Direct bindless slots `[0..fitted)` stay 1:1 for DXIL `UMin(idx, null)`.
                // Per-draw descriptor tables past `fitted` are snapshotted into the
                // `extra` dynamic slots `[fitted..null)` by `draw::State::tables`.
                let fitted = null.saturating_sub(extra);
                tracing::warn!(
                    count,
                    direct_slots = fitted,
                    dynamic_slots = extra,
                    "host view heap capped at {size}; views past it read a null slot"
                );
                fitted
            };
            match make(size) {
                Ok(heap) => (heap, fitted, size),
                Err(e) => {
                    tracing::warn!(error = %e, count, size, "no host view heap");
                    return None;
                }
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(size) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let strides = (!sampler)
            .then(|| {
                let bytes = u64::from(oathkeep_shader::emit::SLOT_STRIDES_COUNT) * 4;
                let (buffer, data) = host.upload_buffer(bytes)?;
                // SAFETY: the buffer's whole mapping; a live buffer.
                let gpu = unsafe {
                    std::ptr::write_bytes(data, 0, bytes as usize);
                    buffer.GetGPUVirtualAddress()
                };
                crate::backend::name(&buffer, || "element strides by view heap slot".to_owned());
                Some((buffer, gpu, data as usize))
            })
            .flatten();
        let h = HostHeap {
            buffer_sweep: Mutex::default(),
            buffer_frame: std::sync::atomic::AtomicU64::new(u64::MAX),
            heap,
            cpu,
            step,
            count,
            title_count,
            dynamic: extra,
            peak_dynamic: std::sync::atomic::AtomicU32::new(0),
            highest_direct: std::sync::atomic::AtomicU32::new(0),
            highest_capped: std::sync::atomic::AtomicU32::new(0),
            capped_mirror: [const { std::sync::atomic::AtomicU64::new(0) }; 3],
            capped_raw_pages: std::sync::atomic::AtomicU64::new(0),
            sampler,
            raw,
            twins: (0..if raw == 0 { 0 } else { count + extra }).map(|_| std::sync::atomic::AtomicBool::new(false)).collect(),
            #[cfg(feature = "diagnostics")]
            written: (0..if sampler || !crate::diagnostics::memory_ledger::on() { 0 } else { size })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            made: Made::new(count as usize),
            quick: (0..count).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
            copied: Mutex::new(Vec::new()),
            waiting: Mutex::default(),
            away: Mutex::default(),
            kept: (0..count + extra).map(|_| Mutex::new(None)).collect(),
            next: Mutex::new((0, vec![false; extra as usize])),
            free: std::sync::atomic::AtomicU32::new(extra),
            too_long: std::sync::atomic::AtomicU32::new(u32::MAX),
            strides,
            buffers: Slots::new(count as usize),
            changed: Slots::new(count as usize),
            textures: Mutex::default(),
            tabled: (0..if sampler { 0 } else { count })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            placed: {
                let spans = std::sync::Arc::<Mutex<Spans>>::default();
                if !sampler {
                    PLACED_SPANS
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(std::sync::Arc::downgrade(&spans));
                }
                spans
            },
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, size) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(ms = started.elapsed().as_millis(), "host descriptor heap made null");
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: BLOCK,
                    ..Default::default()
                })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            // (The twins are null buffer views.)
            let ends = if self.raw == 0 { [total, total] } else { [self.raw.min(total), total] };
            let mut done = 0;
            for (twin, end) in ends.into_iter().enumerate() {
                if done == end {
                    continue;
                }
                if self.sampler {
                    null_sampler(start);
                } else if twin == 1 {
                    null_buffer_view(start);
                } else {
                    null_view(start);
                }
                let mut filled = 1;
                while filled < BLOCK {
                    let n = filled.min(BLOCK - filled);
                    host.device.CopyDescriptorsSimple(n, at(filled), start, kind);
                    filled += n;
                }
                while done < end {
                    let n = BLOCK.min(end - done);
                    host.device.CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                    done += n;
                }
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    pub(super) fn null_sampler(&self, index: u32) {
        null_sampler(self.at(index));
        self.put(index);
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::DynamicAsked, 1);
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let enough = self.free.load(std::sync::atomic::Ordering::Relaxed) >= n
            && n < self.too_long.load(std::sync::atomic::Ordering::Relaxed);
        let n = n as usize;
        // Search from `next` on, then from the start (extending past cursor by
        // n - 1 so contiguous runs crossing the cursor are not missed).
        let cursor = *next as usize;
        let end = self.dynamic as usize;
        let second_end = cursor.saturating_add(n.saturating_sub(1)).min(end);
        let starts = (cursor..end).chain(0..second_end);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts.take(if enough { usize::MAX } else { 0 }) {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            if enough {
                self.too_long.fetch_min(n as u32, std::sync::atomic::Ordering::Relaxed);
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(n, "no free dynamic descriptor slots: a table is not made"));
            #[cfg(feature = "diagnostics")]
            {
                use crate::diagnostics::copy_stats::{C, add};
                add(C::DynamicRefused, 1);
                add(C::DynamicNs, started.elapsed().as_nanos() as u64);
            }
            return None;
        };
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::DynamicNs, started.elapsed().as_nanos() as u64);
        taken[first..first + n].fill(true);
        let prev_free = self.free.fetch_sub(n as u32, std::sync::atomic::Ordering::Relaxed);
        let used = self.dynamic.saturating_sub(prev_free.saturating_sub(n as u32));
        if used > self.peak_dynamic.load(std::sync::atomic::Ordering::Relaxed) {
            self.peak_dynamic.fetch_max(used, std::sync::atomic::Ordering::Relaxed);
        }
        *next = ((first + n) % end) as u32;
        Some(self.count + first as u32)
    }

    /// Notes a changed placed-heap page that reached past `self.count` on a capped heap.
    pub(crate) fn note_capped_raw_page(&self) {
        self.capped_raw_pages.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Diagnostic snapshot for a capped view heap (`title_count > count`):
    /// `(count, title_count, dynamic, peak_dynamic, highest_direct, highest_capped, [below_null, past_null, for_draw], raw_pages)`.
    pub(crate) fn capped_telemetry(&self) -> Option<(u32, u32, u32, u32, u32, u32, [u64; 3], u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.sampler || self.title_count <= self.count {
            return None;
        }
        let counts = [0, 1, 2].map(|i| self.capped_mirror[i].swap(0, Relaxed));
        let raw_pages = self.capped_raw_pages.swap(0, Relaxed);
        if counts == [0, 0, 0] && raw_pages == 0 {
            return None;
        }
        Some((
            self.count,
            self.title_count,
            self.dynamic,
            self.peak_dynamic.load(Relaxed),
            self.highest_direct.load(Relaxed),
            self.highest_capped.load(Relaxed),
            counts,
            raw_pages,
        ))
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Number of dynamic descriptor slots reserved past [`HostHeap::count`].
    pub(crate) fn dynamic_count(&self) -> u32 {
        self.dynamic
    }

    /// The GPU address of the table of element strides by title slot (view
    /// heaps).
    pub(crate) fn strides_gpu(&self) -> Option<u64> {
        self.strides.as_ref().map(|s| s.1)
    }

    /// The table's buffer, for a list to hold.
    pub(crate) fn strides_buffer(&self) -> Option<&ID3D12Resource> {
        self.strides.as_ref().map(|s| &s.0)
    }
}

