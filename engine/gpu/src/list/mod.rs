//! Command allocators and command lists.
//!
//! The title's inlined code writes packets into a list only while its put
//! pointer is below a limit, and calls the list's method otherwise. Our
//! lists keep the pointer and every limit at zero, so every command reaches
//! the table as a call.
//!
//! Each list records onto a host command list as its methods are called
//! (decision 0007); draw state lives in `crate::draw`. Methods not
//! written yet log once with their caller, so a run lists what to do next.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::mem::offset_of;

use crate::object::{
    self, E_INVALIDARG, HRESULT, Iid, LIST, Object, S_OK, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

mod allocator;
mod barrier;
mod clear;
mod copy;
#[cfg(windows)]
mod detach;
#[cfg(windows)]
mod read_back;
mod state;
#[cfg(test)]
mod tests;
#[cfg(windows)]
pub(crate) mod thin;
mod wait;

#[cfg(test)]
use allocator::ALLOCATOR_IIDS;
use allocator::{Upload, allocate_upload};
pub(crate) use allocator::create_allocator;
use barrier::{Barrier, barrier, barrier_groups, sync};
#[cfg(windows)]
pub(crate) use barrier::ensure_state;
use clear::{Rect, clear_dsv, clear_rtv, clear_uav_float, clear_uav_uint};
use copy::{CopyLocation, copy_buffer_region, copy_memory, copy_resource, copy_resource_x, copy_texture_region, copy_texture_region_x, resolve};
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) use copy::SELF_COPIES;
#[cfg(windows)]
use detach::{Part, fresh_list};
#[cfg(windows)]
pub(crate) use detach::{Detached, cut_list, detach, detach_parts, recycle};
#[cfg(not(windows))]
use state::{dispatch, draw};
use state::{dispatch_x, driver_hint, execute_indirect, query_mark, resolve_queries, state as draw_state};
pub(crate) use state::{take_counts, take_indirect};
#[cfg(windows)]
pub(crate) use wait::waits_by_list;
pub(crate) use wait::{ValueWait, unmet_waits};
use wait::{wait_value, wait_value64, write32, write64};

/// The list interfaces up to `ID3D12GraphicsCommandList6` (the table has
/// all their slots): those whose ids are the same as on PC, the console's
/// own List5 and List6 (the title asks for List6), then the
/// device child's.
const LIST_IIDS: [Iid; 10] = [
    guid(0x7116d91c, 0xe7e4, 0x47ce, [0xb8, 0xc6, 0xec, 0x81, 0x68, 0xf4, 0x37, 0xe5]),
    guid(0x5b160d0f, 0xac1b, 0x4185, [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55]),
    guid(0x553103fb, 0x1fe7, 0x4557, [0xbb, 0x38, 0x94, 0x6d, 0x7d, 0x0e, 0x7c, 0xa7]),
    guid(0x38c3e585, 0xff17, 0x412c, [0x91, 0x50, 0x4f, 0xc6, 0xf9, 0xd7, 0x2a, 0x28]),
    guid(0x6fda83a7, 0xb84c, 0x4e38, [0x9a, 0xc8, 0xc7, 0xbd, 0x22, 0x01, 0x6b, 0x3d]),
    guid(0x8754318e, 0xd3a9, 0x4541, [0x98, 0xcf, 0x64, 0x5b, 0x50, 0xdc, 0x48, 0x74]),
    guid(0x55050859, 0x4024, 0x474c, [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74]),
    guid(0xc3827890, 0x4024, 0x474c, [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74]),
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// The layout the title's inlined code reads.
#[repr(C)]
struct List {
    object: Object,
    kind: u32,
    /// The put pointer, then the plain, draw, API and unbiased API limits:
    /// all zero, so every inline check fails.
    put: [u64; 5],
    reserved: [u32; 64],
    /// State the inline code keeps for building packets; unused by us.
    graphics_root_headers: [u32; 128],
    compute_root_headers: [u32; 128],
    pipeline_shadow: [u32; 8],
    /// Newer titles' lists are larger: the title's state flush
    /// (title+0x32786) copies a pipeline's 32 bytes to +0x544..+0x564 and
    /// tests +0x548, so its layout runs past 0x560. Its size is unknown;
    /// this keeps our fields clear of whatever the title writes there.
    newer: [u32; 256],
    /// Ours, after the console layout: memory handed out by
    /// [`allocate_upload`], kept until the list is reset; the host list the
    /// calls record into (decision 0007), and the draw state the title set.
    uploads: std::sync::Mutex<Vec<Upload>>,
    #[cfg(windows)]
    host: Option<crate::host::HostList>,
    #[cfg(windows)]
    state: std::sync::Mutex<crate::draw::State>,
    /// The parts of the recording before `host`, in order, each closed: the
    /// list is cut at its waits for memory values ([`detach::cut`]).
    #[cfg(windows)]
    earlier: Vec<Part>,
    /// The waits recorded before the first work of `host`: the queue stands
    /// at them before it runs `host`.
    #[cfg(windows)]
    gate: Vec<ValueWait>,
    /// With `OATHKEEP_THIN=1`, of a list the title has: what it wrote down
    /// ([`thin`]; then there is no `host`).
    #[cfg(windows)]
    thin: thin::Front,
}

const _: () = assert!(offset_of!(List, put) == 0x18);
const _: () = assert!(offset_of!(List, pipeline_shadow) + 32 == 0x560);

/// Runs `f` with the host list and draw state of a title list, if there
/// is a host GPU.
#[cfg(windows)]
fn with_state(this: This, f: impl FnOnce(&crate::host::HostList, &mut crate::draw::State)) {
    let Some(list) = host_list(this) else { return };
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the title passes one of our lists.
    let state = unsafe { &(*this.cast::<List>()).state };
    f(list, &mut state.lock().unwrap_or_else(|e| e.into_inner()));
    #[cfg(feature = "diagnostics")]
    crate::copystat::recorded(started);
}

/// The submission of `lists` has run: each gives its descriptor tables'
/// slots back ([`crate::draw::State::tables_done`]). A list cut at waits
/// keeps them: its state goes with its last part.
#[cfg(windows)]
pub(crate) fn tables_done(lists: &[This]) {
    for &this in lists {
        if this.is_null() {
            continue;
        }
        // SAFETY: the title passes our lists.
        let list = unsafe { &*this.cast::<List>() };
        if list.earlier.is_empty() {
            list.state.lock().unwrap_or_else(|e| e.into_inner()).tables_done();
        }
    }
}

/// The host list behind a title list, if there is a host GPU.
#[cfg(windows)]
pub(crate) fn host_list<'a>(this: This) -> Option<&'a crate::host::HostList> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our lists.
    unsafe { (*this.cast::<List>()).host.as_ref() }
}

/// Lists start open, as in D3D12.
pub(crate) extern "system" fn create_list(
    _: This,
    _nodes: u32,
    kind: u32,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    new_list(kind, allocator, pipeline, iid, out)
}

/// The console's list creation: type, flags, node mask and barrier batch
/// size in a description.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct ListDesc {
    kind: u32,
    flags: u32,
    nodes: u32,
    barrier_batch: u32,
}

pub(crate) extern "system" fn create_list_x(
    _: This,
    desc: *const ListDesc,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "console command list");
    new_list(desc.kind, allocator, pipeline, iid, out)
}

/// `pipeline` is the list's initial pipeline (null: none), bound until the
/// title sets another, as after a reset.
fn new_list(
    kind: u32,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    tracing::debug!(kind, "command list");
    #[cfg(windows)]
    if thin::on() {
        let mut list = boxed(kind, pipeline, true);
        list.thin.begin(kind, pipeline);
        return object::hand_out(LIST, list, iid, &LIST_IIDS, out);
    }
    object::hand_out(LIST, boxed(kind, pipeline, false), iid, &LIST_IIDS, out)
}

/// A list; with `thin` one whose calls are written down (it has no host list).
fn boxed(kind: u32, pipeline: This, thin: bool) -> Box<List> {
    #[cfg(not(windows))]
    let _ = thin;
    let list = Box::new(List {
        object: match (crate::device::newer(), thin) {
            (true, false) => Object::new(&NEWER_LIST_FUNCTIONS),
            (false, false) => Object::new(&LIST_FUNCTIONS),
            #[cfg(windows)]
            (true, true) => Object::new(&NEWER_THIN_FUNCTIONS),
            #[cfg(windows)]
            (false, true) => Object::new(&THIN_FUNCTIONS),
            #[cfg(not(windows))]
            _ => Object::new(&LIST_FUNCTIONS),
        },
        kind,
        put: [0; 5],
        reserved: [0; 64],
        graphics_root_headers: [0; 128],
        compute_root_headers: [0; 128],
        pipeline_shadow: [0; 8],
        newer: [0; 256],
        uploads: Default::default(),
        #[cfg(windows)]
        host: crate::host::host().filter(|_| !thin).and_then(|h| match h.new_list() {
            Ok(list) => Some(list),
            Err(e) => {
                tracing::warn!(error = %e, "no host command list");
                None
            }
        }),
        #[cfg(windows)]
        state: {
            let mut state = crate::draw::State::default();
            state.set_pipeline(pipeline);
            std::sync::Mutex::new(state)
        },
        #[cfg(windows)]
        earlier: Vec::new(),
        #[cfg(windows)]
        gate: Vec::new(),
        #[cfg(windows)]
        thin: Default::default(),
    });
    #[cfg(not(windows))]
    let _ = pipeline;
    list
}

extern "system" fn list_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(LIST, iid, object::is(iid, &LIST_IIDS).then_some(this), out)
}
extern "system" fn list_release(this: This) -> u32 {
    object::release_boxed::<List>(this)
}
extern "system" fn list_type(this: This) -> u32 {
    // SAFETY: `this` is one of our lists.
    unsafe { (*this.cast::<List>()).kind }
}
extern "system" fn close(this: This) -> HRESULT {
    let _called = crate::timeline::span("the title's Close");
    #[cfg(windows)]
    if let Some(host) = host_list(this) {
        host.close();
    }
    S_OK
}
extern "system" fn reset(this: This, allocator: This, pipeline: This) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's list Reset");
    wait::clear_for(this);
    // Every submission of the list has run (submissions are waited for).
    // SAFETY: the title passes one of our lists.
    let uploads = unsafe { &(*this.cast::<List>()).uploads };
    uploads.lock().unwrap_or_else(|e| e.into_inner()).clear();
    // The parts of a recording that was cut and never submitted go with it
    // (a submission takes the parts it runs).
    #[cfg(windows)]
    {
        // SAFETY: the title passes one of our lists, and neither records
        // into it nor submits it during its reset.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        list.earlier.clear();
        list.gate.clear();
    }
    #[cfg(windows)]
    with_state(this, |host, state| {
        host.reset();
        state.reset(pipeline);
    });
    crate::pipeline::bound(pipeline);
    #[cfg(not(windows))]
    let _ = this;
    S_OK
}

/// Reset with console flags `(allocator, pipeline, flags)`; the flags only
/// tune the console's command memory.
extern "system" fn reset_x(this: This, allocator: This, pipeline: This, _flags: u32) -> HRESULT {
    reset(this, allocator, pipeline)
}

extern "system" fn set_pipeline(this: This, pipeline: This) {
    crate::pipeline::bound(pipeline);
    #[cfg(windows)]
    with_state(this, |_, state| state.set_pipeline(pipeline));
    #[cfg(not(windows))]
    let _ = this;
}

/// Draw state the host has no use for (root signatures: ours come with
/// the pipeline; depth order and shading rate hints): any arguments,
/// nothing returned.
extern "system" fn ignored_state(_: This) {}

/// The list methods by their slot in older titles' lists (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, list 1, graphics list 96
/// (59 as on PC, then console ones), then versions 1-6 (6 + 1 + 1 + 11 + 3
/// + 13): 140 slots.
const METHODS: [(usize, *const ()); 54] = [
    (0, list_query as *const ()),
    (2, list_release as *const ()),
    (7, object::parent_device as *const ()),      // 0x38
    (8, list_type as *const ()),                  // 0x40
    (9, close as *const ()),                      // 0x48
    (10, reset as *const ()),                     // 0x50
    (12, draw_state::draw as *const ()),               // 0x60 instanced
    (13, draw_state::draw_indexed as *const ()),       // 0x68 indexed
    (14, draw_state::dispatch as *const ()),           // 0x70
    (15, copy_buffer_region as *const ()),             // 0x78
    (16, copy_texture_region as *const ()),            // 0x80
    (17, copy_resource as *const ()),                  // 0x88
    (19, resolve as *const ()),                        // 0x98
    (20, draw_state::topology as *const ()),           // 0xa0
    (21, draw_state::viewports as *const ()),          // 0xa8
    (22, draw_state::scissors as *const ()),           // 0xb0
    (23, draw_state::blend_factor as *const ()),       // 0xb8
    (24, draw_state::stencil_ref as *const ()),        // 0xc0
    (25, set_pipeline as *const ()),                   // 0xc8
    (26, barrier as *const ()),                        // 0xd0
    (32, draw_state::heaps as *const ()),              // 0x100
    (33, ignored_state as *const ()),                  // 0x108 compute root signature
    (34, ignored_state as *const ()),                  // 0x110 graphics root signature
    (35, draw_state::compute_table as *const ()),      // 0x118
    (36, draw_state::graphics_table as *const ()),     // 0x120
    (37, draw_state::compute_constant as *const ()),   // 0x128
    (38, draw_state::graphics_constant as *const ()),  // 0x130
    (39, draw_state::compute_constants as *const ()),  // 0x138
    (40, draw_state::graphics_constants as *const ()), // 0x140
    (41, draw_state::compute_view as *const ()),       // 0x148 CBV
    (42, draw_state::graphics_view as *const ()),      // 0x150 CBV
    (43, draw_state::compute_view as *const ()),       // 0x158 SRV
    (44, draw_state::graphics_view as *const ()),      // 0x160 SRV
    (45, draw_state::compute_view as *const ()),       // 0x168 UAV
    (46, draw_state::graphics_view as *const ()),      // 0x170 UAV
    (47, draw_state::index_buffer as *const ()),       // 0x178
    (48, draw_state::vertex_buffers as *const ()),     // 0x180
    (50, draw_state::targets as *const ()),            // 0x190
    (51, clear_dsv as *const ()),                      // 0x198
    (52, clear_rtv as *const ()),                      // 0x1a0
    (53, clear_uav_uint as *const ()),                 // 0x1a8
    (54, clear_uav_float as *const ()),                // 0x1b0
    (59, execute_indirect as *const ()),               // 0x1d8 indirect
    (60, copy_memory as *const ()),                    // 0x1e0
    (68, draw_state::depth_bounds as *const ()),       // 0x220 depth bounds
    (69, write32 as *const ()),                        // 0x228
    (70, write64 as *const ()),                        // 0x230
    (71, wait_value as *const ()),                     // 0x238, 32 bits
    (72, wait_value64 as *const ()),                   // 0x240, 64 bits
    (73, ignored_state as *const ()),                  // 0x248 pixel shader depth order
    (83, copy_texture_region_x as *const ()),          // 0x298
    (84, copy_resource_x as *const ()),                // 0x2a0
    (124, ignored_state as *const ()),                 // 0x3e0 shading rate
    (127, draw_state::dispatch_mesh as *const ()),     // 0x3f8 mesh dispatch
];
/// The rest of [`METHODS`], kept apart only to stay readable.
const MORE_METHODS: [(usize, *const ()); 9] = [
    (28, query_mark as *const ()),                  // 0xe0 BeginQuery
    (29, query_mark as *const ()),                  // 0xe8 EndQuery
    (30, resolve_queries as *const ()),             // 0xf0
    (78, dispatch_x as *const ()),                  // 0x270
    (98, draw_state::dispatch_mesh_x as *const ()), // 0x310
    (99, driver_hint as *const ()),            // 0x318
    // Pipeline syncs at the top and bottom of the pipe, 0x408 and 0x410.
    (129, sync as *const ()),
    (130, sync as *const ()),
    (137, reset_x as *const ()), // 0x448
];

/// Newer titles' lists have two more methods in the console part of the
/// base list, after the clears (0x198/0x1a0 as before) and before
/// ExecuteIndirect, which moved from 0x1d8 to 0x1e8: every slot from 0x1d8
/// on is two later (taken here as starting exactly at 0x1d8). Inferred from
/// The title's call shapes: ExecuteIndirect 0x1e8, bottom-of-pipe
/// writes 0x238/0x240, waits 0x248/0x250, depth bounds 0x230, shading rate
/// 0x3f0/0x400, syncs 0x418/0x420, ResetX 0x458 (asserts on failure).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 59 { slot + 2 } else { slot }
}

/// Methods only newer titles' lists have. 0x490: the enhanced barrier
/// `(group count, groups)`, with 16-byte groups `(type, count, barriers)`
/// and 40-byte buffer barriers (title+0x95e0 builds them). 0x470 `(u64)`,
/// called from the title's state flush with its result unused.
/// shortcut: 0x470's purpose is unknown; ignored until a title shows it
/// matters.
const NEWER_METHODS: [(usize, *const ()); 3] = [
    (142, ignored_state as *const ()),
    (146, barrier_groups as *const ()),
    (148, allocate_upload as *const ()),
];

const fn list_table(newer: bool) -> [*const (); 160] {
    let mut f = object::d3d12_object(object::missing_table::<LIST, 160>());
    let mut i = 0;
    while i < METHODS.len() + MORE_METHODS.len() {
        let (slot, method) = if i < METHODS.len() {
            METHODS[i]
        } else {
            MORE_METHODS[i - METHODS.len()]
        };
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    let mut i = 0;
    while newer && i < NEWER_METHODS.len() {
        f[NEWER_METHODS[i].0] = NEWER_METHODS[i].1;
        i += 1;
    }
    f
}

/// Every slot up to the limit exists and logs, for the methods newer
/// titles add.
static LIST_FUNCTIONS: Table<160> = Table(list_table(false));
static NEWER_LIST_FUNCTIONS: Table<160> = Table(list_table(true));
/// Of lists whose calls are written down.
#[cfg(windows)]
static THIN_FUNCTIONS: Table<160> = Table(thin::table(false));
#[cfg(windows)]
static NEWER_THIN_FUNCTIONS: Table<160> = Table(thin::table(true));
