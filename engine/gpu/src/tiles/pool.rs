use std::sync::Mutex;
use std::sync::atomic::Ordering;

use windows::Win32::Graphics::Direct3D12::*;

use super::{HEAP_TILES, STATS, TILE};

/// One heap of the pool and its free runs of tiles (first tile, tiles), in
/// order, no two touching.
struct Heap {
    heap: ID3D12Heap,
    free: Vec<(u32, u32)>,
}

// SAFETY: a heap of the device, named only in queue and device calls, which
// any thread may make; its free runs are plain numbers behind `POOL`'s lock.
unsafe impl Send for Heap {}

impl Heap {
    fn free_tiles(&self) -> u32 {
        self.free.iter().map(|run| run.1).sum()
    }
}

/// The pool's heaps by place; a place is empty after its heap went back to
/// the card (a texture names its heaps by place).
static POOL: Mutex<Vec<Option<Heap>>> = Mutex::new(Vec::new());

/// `count` tiles of one heap, in as many runs as it takes, the lowest free
/// tiles first (single free tiles are used up before a long run is cut):
/// the heap, its place in the pool and the runs (first tile, tiles). A new
/// heap is made when no heap has that many free, unless `no_grow` is true.
pub(super) fn take(device: &ID3D12Device, count: u32, no_grow: bool) -> Option<(ID3D12Heap, usize, Vec<(u32, u32)>)> {
    if count == 0 || count > HEAP_TILES {
        return None;
    }
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let found = pool.iter().position(|h| h.as_ref().is_some_and(|h| h.free_tiles() >= count));
    let index = match found {
        Some(index) => index,
        None if no_grow => return None,
        None => {
            let desc = D3D12_HEAP_DESC {
                SizeInBytes: u64::from(HEAP_TILES) * TILE,
                Properties: D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | crate::host::not_resident(),
            };
            let mut made: Option<ID3D12Heap> = None;
            // SAFETY: plain creation; the out pointer is ours.
            if let Err(e) = unsafe { device.CreateHeap::<ID3D12Heap>(&raw const desc, &raw mut made) } {
                crate::residency::allocation_error(&e);
                tracing::warn!(error = %e, "no heap for texture levels given on demand: those levels stay empty");
                return None;
            }
            let heap = made?;
            if let Err(e) = crate::host::resident(device, &heap).and_then(|()| crate::host::mapped(device)) {
                tracing::warn!(error = %e, "a heap for texture levels given on demand is not resident");
                return None;
            }
            let fresh = Heap {
                heap,
                free: vec![(0, HEAP_TILES)],
            };
            STATS[3].fetch_add(u64::from(HEAP_TILES) * TILE, Ordering::Relaxed);
            match pool.iter().position(Option::is_none) {
                Some(index) => {
                    pool[index] = Some(fresh);
                    index
                }
                None => {
                    pool.push(Some(fresh));
                    pool.len() - 1
                }
            }
        }
    };
    let h = pool[index].as_mut()?;
    let mut runs = Vec::new();
    let mut left = count;
    while left > 0 {
        let &(start, tiles) = h.free.first()?;
        let part = tiles.min(left);
        runs.push((start, part));
        if part == tiles {
            h.free.remove(0);
        } else {
            h.free[0] = (start + part, tiles - part);
        }
        left -= part;
    }
    Some((h.heap.clone(), index, runs))
}

/// Gives the runs `runs` of the pool's heap at `index` back; runs that
/// touch become one. A heap with every tile free goes back to the card,
/// unless it is the pool's only one. (Called when the texture that held
/// the tiles is dropped: nothing on the card reads through its mappings
/// any more, as for any host texture that is dropped.)
pub(super) fn give_back(index: usize, runs: &[(u32, u32)]) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(h)) = pool.get_mut(index) else {
        return;
    };
    h.free.extend_from_slice(runs);
    h.free.sort_unstable();
    let mut joined: Vec<(u32, u32)> = Vec::with_capacity(h.free.len());
    for &(from, tiles) in &h.free {
        match joined.last_mut() {
            Some(last) if last.0 + last.1 == from => last.1 += tiles,
            _ => joined.push((from, tiles)),
        }
    }
    h.free = joined;
    let all_free = h.free_tiles() == HEAP_TILES;
    if all_free && pool.iter().flatten().count() > 1 {
        pool[index] = None;
        STATS[3].fetch_sub(u64::from(HEAP_TILES) * TILE, Ordering::Relaxed);
        STATS[8].fetch_add(1, Ordering::Relaxed);
    }
}
