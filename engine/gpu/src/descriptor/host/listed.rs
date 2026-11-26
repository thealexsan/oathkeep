use windows::Win32::Graphics::Direct3D12::*;
use super::{View, host};
use super::staged::Kept;

/// What is known of a record a list's tables hold, in that recording of the
/// list ([`HostHeap::make_listed`]).
#[derive(Clone, Copy)]
pub(crate) struct Listed {
    pub(crate) kind: Option<oathkeep_shader::native::ResourceType>,
    pub(crate) texture: bool,
    /// Read at this epoch of the list ([`crate::host::HostList::epoch`]) with
    /// this depth target tested: up to date; and whether it is that target's.
    pub(crate) ready: (u64, usize, bool),
    /// The title's slot last noted for it, and whether the draw read it.
    pub(crate) tabled: Option<(u32, bool)>,
}

pub(super) struct ListView {
    pub(super) view: View,
    /// Its descriptor's slot in [`ListViews`]; its raw twin's is the next.
    pub(super) at: u32,
    pub(super) twin: bool,
    pub(super) stride_word: u32,
    pub(super) kept: Kept,
    pub(super) listed: Listed,
}

/// The descriptors a thread made for one recording of a list, by the record
/// they were made from, in a heap the card does not see.
///
/// The title's culling and lighting dispatches each bind a table of their own
/// that differs in one constant buffer, and the same thirty views beside it:
/// each was found, checked, made and brought up to date anew for every
/// dispatch, some 2,500 indirect dispatches in the one recording job the
/// frame waits for last (timeline waits-2: 24 of 65 ms). Here a list makes a
/// view once, as it makes a table once (`crate::draw::State::tables`), and
/// copies its descriptor.
pub(super) struct ListViews {
    pub(super) recording: u64,
    heap: ID3D12DescriptorHeap,
    step: usize,
    start: usize,
    pub(super) made: std::collections::HashMap<[u32; 3], ListView, std::hash::BuildHasherDefault<crate::host::AddressHash>>,
}

impl ListViews {
    pub(super) const SLOTS: u32 = 8192;

    pub(super) fn new() -> Option<ListViews> {
        let host = host()?;
        // SAFETY: plain object creation.
        unsafe {
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: Self::SLOTS,
                    ..Default::default()
                })
                .ok()?;
            let step = host.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let start = heap.GetCPUDescriptorHandleForHeapStart().ptr;
            Some(ListViews { recording: 0, heap, step, start, made: Default::default() })
        }
    }

    pub(super) fn slot(&self, i: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        debug_assert!(i < Self::SLOTS && !windows::core::Interface::as_raw(&self.heap).is_null());
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.start + i as usize * self.step }
    }
}

thread_local! {
    pub(super) static LIST_VIEWS: std::cell::RefCell<Option<ListViews>> = const { std::cell::RefCell::new(None) };
}

/// The view the record `words` names, as this thread made it for `recording`.
pub(crate) fn listed_view(recording: u64, words: [u32; 3]) -> Option<View> {
    LIST_VIEWS.with(|views| {
        let views = views.borrow();
        let views = views.as_ref().filter(|v| v.recording == recording)?;
        views.made.get(&words).map(|m| m.view.clone())
    })
}

/// Notes what is known of the record `words` in `recording`.
pub(crate) fn note_listed(recording: u64, words: [u32; 3], listed: Listed) {
    LIST_VIEWS.with(|views| {
        if let Some(views) = views.borrow_mut().as_mut().filter(|v| v.recording == recording)
            && let Some(made) = views.made.get_mut(&words)
        {
            made.listed = listed;
        }
    });
}

/// Whether a list makes a view its tables hold once (`OATHKEEP_LIST_VIEWS=0`:
/// for every table).
pub(crate) fn list_views() -> bool {
    crate::toggled!("OATHKEEP_LIST_VIEWS")
}

