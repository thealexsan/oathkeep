use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use crate::host::host;

use super::{CbvSlabSlot, Shadow};

/// Shared 4 MiB 256-byte-aligned slab pool for unaligned constant buffer views.
const CBV_SLAB_BYTES: u64 = 4 << 20;
/// Maximum D3D12 CBV size (64 KiB).
pub(super) const CBV_SLAB_MAX_SLOT: u64 = 64 << 10;

struct CbvSlab {
    buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
    next: u64,
    cap: u64,
    live: usize,
}

#[derive(Default)]
struct CbvSlabPool {
    slabs: Vec<CbvSlab>,
    free: BTreeMap<u64, Vec<(usize, u64)>>,
}

static CBV_SLAB_POOL: Mutex<CbvSlabPool> = Mutex::new(CbvSlabPool {
    slabs: Vec::new(),
    free: BTreeMap::new(),
});

/// 256-byte-aligned CBV shadows keyed by guest start address.
static ALIGNED_CBV: RwLock<BTreeMap<u64, Arc<Shadow>>> = RwLock::new(BTreeMap::new());
static ALIGNED_CBV_LEN: AtomicUsize = AtomicUsize::new(0);

/// Pure slab-sizing rule for an unaligned CBV request: clamps to the 64 KiB
/// D3D12 CBV limit and rounds up to 256 bytes. Returns `None` for empty/overflow.
pub(super) fn cbv_slab_sizes(address: u64, len: u64) -> Option<(u64, u64)> {
    if address == 0 || len == 0 {
        return None;
    }
    let need = len.min(CBV_SLAB_MAX_SLOT);
    address.checked_add(need)?;
    let aligned = need.checked_next_multiple_of(256)?;
    Some((need, aligned))
}

pub(super) fn release_cbv_slab_slot(slot: CbvSlabSlot) {
    let mut pool = CBV_SLAB_POOL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(slab) = pool.slabs.get_mut(slot.slab_idx) else {
        return;
    };
    slab.live = slab.live.saturating_sub(1);
    // `Shadow::drop` runs before `self.buffer` is dropped: `pool` (1) +
    // `self.buffer` (1) + remaining `slab.live` shadows own all internal refs.
    // Only recycle slab space when zero descriptor slots or command lists hold
    // external references to this slab's `ID3D12Resource` (prevents CBV slab UAF).
    let internal_refs = 2u32.saturating_add(slab.live as u32);
    if crate::host::references(&slab.buffer) <= internal_refs {
        if slab.live == 0 {
            slab.next = 0;
            pool.free.retain(|_, list| {
                list.retain(|&(idx, _)| idx != slot.slab_idx);
                !list.is_empty()
            });
        } else {
            pool.free
                .entry(slot.size)
                .or_default()
                .push((slot.slab_idx, slot.offset));
        }
    }
}

fn alloc_cbv_slab(aligned_size: u64) -> Option<(ID3D12Resource, *mut u8, u64, Option<CbvSlabSlot>)> {
    if aligned_size == 0 || aligned_size > CBV_SLAB_MAX_SLOT {
        let (buffer, data) = host()?.shared_buffer(aligned_size.max(256))?;
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        return Some((buffer, data, gpu, None));
    }
    {
        let mut pool = CBV_SLAB_POOL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((slab_idx, offset)) = pool.free.get_mut(&aligned_size).and_then(Vec::pop) {
            let slab = &mut pool.slabs[slab_idx];
            slab.live += 1;
            return Some((
                slab.buffer.clone(),
                (slab.data + offset as usize) as *mut u8,
                slab.gpu + offset,
                Some(CbvSlabSlot {
                    slab_idx,
                    offset,
                    size: aligned_size,
                }),
            ));
        }
        if let Some((slab_idx, slab)) = pool
            .slabs
            .iter_mut()
            .enumerate()
            .rev()
            .find(|(_, s)| s.cap.saturating_sub(s.next) >= aligned_size)
        {
            let offset = slab.next;
            slab.next += aligned_size;
            slab.live += 1;
            return Some((
                slab.buffer.clone(),
                (slab.data + offset as usize) as *mut u8,
                slab.gpu + offset,
                Some(CbvSlabSlot {
                    slab_idx,
                    offset,
                    size: aligned_size,
                }),
            ));
        }
    }
    let (buffer, data) = host()?.shared_buffer(CBV_SLAB_BYTES)?;
    crate::host::name(&buffer, || {
        format!("aligned CBV slab ({} KiB)", CBV_SLAB_BYTES >> 10)
    });
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let mut pool = CBV_SLAB_POOL.lock().unwrap_or_else(|e| e.into_inner());
    let slab_idx = pool.slabs.len();
    pool.slabs.push(CbvSlab {
        buffer: buffer.clone(),
        data: data as usize,
        gpu,
        next: aligned_size,
        cap: CBV_SLAB_BYTES,
        live: 1,
    });
    let slab_count = pool.slabs.len();
    drop(pool);
    tracing::info!(
        slab_count,
        slab_kib = CBV_SLAB_BYTES >> 10,
        "aligned CBV slab allocated"
    );
    Some((
        buffer,
        data,
        gpu,
        Some(CbvSlabSlot {
            slab_idx,
            offset: 0,
            size: aligned_size,
        }),
    ))
}

#[inline]
pub(crate) fn has_aligned_cbv() -> bool {
    ALIGNED_CBV_LEN.load(Ordering::Relaxed) != 0
}

/// Looks up an existing 256-byte-aligned CBV slab shadow covering `[address, address + len)`.
pub(crate) fn find_aligned_cbv_existing(address: u64, len: u64) -> Option<Arc<Shadow>> {
    if !has_aligned_cbv() {
        return None;
    }
    let (need, _) = cbv_slab_sizes(address, len)?;
    let map = ALIGNED_CBV.read().unwrap_or_else(|e| e.into_inner());
    let s = map.get(&address)?;
    if s.len >= need {
        s.used.store(crate::host::moment(), Ordering::Relaxed);
        return Some(s.clone());
    }
    None
}

/// Returns or sub-allocates an exact-size 256-byte-aligned CBV shadow from the
/// shared 4 MiB CBV slab pool for an unaligned `View::Constant`.
pub(crate) fn find_aligned_cbv(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let (need, aligned) = cbv_slab_sizes(address, len)?;
    if let Some(s) = find_aligned_cbv_existing(address, need) {
        return Some(s);
    }
    let (buffer, data, gpu, slab_slot) = alloc_cbv_slab(aligned)?;
    let candidate = Arc::new(Shadow::new_suballocated(
        buffer, aligned, data, gpu, address, need, slab_slot,
    ));
    candidate.used.store(crate::host::moment(), Ordering::Relaxed);
    let mut map = ALIGNED_CBV.write().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(&address)
        && existing.len >= need
    {
        existing.used.store(crate::host::moment(), Ordering::Relaxed);
        return Some(existing.clone());
    }
    if let Some(old) = map.insert(address, candidate.clone()) {
        crate::host::retire(old);
    }
    let total = map.len();
    ALIGNED_CBV_LEN.store(total, Ordering::Release);
    if total <= 8 || total.is_power_of_two() {
        tracing::info!(
            view_start = format_args!("{address:#x}"),
            view_len = need,
            aligned_size = aligned,
            aligned_cbv_shadows = total,
            "aligned CBV slab shadow created"
        );
    }
    Some(candidate)
}

/// Evicts idle aligned CBV shadows only when zero descriptor slots or in-flight
/// command lists reference their backing resource (UAF-safe).
pub(super) fn let_go_idle_aligned_cbv(idle_ms: u64, idle_frames: u64) {
    if !has_aligned_cbv() {
        return;
    }
    let gone: Vec<Arc<Shadow>> = {
        let mut map = ALIGNED_CBV.write().unwrap_or_else(|e| e.into_inner());
        if map.is_empty() {
            return;
        }
        let mut evict_keys = Vec::new();
        let mut slab_counts: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
        for (&addr, s) in map.iter() {
            let is_idle = Arc::strong_count(s) == 1
                && crate::host::long_ago(s.used.load(Ordering::Relaxed), idle_ms, idle_frames);
            match &s.cbv_slab {
                None => {
                    if is_idle && crate::host::references(&s.buffer) == 1 {
                        evict_keys.push(addr);
                    }
                }
                Some(slot) => {
                    let entry = slab_counts.entry(slot.slab_idx).or_insert((0, 0));
                    entry.0 += 1;
                    if is_idle {
                        entry.1 += 1;
                    }
                }
            }
        }
        let mut reclaim_slabs = Vec::new();
        if !slab_counts.is_empty() {
            let pool = CBV_SLAB_POOL.lock().unwrap_or_else(|e| e.into_inner());
            for (slab_idx, (in_map, idle_in_map)) in slab_counts {
                if let Some(slab) = pool.slabs.get(slab_idx)
                    && slab.live > 0
                    && in_map == slab.live
                    && idle_in_map == slab.live
                    && crate::host::references(&slab.buffer) <= 1u32.saturating_add(slab.live as u32)
                {
                    reclaim_slabs.push(slab_idx);
                }
            }
        }
        if !reclaim_slabs.is_empty() {
            for (&addr, s) in map.iter() {
                if let Some(slot) = &s.cbv_slab
                    && reclaim_slabs.contains(&slot.slab_idx)
                {
                    evict_keys.push(addr);
                }
            }
        }
        let evicted: Vec<Arc<Shadow>> = evict_keys
            .into_iter()
            .filter_map(|k| map.remove(&k))
            .collect();
        ALIGNED_CBV_LEN.store(map.len(), Ordering::Release);
        evicted
    };
    for shadow in gone {
        crate::host::retire(shadow);
    }
}
