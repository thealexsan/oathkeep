//! Per-command-list draw state, setters, and dynamic slot lifecycle.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

use crate::host::HostList;
use crate::object::This;

use super::fixed::TargetKey;
use super::indirect_batch::{Batch, Window};
use super::plan::FreshTable;
use super::{LINE_LOOP, QUAD_STRIP, QUADS, RECTANGLES, TRIANGLE_FAN};

/// A root argument as the title set it.
#[derive(Debug, Clone, Default)]
pub(super) enum Root {
    #[default]
    None,
    /// A descriptor table's first descriptor (an address in a heap).
    Table(u64),
    Constants(Vec<u32>),
    /// A root view's GPU address.
    View(u64),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VertexView {
    pub(super) address: u64,
    pub(super) size: u32,
    pub(super) stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexView {
    pub(super) address: u64,
    pub(super) size: u32,
    pub(super) format: u32,
}

/// A root SRV's dynamic slot in a list, and the host copy it shows.
pub(super) type RootSlot = (u32, Option<std::sync::Arc<crate::shadow::Shadow>>);

/// What one title list has set, and what its host list was last given.
#[derive(Default)]
pub(crate) struct State {
    pub(super) pipeline: usize,
    /// View heap, sampler heap.
    pub(super) heaps: [usize; 2],
    pub(super) graphics: Vec<Root>,
    pub(super) compute: Vec<Root>,
    pub(super) vertex: [VertexView; 32],
    pub(super) index: IndexView,
    /// Bound render target and depth resources, for their host states.
    pub(super) targets: Vec<usize>,
    pub(super) depth: usize,
    /// The host state the bound depth view needs ([`super::fixed::depth_state`]).
    pub(super) depth_state: u32,
    /// The title's primitive topology (the host's may differ).
    pub(super) topology: u32,
    /// What the title set that goes straight onto the host list, kept for
    /// the host list that continues this one after a cut ([`State::replay`]):
    /// whether a topology was set, the render target and depth handles, the
    /// viewports, the scissors, the blend factor and the stencil reference.
    pub(super) topology_set: bool,
    pub(super) target_handles: Vec<u64>,
    pub(super) depth_handle: u64,
    pub(super) targets_set: bool,
    /// The render target views and the depth view the host list has, once
    /// it has any ([`State::set_targets`]): per view the host texture (0:
    /// a null view) and what its descriptor says.
    pub(super) host_targets: Option<(Vec<TargetKey>, Option<TargetKey>)>,
    pub(super) viewports: Vec<D3D12_VIEWPORT>,
    pub(super) scissors: Vec<RECT>,
    pub(super) blend: Option<[f32; 4]>,
    pub(super) stencil: Option<u32>,
    pub(super) depth_bounds: Option<[f32; 2]>,
    /// The indirect records clamped last, if later draws may still read
    /// them ([`super::indirect_batch::windows`]).
    pub(super) window: Option<Window>,
    /// The records clamped in a batch, if later executions may still read
    /// them ([`super::indirect_batch::batches`]).
    pub(super) batch: Option<Batch>,
    /// The user data bound last, of the last stage that has some (for the
    /// report of a removed device, `crate::host::indirects`).
    #[cfg(feature = "diagnostics")]
    pub(super) user: Vec<u32>,
    /// The tables the last bind made afresh for a program the stand-in for
    /// the indirect light follows (`crate::ambient`): root parameter, bytes
    /// into the view heap.
    pub(super) ambient_tables: Vec<(u8, u32)>,
    /// What the host list has: pipeline, root signature, heaps.
    pub(super) host_pipeline: usize,
    /// Graphics, compute.
    pub(super) host_roots: [usize; 2],
    pub(super) host_heaps: [usize; 2],
    pub(super) host_heaps_set: bool,
    /// Tables made afresh in this list: (table, slot count, contents) → first
    /// dynamic slot.
    pub(super) fresh_tables: std::collections::HashMap<
        (u64, u32, u64),
        FreshTable,
        std::hash::BuildHasherDefault<crate::host::AddressHash>,
    >,
    /// The dynamic slots the list holds: title heap, first, count; given
    /// back when the list has run, is reset or released.
    pub(super) dynamic: Vec<(usize, u32, u32)>,
    /// Root SRVs bound as one-view tables in this list: (title heap, title
    /// address; 0 for none) → dynamic slot, and the host copy the slot
    /// shows: later draws of the list bring that one up to date, whatever
    /// copy the address would find by then (`shadow.rs` `tails`).
    pub(super) root_srv_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// Root UAVs bound as one-view tables in this list, likewise.
    pub(super) root_uav_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// The list ran and its dynamic slots were given back.
    pub(super) tables_gone: bool,
    /// This recording, for [`crate::descriptor::host::HostHeap::make_listed`]
    /// (0: it has none yet).
    pub(super) recording: u64,
}

impl Drop for State {
    fn drop(&mut self) {
        self.release_root_window_slots();
        for &(heap, first, n) in &self.dynamic {
            crate::descriptor::release_dynamic(heap, first, n);
        }
    }
}

fn set(roots: &mut Vec<Root>, slot: u32, root: Root) {
    let slot = slot as usize;
    if roots.len() <= slot {
        roots.resize(slot + 1, Root::None);
    }
    roots[slot] = root;
}

impl State {
    /// Clears dynamic view-heap slots allocated for root SRVs/UAVs backed by
    /// `Shadow` windows outside buffers before returning those slots to the
    /// dynamic ring, so `HostHeap::kept` does not pin idle or superseded tail
    /// windows across the 262,144-slot ring.
    fn release_root_window_slots(&mut self) {
        for (&(heap_at, _), &(index, ref shadow)) in &self.root_srv_slots {
            if shadow.as_ref().is_some_and(|s| s.is_window()) {
                crate::descriptor::with_heap(heap_at, |h| {
                    if let Some(host) = h.host() {
                        host.null_raw_at(index);
                    }
                });
            }
        }
        for (&(heap_at, _), &(index, ref shadow)) in &self.root_uav_slots {
            if shadow.as_ref().is_some_and(|s| s.is_window()) {
                crate::descriptor::with_heap(heap_at, |h| {
                    if let Some(host) = h.host() {
                        host.null_raw_uav_at(index);
                    }
                });
            }
        }
    }

    /// The list's submission has run (submissions are waited for): the
    /// dynamic slots its tables were made in are given back, not kept
    /// until the title resets the list. A frame of the opening cutscene
    /// asks for 12,500 tables; held until the resets they ran the 131,072
    /// slots out, and up to 4,900 draws a frame went without their tables.
    /// shortcut: a list the title runs a second time without a reset then
    /// reads slots others may have taken (logged once).
    pub(crate) fn tables_done(&mut self) {
        if self.tables_gone && self.dynamic.is_empty() {
            return;
        }
        if self.tables_gone {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("a list ran again after its descriptor tables were given back"));
        }
        self.release_root_window_slots();
        for (heap, first, n) in self.dynamic.drain(..) {
            crate::descriptor::release_dynamic(heap, first, n);
        }
        self.fresh_tables.clear();
        self.recording = 0;
        self.root_srv_slots.clear();
        self.root_uav_slots.clear();
        self.tables_gone = true;
    }

    pub(crate) fn reset(&mut self, pipeline: This) {
        *self = State::default();
        self.pipeline = pipeline as usize;
    }

    /// The title's topology; console-only ones become host lists (see
    /// [`crate::primitive`]).
    pub(crate) fn set_topology(&mut self, list: &HostList, topology: u32) {
        self.topology = topology;
        self.topology_set = true;
        let host = match topology {
            RECTANGLES => 4,
            TRIANGLE_FAN | LINE_LOOP | QUADS | QUAD_STRIP => {
                once!(topology, "console topology drawn as a triangle list");
                4
            }
            t => t,
        };
        // SAFETY: an open list.
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
        };
    }

    pub(crate) fn set_pipeline(&mut self, pipeline: This) {
        self.pipeline = pipeline as usize;
    }

    pub(crate) fn set_heaps(&mut self, heaps: &[This]) {
        self.heaps = [0; 2];
        for &h in heaps {
            if !h.is_null() {
                let samplers = crate::descriptor::heap(h).samplers();
                self.heaps[usize::from(samplers)] = h as usize;
            }
        }
    }

    fn roots(&mut self, compute: bool) -> &mut Vec<Root> {
        if compute {
            &mut self.compute
        } else {
            &mut self.graphics
        }
    }

    pub(super) fn roots_ref(&self, compute: bool) -> &Vec<Root> {
        if compute {
            &self.compute
        } else {
            &self.graphics
        }
    }

    pub(crate) fn set_table(&mut self, compute: bool, slot: u32, handle: u64) {
        set(self.roots(compute), slot, Root::Table(handle));
    }

    pub(crate) fn set_view(&mut self, compute: bool, slot: u32, address: u64) {
        set(self.roots(compute), slot, Root::View(address));
    }

    pub(crate) fn set_constants(&mut self, compute: bool, slot: u32, values: &[u32], offset: u32) {
        let roots = self.roots(compute);
        let s = slot as usize;
        if !matches!(roots.get(s), Some(Root::Constants(_))) {
            set(roots, slot, Root::Constants(Vec::new()));
        }
        if let Root::Constants(c) = &mut roots[s] {
            let end = offset as usize + values.len();
            if c.len() < end {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(values);
        }
    }

    pub(crate) fn set_vertex_buffers(&mut self, start: u32, views: &[VertexView]) {
        for (i, v) in views.iter().enumerate() {
            if let Some(slot) = self.vertex.get_mut(start as usize + i) {
                *slot = *v;
            }
        }
    }

    pub(crate) fn clear_vertex_buffers(&mut self, start: u32, count: u32) {
        for slot in self.vertex.iter_mut().skip(start as usize).take(count as usize) {
            *slot = VertexView::default();
        }
    }

    pub(crate) fn set_index_buffer(&mut self, view: Option<&IndexView>) {
        self.index = view.copied().unwrap_or_default();
    }

    /// The bound heap holding `handle`.
    pub(super) fn heap_of(&self, handle: u64) -> Option<&'static crate::descriptor::Heap> {
        self.heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| h.index(handle).is_some())
    }
}
