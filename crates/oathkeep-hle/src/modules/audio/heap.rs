use std::alloc::Layout;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::Mutex;

use super::super::xasync::E_INVALIDARG;
use super::super::xgameruntime::{HRESULT, S_OK};
use super::xma::{XMA, Xma};
use super::{E_FAIL, E_OUTOFMEMORY, lock};

// The APU heap.

struct Block {
    size: usize,
    apu: u32,
    layout: Layout,
}

pub(super) struct Heap {
    /// By host address.
    blocks: BTreeMap<usize, Block>,
    /// Host address by device address.
    by_apu: BTreeMap<u32, usize>,
    /// Free device address ranges, start to end, merged with neighbours.
    pub(super) free: BTreeMap<u32, u32>,
    /// Set by `ApuCreateHeap` or the first allocation: the heap exists.
    created: bool,
}

pub(super) static HEAP: Mutex<Heap> = Mutex::new(Heap {
    blocks: BTreeMap::new(),
    by_apu: BTreeMap::new(),
    free: BTreeMap::new(),
    created: false,
});

/// Device addresses start above 0, which means "none" to titles.
const APU_BASE: u32 = 0x0001_0000;

impl Heap {
    /// First fit in the free ranges.
    fn take(&mut self, size: u32, align: u32) -> Option<u32> {
        if self.free.is_empty() && !self.created {
            self.free.insert(APU_BASE, u32::MAX);
        }
        self.created = true;
        let (start, end, apu) = self.free.iter().find_map(|(&start, &end)| {
            let apu = start.checked_next_multiple_of(align)?;
            (apu.checked_add(size)? <= end).then_some((start, end, apu))
        })?;
        self.free.remove(&start);
        if start < apu {
            self.free.insert(start, apu);
        }
        if apu + size < end {
            self.free.insert(apu + size, end);
        }
        Some(apu)
    }

    fn give_back(&mut self, mut start: u32, mut end: u32) {
        if let Some((&before, &before_end)) = self.free.range(..start).next_back()
            && before_end == start
        {
            self.free.remove(&before);
            start = before;
        }
        if let Some(after_end) = self.free.remove(&end) {
            end = after_end;
        }
        self.free.insert(start, end);
    }
}

/// Zeroed host memory and its device address.
fn heap_alloc(size: usize, align: usize) -> Option<(*mut u8, u32)> {
    let layout = Layout::from_size_align(size.max(1), align.max(16)).ok()?;
    let mut heap = lock(&HEAP);
    let apu = heap.take(u32::try_from(layout.size()).ok()?, align as u32)?;
    // SAFETY: non-zero size.
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        heap.give_back(apu, apu + layout.size() as u32);
        return None;
    }
    heap.blocks
        .insert(ptr as usize, Block { size, apu, layout });
    heap.by_apu.insert(apu, ptr as usize);
    // The big ones are named in the log: they are the title's audio memory
    // given out of our allocator, which the memory line counts as ours.
    if size >= 32 << 20 {
        tracing::info!(mb = size >> 20, at = ?ptr, "audio memory for the title from our allocator");
    }
    Some((ptr, apu))
}

fn heap_free(ptr: *mut u8) {
    let mut heap = lock(&HEAP);
    let Some(block) = heap.blocks.remove(&(ptr as usize)) else {
        return;
    };
    heap.by_apu.remove(&block.apu);
    heap.give_back(block.apu, block.apu + block.layout.size() as u32);
    drop(heap);
    // SAFETY: allocated by `heap_alloc` with this layout.
    unsafe { std::alloc::dealloc(ptr, block.layout) };
}

/// Sizes the heap once, before anything is allocated from it. The console
/// reserves both parts up front; we allocate per block, so the sizes only
/// decide whether the call is valid.
// shortcut: the cached and non-cached sizes are not enforced as limits;
// enforce them if a title relies on running out.
pub extern "system" fn ApuCreateHeap(cached: u32, _non_cached: u32) -> HRESULT {
    const ALREADY_ALLOCATED: HRESULT = 0x8ACA_0001_u32 as i32;
    if cached == 0 {
        return E_INVALIDARG;
    }
    let mut heap = lock(&HEAP);
    if heap.created {
        return ALREADY_ALLOCATED;
    }
    heap.created = true;
    heap.free.insert(APU_BASE, u32::MAX);
    S_OK
}

/// The host address of a device address, null outside the heap.
pub extern "system" fn ApuMapApuAddress(apu: u32) -> *mut c_void {
    heap_unmap(apu).map_or(std::ptr::null_mut(), |p| p.cast())
}

/// The device address of any byte of a heap block, 0 elsewhere.
fn heap_map(ptr: usize) -> u32 {
    let heap = lock(&HEAP);
    match heap.blocks.range(..=ptr).next_back() {
        Some((&start, block)) if ptr < start + block.size.max(1) => {
            block.apu + (ptr - start) as u32
        }
        _ => 0,
    }
}

/// The host address of a device address, if a heap block holds it.
pub(super) fn heap_unmap(apu: u32) -> Option<*mut u8> {
    let heap = lock(&HEAP);
    let (&start, &host) = heap.by_apu.range(..=apu).next_back()?;
    let block = &heap.blocks[&host];
    (apu < start.saturating_add(block.size.max(1) as u32))
        .then(|| (host + (apu - start) as usize) as *mut u8)
}

/// `len` bytes of title memory at a device address, if one heap block holds
/// them all.
pub(super) fn heap_bytes<'a>(apu: u32, len: usize) -> Option<&'a mut [u8]> {
    let heap = lock(&HEAP);
    let (&start, &host) = heap.by_apu.range(..=apu).next_back()?;
    let offset = (apu - start) as usize;
    (offset + len <= heap.blocks[&host].size)
        // SAFETY: inside a live heap block, which the title and the audio
        // hardware share like on the console.
        .then(|| unsafe { std::slice::from_raw_parts_mut((host + offset) as *mut u8, len) })
}

pub extern "system" fn ApuAlloc(
    out: *mut *mut c_void,
    apu: *mut u32,
    size: u32,
    align: u32,
    _flags: u32,
) -> HRESULT {
    if out.is_null() || size == 0 || align < 4 || !align.is_power_of_two() {
        return E_INVALIDARG;
    }
    let Some((ptr, address)) = heap_alloc(size as usize, align as usize) else {
        return E_OUTOFMEMORY;
    };
    // SAFETY: checked non-null; optional out parameter checked below.
    unsafe {
        out.write(ptr.cast());
        if !apu.is_null() {
            apu.write(address);
        }
    }
    S_OK
}

pub extern "system" fn ApuFree(ptr: *mut c_void) {
    heap_free(ptr.cast());
}

pub extern "system" fn ApuMapVirtualAddress(ptr: *const c_void) -> u32 {
    heap_map(ptr as usize)
}

pub extern "system" fn ApuIsVirtualAddressValid(ptr: *const c_void, align: u32) -> bool {
    let apu = heap_map(ptr as usize);
    apu != 0 && (align == 0 || apu.is_multiple_of(align))
}

// SHAPE context arrays.

/// Six counts (rate conversion, EQ/compression, filter/volume, DMA, XMA,
/// PCM), then the six arrays and their device addresses.
#[repr(C, packed)]
pub struct ShapeContexts {
    pub(super) counts: [u32; 6],
    pub(super) arrays: [*mut c_void; 6],
    pub(super) apu: [u32; 6],
}

/// Bytes per context: rate conversion, DMA and XMA as fmod clears them;
/// the others (not seen yet) get a generous upper bound.
const CONTEXT_BYTES: [usize; 6] = [32, 256, 256, 16, 64, 256];

static CONTEXTS: Mutex<Option<[usize; 6]>> = Mutex::new(None);

pub extern "system" fn AcpHalAllocateShapeContexts(contexts: *mut ShapeContexts) -> HRESULT {
    if contexts.is_null() {
        return E_INVALIDARG;
    }
    let mut allocated = lock(&CONTEXTS);
    if allocated.is_some() {
        return E_FAIL;
    }
    // SAFETY: the title passes its request; packed fields are copied out.
    let counts = unsafe { std::ptr::addr_of!((*contexts).counts).read_unaligned() };
    let mut arrays: [*mut c_void; 6] = [std::ptr::null_mut(); 6];
    let mut addresses = [0u32; 6];
    for i in 0..6 {
        if counts[i] == 0 {
            continue;
        }
        let Some((ptr, apu)) = heap_alloc(counts[i] as usize * CONTEXT_BYTES[i], 256) else {
            arrays.iter().for_each(|&p| heap_free(p.cast()));
            return E_OUTOFMEMORY;
        };
        (arrays[i], addresses[i]) = (ptr.cast(), apu);
    }
    // SAFETY: as above.
    unsafe {
        std::ptr::addr_of_mut!((*contexts).arrays).write_unaligned(arrays);
        std::ptr::addr_of_mut!((*contexts).apu).write_unaligned(addresses);
    }
    *allocated = Some(arrays.map(|p| p as usize));
    let mut xma = XMA.write().unwrap_or_else(|e| e.into_inner());
    xma.array = arrays[4] as usize;
    xma.voices = (0..counts[4]).map(|_| Mutex::new(None)).collect();
    S_OK
}

pub extern "system" fn AcpHalReleaseShapeContexts() -> HRESULT {
    match lock(&CONTEXTS).take() {
        Some(arrays) => {
            *XMA.write().unwrap_or_else(|e| e.into_inner()) = Xma::EMPTY;
            arrays.iter().for_each(|&p| heap_free(p as *mut u8));
            S_OK
        }
        None => E_FAIL,
    }
}
