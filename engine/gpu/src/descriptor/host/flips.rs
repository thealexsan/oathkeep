use std::sync::Mutex;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use super::{HostHeap, VIEW_TAG, View, host};
use super::null_views::null_for;

/// Levels on demand (`crate::tiles`): where the placed textures start
/// whose first level with memory changed since a texture pass last took
/// them ([`HostHeap::texture_views_remade`]).
pub(super) static RESHOWN: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The first level with memory of the placed texture at `start` changed:
/// its slots are made again at the next texture pass.
pub(crate) fn levels_changed(start: u64) {
    RESHOWN.lock().unwrap_or_else(|e| e.into_inner()).push(start);
}

/// [`levels_changed`] once the submissions begun so far (that of the list
/// whose step calls this among them) have run.
pub(crate) fn levels_changed_after(start: u64) {
    let after = crate::host::submissions().0;
    RESHOWN_AFTER.lock().unwrap_or_else(|e| e.into_inner()).push((after, start));
}

pub(super) static RESHOWN_AFTER: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Slots that show a placed texture from another level than the title's view
/// in them says now, and are made from that view when the upload it needs
/// has run ([`HostHeap::texture_slot_flip`]).
pub(super) struct Flip {
    /// The submission to wait for.
    pub(super) after: u64,
    /// The heap (its address), the slot, where its record is and which
    /// record it was.
    pub(super) heap: usize,
    pub(super) index: u32,
    pub(super) handle: u64,
    pub(super) key: u64,
    /// The frame it was asked in, of a slot that shows nothing of the
    /// texture until then ([`HostHeap::texture_slot_show_after`]).
    pub(super) late: Option<u64>,
}

/// Whether a title slot shows a texture that is new to it only once the
/// submission with its upload has finished (`OATHKEEP_SHOW_AFTER=1`; off).
/// Shown at once, the lists already on the card read it by index while it is
/// made: all three losses whose report had the address book (n08, n11, n15)
/// were page faults in a texture made and shown in the last tenth of a
/// second.
pub(crate) fn show_after() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_SHOW_AFTER").is_some_and(|v| v != "0"))
}

/// Slots shown late ([`show_after`]), the frames they waited, the longest;
/// those that were given the neutral texture meanwhile; those not shown any
/// more when their time came.
pub(super) static LATE: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// What a texture slot showed before its last write: its record (0: null or
/// the neutral texture), when (ms), and the submissions begun and finished.
#[cfg(feature = "diagnostics")]
pub(super) static BEFORE: Mutex<std::collections::BTreeMap<u32, (u64, u64, (u64, u64), &'static str)>> =
    Mutex::new(std::collections::BTreeMap::new());

#[cfg(feature = "diagnostics")]
pub(super) fn note_before(index: u32, was: u64, by: &'static str) {
    if crate::book::on() {
        let entry = (was, crate::host::now_ms(), crate::host::submissions(), by);
        BEFORE.lock().unwrap_or_else(|e| e.into_inner()).insert(index, entry);
    }
}

/// A 1x1 gray texture, in the place of one that is not there yet.
fn neutral() -> Option<&'static ID3D12Resource> {
    static TEXTURE: std::sync::OnceLock<Option<ID3D12Resource>> = std::sync::OnceLock::new();
    TEXTURE
        .get_or_init(|| {
            let host = host()?;
            let device = &host.device;
            let (from, data) = host.upload_buffer(256)?;
            // SAFETY: the buffer's mapping; plain creation and recording,
            // waited for before what it names goes.
            unsafe {
                std::ptr::write_bytes(data, 0x80, 256);
                let mut made: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                            Width: 1,
                            Height: 1,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            ..Default::default()
                        },
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut made,
                    )
                    .ok()?;
                let made = made?;
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COPY, ..Default::default() })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY).ok()?;
                let list: ID3D12GraphicsCommandList =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None).ok()?;
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&made),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        Width: 1,
                        Height: 1,
                        Depth: 1,
                        RowPitch: 256,
                    },
                };
                let source = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&from),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
                };
                list.CopyTextureRegion(&to, 0, 0, 0, &source, None);
                list.Close().ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                queue.ExecuteCommandLists(&[Some(windows::core::Interface::cast(&list).ok()?)]);
                queue.Signal(&fence, 1).ok()?;
                // (Without an event: this waits.)
                fence.SetEventOnCompletion(1, windows::Win32::Foundation::HANDLE::default()).ok()?;
                Some(made)
            }
        })
        .as_ref()
}

/// Makes the descriptor of [`neutral`] for a slot of `view` at `at`, if the
/// view is a 2D texture's or an array's; else a null one of its kind.
pub(super) fn neutral_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<ID3D12Resource> {
    const TEXTURE2D: u32 = 4;
    const TEXTURE2D_ARRAY: u32 = 5;
    let dimension = match view {
        View::Shader(_, Some(desc)) => desc[1],
        View::Placed(false, resource, desc) if resource[0] != 1 => desc[1],
        _ => 0,
    };
    let desc = match dimension {
        TEXTURE2D => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() },
            },
        },
        TEXTURE2D_ARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_SRV { MipLevels: 1, ArraySize: 1, ..Default::default() },
            },
        },
        _ => {
            null_for(view, at);
            return None;
        }
    };
    let (Some(texture), Some(host)) = (neutral(), host()) else {
        null_for(view, at);
        return None;
    };
    // SAFETY: a slot of our heap, a live texture.
    unsafe { host.device.CreateShaderResourceView(texture, Some(&desc), at) };
    Some(texture.clone())
}

pub(super) static FLIPS: Mutex<Vec<Flip>> = Mutex::new(Vec::new());

/// Deferred slot flips recorded into an open or not-yet-submitted command
/// list (`HostHeap::flip`), tracked so a one-shot decoration bake draw can
/// promote them out of the `neutral()` gray stand-in before sampling them.
pub(crate) struct PendingListFlip {
    pub(super) id: u64,
    pub(crate) origin: (usize, u64),
    pub(super) flip: Flip,
}

impl PendingListFlip {
    pub(crate) fn index(&self) -> u32 {
        self.flip.index
    }

    pub(crate) fn handle(&self) -> u64 {
        self.flip.handle
    }

    pub(crate) fn key(&self) -> u64 {
        self.flip.key
    }
}

pub(super) static NEXT_PENDING_FLIP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
pub(super) static PENDING_LIST_FLIPS: Mutex<Vec<PendingListFlip>> = Mutex::new(Vec::new());
pub(super) static PENDING_LIST_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(super) fn take_pending_list_flip(id: u64) -> Option<Flip> {
    if PENDING_LIST_COUNT.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return None;
    }
    let mut pending = PENDING_LIST_FLIPS.lock().unwrap_or_else(|e| e.into_inner());
    let pos = pending.iter().position(|p| p.id == id)?;
    let entry = pending.swap_remove(pos);
    PENDING_LIST_COUNT.store(pending.len(), std::sync::atomic::Ordering::Release);
    Some(entry.flip)
}

pub(super) fn remove_pending_list_flip(id: u64) {
    let _ = take_pending_list_flip(id);
}

/// Makes the slots whose new texture's upload has run
/// ([`HostHeap::texture_slots_flip`]); on any thread. Not if the heap is at
/// something else: the next texture pass does.
pub(crate) fn flip_now() {
    let _ = crate::descriptor::with_view_heap(HostHeap::texture_slots_flip);
}

/// Since the start: slots made from their new view after its upload had run,
/// and those the title had written again by then.
pub(crate) static FLIPPED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The key of the record at `handle`, a view's descriptor slot; 0: none.
pub(super) fn record_key(handle: u64) -> u64 {
    // SAFETY: the handle is a descriptor slot in title memory.
    let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
    if tag == VIEW_TAG { u64::from(generation) << 32 | u64::from(i) } else { 0 }
}

/// Levels on demand (`crate::tiles`): where the placed textures start
/// whose last upload left a held level without tiles (the card had no
/// room, or no heap could be had), and when (`crate::host::moment`). A
/// texture pass makes their slots due again once the card has room
/// ([`HostHeap::texture_slots_waiting`]).
pub(super) static WAITING: Mutex<std::collections::BTreeMap<u64, u64>> = Mutex::new(std::collections::BTreeMap::new());

/// The last upload of the placed texture at `start` left a held level
/// without tiles. A texture picked by index is uploaded once and then
/// only when its memory is found changed, and its staging buffer goes
/// with the upload: without this the level stayed empty (the texture
/// blurred, or black if no level had tiles) for as long as the title left
/// its memory alone, where a texture refused for want of room is tried
/// again.
pub(crate) fn levels_waiting(start: u64) {
    WAITING.lock().unwrap_or_else(|e| e.into_inner()).insert(start, crate::host::moment());
}

/// Placed textures (by the start of their memory) filled while the runtime
/// was remapping with less than was mapped of them
/// (`crate::texture::fill_unlocked`), whose slots the next texture pass
/// makes due again; and how often in a row that was, which ends it: memory
/// that is shown and cannot be read would be uploaded again every frame.
pub(super) static SHORT: Mutex<(std::collections::BTreeSet<u64>, std::collections::BTreeMap<u64, u32>)> =
    Mutex::new((std::collections::BTreeSet::new(), std::collections::BTreeMap::new()));

/// Fillings in a row of one texture that got less than was mapped, after
/// which its slots wait for the next notice of its memory.
pub(super) const SHORT_IN_A_ROW: u32 = 8;

/// The placed texture at `start` was filled with less than was mapped.
pub(crate) fn read_short(start: u64) {
    let mut short = SHORT.lock().unwrap_or_else(|e| e.into_inner());
    let times = short.1.entry(start).or_insert(0);
    *times += 1;
    if *times <= SHORT_IN_A_ROW {
        short.0.insert(start);
    }
}

/// The placed texture at `start` was filled with all that was mapped.
pub(crate) fn read_all(start: u64) {
    let mut short = SHORT.lock().unwrap_or_else(|e| e.into_inner());
    if !short.1.is_empty() {
        short.1.remove(&start);
    }
}

