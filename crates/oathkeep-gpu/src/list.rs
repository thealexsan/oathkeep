#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::mem::offset_of;

use crate::object::{
    self, ALLOCATOR, E_INVALIDARG, HRESULT, Iid, LIST, Object, S_OK, Table, This, guid,
};
use crate::queue::Written;
use crate::resource::PAGEABLE_IIDS;

const ALLOCATOR_IIDS: [Iid; 4] = [
    guid(
        0x6102dee4,
        0xaf59,
        0x4b09,
        [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

const LIST_IIDS: [Iid; 10] = [
    guid(
        0x7116d91c,
        0xe7e4,
        0x47ce,
        [0xb8, 0xc6, 0xec, 0x81, 0x68, 0xf4, 0x37, 0xe5],
    ),
    guid(
        0x5b160d0f,
        0xac1b,
        0x4185,
        [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
    ),
    guid(
        0x553103fb,
        0x1fe7,
        0x4557,
        [0xbb, 0x38, 0x94, 0x6d, 0x7d, 0x0e, 0x7c, 0xa7],
    ),
    guid(
        0x38c3e585,
        0xff17,
        0x412c,
        [0x91, 0x50, 0x4f, 0xc6, 0xf9, 0xd7, 0x2a, 0x28],
    ),
    guid(
        0x6fda83a7,
        0xb84c,
        0x4e38,
        [0x9a, 0xc8, 0xc7, 0xbd, 0x22, 0x01, 0x6b, 0x3d],
    ),
    guid(
        0x8754318e,
        0xd3a9,
        0x4541,
        [0x98, 0xcf, 0x64, 0x5b, 0x50, 0xdc, 0x48, 0x74],
    ),
    guid(
        0x55050859,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    guid(
        0xc3827890,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Allocator {
    object: Object,
    kind: u32,
}

#[repr(C)]
struct List {
    object: Object,
    kind: u32,
    put: [u64; 5],
    reserved: [u32; 64],
    graphics_root_headers: [u32; 128],
    compute_root_headers: [u32; 128],
    pipeline_shadow: [u32; 8],
    newer: [u32; 256],
    uploads: std::sync::Mutex<Vec<Vec<u8>>>,
    #[cfg(windows)]
    host: Option<crate::host::HostList>,
    #[cfg(windows)]
    state: std::sync::Mutex<crate::draw::State>,
    #[cfg(windows)]
    earlier: Vec<Part>,
    #[cfg(windows)]
    gate: Vec<ValueWait>,
}

#[cfg(windows)]
struct Part {
    waits: Vec<ValueWait>,
    host: crate::host::HostList,
}
const _: () = assert!(offset_of!(List, put) == 0x18);
const _: () = assert!(offset_of!(List, pipeline_shadow) + 32 == 0x560);

#[cfg(windows)]
fn with_state(this: This, f: impl FnOnce(&crate::host::HostList, &mut crate::draw::State)) {
    let Some(list) = record_host(this) else {
        return;
    };
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the title passes one of our lists.
    let state = unsafe { &(*this.cast::<List>()).state };
    f(list, &mut state.lock().unwrap_or_else(|e| e.into_inner()));
    #[cfg(feature = "diagnostics")]
    crate::copystat::recorded(started);
}

#[cfg(windows)]
pub(crate) fn tables_done(lists: &[This]) {
    for &this in lists {
        if this.is_null() {
            continue;
        }
        // SAFETY: the title passes our lists.
        let list = unsafe { &*this.cast::<List>() };
        if list.earlier.is_empty() {
            list.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .tables_done();
        }
    }
}

#[cfg(windows)]
pub(crate) fn host_list<'a>(this: This) -> Option<&'a crate::host::HostList> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our lists.
    unsafe { (*this.cast::<List>()).host.as_ref() }
}

#[cfg(windows)]
fn record_host<'a>(this: This) -> Option<&'a crate::host::HostList> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our lists, recorded by one thread at a
    // time and not submitted while it records.
    let list = unsafe { &mut *this.cast_mut().cast::<List>() };
    if list.host.is_none() {
        list.host = fresh_list();
    }
    list.host.as_ref()
}

#[cfg(windows)]
pub(crate) struct Detached {
    host: crate::host::HostList,
    _uploads: Vec<Vec<u8>>,
    _state: crate::draw::State,
}

#[cfg(windows)]
impl Detached {
    pub(crate) fn host(&self) -> &crate::host::HostList {
        &self.host
    }
}

#[cfg(windows)]
struct Idle(crate::host::HostList, u64);

// SAFETY: D3D12 objects are free-threaded, and an idle list is touched by
// nobody until it is taken out again.
#[cfg(windows)]
unsafe impl Send for Idle {}

#[cfg(windows)]
static IDLE: std::sync::Mutex<(Vec<Idle>, u64)> = std::sync::Mutex::new((Vec::new(), 0));

#[cfg(windows)]
const IDLE_MOST: (usize, u64) = (32, 64 << 20);

#[cfg(windows)]
pub(crate) fn trim_idle() {
    trim_idle_to(8);
}

#[cfg(windows)]
pub(crate) fn trim_idle_to(keep: usize) {
    let mut dropped = Vec::new();
    {
        let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
        while idle.0.len() > keep {
            if let Some(entry) = idle.0.pop() {
                idle.1 = idle.1.saturating_sub(entry.1);
                dropped.push(entry);
            }
        }
    }
    drop(dropped);
}

#[cfg(windows)]
fn pools() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LIST_POOL").is_none_or(|v| v != "0"))
}

#[cfg(windows)]
fn fresh_list() -> Option<crate::host::HostList> {
    let idle = {
        let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
        let taken = idle.0.pop();
        if let Some(Idle(_, bytes)) = &taken {
            idle.1 -= bytes;
        }
        taken
    };
    match idle {
        Some(Idle(list, _)) => Some(list),
        None => crate::host::host().and_then(|h| match h.new_list() {
            Ok(list) => Some(list),
            Err(e) => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| tracing::warn!(error = %e, "no host command list"));
                None
            }
        }),
    }
}

#[cfg(windows)]
pub(crate) fn recycle(lists: Vec<Detached>, ran: &[usize]) {
    if !pools() {
        return;
    }
    for (
        i,
        Detached {
            host,
            _uploads,
            _state,
        },
    ) in lists.into_iter().enumerate()
    {
        drop(_state);
        drop(_uploads);
        if !ran.contains(&i)
            || IDLE.lock().unwrap_or_else(|e| e.into_inner()).0.len() >= IDLE_MOST.0
        {
            continue;
        }
        host.reset();
        let bytes = host.scratch_kept();
        let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
        if idle.0.len() < IDLE_MOST.0 && idle.1 + bytes <= IDLE_MOST.1 {
            idle.0.push(Idle(host, bytes));
            idle.1 += bytes;
        }
    }
}

#[cfg(windows)]
fn cut(this: This, wait: ValueWait) {
    if this.is_null() {
        return;
    }
    // SAFETY: the title passes one of our lists; a list is recorded by one
    // thread at a time and is not submitted while it records.
    let list = unsafe { &mut *this.cast_mut().cast::<List>() };
    let Some(current) = list.host.as_ref() else {
        list.gate.push(wait);
        return;
    };
    if current.untouched() {
        list.gate.push(wait);
        return;
    }
    let Some(fresh) = fresh_list() else {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                "no host list for the part after a wait: the wait moves to the part's start"
            )
        });
        list.gate.push(wait);
        return;
    };
    current.close();
    static CUTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if CUTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
        tracing::info!(
            parts = list.earlier.len() + 2,
            "a list is cut at a wait for a memory value"
        );
    }
    if let Some(done) = list.host.replace(fresh) {
        let waits = std::mem::replace(&mut list.gate, vec![wait]);
        list.earlier.push(Part { waits, host: done });
    }
    if let Some(host) = list.host.as_ref() {
        list.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replay(host);
    }
}

#[cfg(windows)]
pub(crate) fn cut_list(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    // SAFETY: one of our lists, closed and submitted: nothing records
    // into it.
    let list = unsafe { &*this.cast::<List>() };
    !list.earlier.is_empty() || !list.gate.is_empty()
}

#[cfg(windows)]
pub(crate) type Parts = Vec<(Vec<ValueWait>, Vec<Detached>, Vec<usize>)>;

#[cfg(windows)]
pub(crate) fn detach_parts(lists: &[This]) -> Parts {
    let mut parts: Parts = vec![(Vec::new(), Vec::new(), Vec::new())];
    let mut seen: Vec<(This, Option<(usize, usize)>)> = Vec::new();
    for &this in lists {
        if this.is_null() {
            continue;
        }
        if let Some(&(_, at)) = seen.iter().find(|(t, _)| *t == this) {
            match at {
                Some((entry, place)) if entry + 1 == parts.len() => parts[entry].2.push(place),
                _ => {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| {
                        tracing::warn!("a list named twice by a submission that waits for memory values runs once")
                    });
                }
            }
            continue;
        }
        // SAFETY: one of our lists, closed and submitted: nothing records
        // into it until the title resets it.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        let host = match list.host.take() {
            Some(host) => host,
            None => {
                if list.gate.is_empty() && list.earlier.is_empty() {
                    continue;
                }
                let Some(fresh) = fresh_list() else {
                    continue;
                };
                fresh.close();
                fresh
            }
        };
        let earlier = std::mem::take(&mut list.earlier);
        let gate = std::mem::take(&mut list.gate);
        let uploads = std::mem::take(&mut *list.uploads.lock().unwrap_or_else(|e| e.into_inner()));
        let state = std::mem::take(&mut *list.state.lock().unwrap_or_else(|e| e.into_inner()));
        let single = earlier.is_empty() && gate.is_empty();
        let mut pieces: Vec<(Vec<ValueWait>, Detached)> = earlier
            .into_iter()
            .map(|part| {
                (
                    part.waits,
                    Detached {
                        host: part.host,
                        _uploads: Vec::new(),
                        _state: Default::default(),
                    },
                )
            })
            .collect();
        pieces.push((
            gate,
            Detached {
                host,
                _uploads: uploads,
                _state: state,
            },
        ));
        let mut last = None;
        for (waits, detached) in pieces {
            if !waits.is_empty() {
                parts.push((waits, Vec::new(), Vec::new()));
            }
            let entry = parts.len() - 1;
            let (_, taken, order) = &mut parts[entry];
            let place = taken.len();
            order.push(place);
            taken.push(detached);
            last = Some((entry, place));
        }
        seen.push((this, if single { last } else { None }));
    }
    parts.retain(|(_, taken, _)| !taken.is_empty());
    parts
}

#[cfg(windows)]
pub(crate) fn detach(lists: &[This]) -> (Vec<Detached>, Vec<usize>) {
    let mut taken: Vec<(This, Detached)> = Vec::new();
    let mut order = Vec::new();
    for &this in lists {
        if this.is_null() {
            continue;
        }
        if let Some(i) = taken.iter().position(|(t, _)| *t == this) {
            order.push(i);
            continue;
        }
        let list = this.cast_mut().cast::<List>();
        // SAFETY: one of our lists, closed and submitted: nothing records
        // into it until the title resets it.
        let (host, uploads, state) = unsafe {
            let Some(host) = (*list).host.take() else {
                continue;
            };
            let uploads =
                std::mem::take(&mut *(*list).uploads.lock().unwrap_or_else(|e| e.into_inner()));
            let state =
                std::mem::take(&mut *(*list).state.lock().unwrap_or_else(|e| e.into_inner()));
            (host, uploads, state)
        };
        order.push(taken.len());
        taken.push((
            this,
            Detached {
                host,
                _uploads: uploads,
                _state: state,
            },
        ));
    }
    (taken.into_iter().map(|(_, d)| d).collect(), order)
}

pub(crate) extern "system" fn create_allocator(
    _: This,
    kind: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    let allocator = Box::new(Allocator {
        object: Object::new(&ALLOCATOR_FUNCTIONS),
        kind,
    });
    object::hand_out(ALLOCATOR, allocator, iid, &ALLOCATOR_IIDS, out)
}

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
    let list = Box::new(List {
        object: if crate::device::newer() {
            Object::new(&NEWER_LIST_FUNCTIONS)
        } else {
            Object::new(&LIST_FUNCTIONS)
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
        host: None,
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
    });
    #[cfg(not(windows))]
    let _ = pipeline;
    object::hand_out(LIST, list, iid, &LIST_IIDS, out)
}

extern "system" fn allocator_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &ALLOCATOR_IIDS).then_some(this);
    object::answer(ALLOCATOR, iid, found, out)
}
extern "system" fn allocator_release(this: This) -> u32 {
    object::release_boxed::<Allocator>(this)
}
extern "system" fn allocator_reset(_: This) -> HRESULT {
    S_OK
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
    VALUE_WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&(list, _)| list != this as usize);
    // SAFETY: the title passes one of our lists.
    let uploads = unsafe { &(*this.cast::<List>()).uploads };
    uploads.lock().unwrap_or_else(|e| e.into_inner()).clear();
    #[cfg(windows)]
    {
        // SAFETY: the title passes one of our lists, and neither records
        // into it nor submits it during its reset.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        list.earlier.clear();
        list.gate.clear();
        if let Some(host) = list.host.as_ref() {
            host.reset();
        }
        list.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset(pipeline);
    }
    #[cfg(not(windows))]
    let _ = (this, pipeline);
    S_OK
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Barrier {
    kind: u32,
    flags: u32,
    first: This,
    second: u64,
    last: u64,
}

#[cfg(windows)]
fn host_barrier(list: &crate::host::HostList, b: &Barrier) -> Option<Barrier> {
    use windows::core::Interface;
    let host = |r: This| crate::resource::host_resource(r).map(|h| h.as_raw().cast_const().cast());
    match b.kind {
        0 => {
            const BEGIN_ONLY: u32 = 1;
            if b.flags & BEGIN_ONLY != 0 {
                return None;
            }
            let wanted = crate::host::texture_state(b.last as u32);
            let (texture, before) = list.transition(b.first, wanted)?;
            Some(Barrier {
                kind: 0,
                flags: 0,
                first: texture.as_raw().cast_const().cast(),
                second: 0xffff_ffff | (u64::from(before) << 32),
                last: u64::from(crate::host::legal_state(texture, wanted)),
            })
        }
        1 => Some(Barrier {
            first: if b.first.is_null() {
                b.first
            } else {
                host(b.first)?
            },
            second: if b.second == 0 {
                0
            } else {
                host(b.second as This)? as u64
            },
            ..*b
        }),
        2 => Some(Barrier {
            first: if b.first.is_null() {
                b.first
            } else {
                match crate::resource::twin_resource(b.first) {
                    Some(twin) => twin.as_raw().cast_const().cast(),
                    None => host(b.first)?,
                }
            },
            ..*b
        }),
        _ => None,
    }
}

const UAV_BARRIER: Barrier = Barrier {
    kind: 2,
    flags: 0,
    first: std::ptr::null(),
    second: 0,
    last: 0,
};

extern "system" fn barrier(this: This, count: u32, barriers: *const Barrier) -> u32 {
    #[cfg(windows)]
    if count > 0 && !barriers.is_null() {
        // SAFETY: the title passes `count` barriers.
        let barriers = unsafe { std::slice::from_raw_parts(barriers, count as usize) };
        const BEGIN_ONLY: u32 = 1;
        if barriers
            .iter()
            .all(|b| b.kind == 0 && b.flags & BEGIN_ONLY != 0)
        {
            return 0;
        }
        let Some(list) = record_host(this) else {
            return 0;
        };
        let mut host: Vec<Barrier> = barriers
            .iter()
            .filter_map(|b| host_barrier(list, b))
            .collect();
        if barriers.iter().any(|b| {
            b.kind != 1 && !b.first.is_null() && crate::resource::buffer_bytes(b.first).is_some()
        }) {
            host.push(UAV_BARRIER);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::WaitsForBuffers, 1);
        }
        if !host.is_empty() {
            let cleans_uav = host.iter().any(|b| b.kind == 2 && b.first.is_null());
            use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
            const _: () = assert!(size_of::<Barrier>() == size_of::<D3D12_RESOURCE_BARRIER>());
            // SAFETY: the same layout; borrowed resource pointers, never
            // released through this slice.
            let host = unsafe {
                std::slice::from_raw_parts(
                    host.as_ptr().cast::<D3D12_RESOURCE_BARRIER>(),
                    host.len(),
                )
            };
            // SAFETY: an open list and live resources.
            unsafe { list.list.ResourceBarrier(host) };
            if cleans_uav {
                list.mark_uav_clean();
            }
        }
    }
    0
}

#[cfg(windows)]
pub(crate) fn ensure_state(list: &crate::host::HostList, resource: This, state: u32) {
    use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
    if let Some((texture, before)) = list.transition(resource, state) {
        let state = crate::host::legal_state(texture, state);
        let barrier = crate::host::transition(texture, State(before as i32), State(state as i32));
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
    }
}

#[cfg(windows)]
const COPY_DEST: u32 = 0x400;
#[cfg(windows)]
const COPY_SOURCE: u32 = 0x800;

#[cfg(windows)]
fn buffer_range(buffer: This, offset: u64, len: u64) -> Option<*mut u8> {
    let (data, size) = crate::resource::buffer_bytes(buffer)?;
    let end = offset.checked_add(len)?;
    if end > size {
        tracing::warn!(offset, len, size, "copy outside its buffer");
        return None;
    }
    // SAFETY: inside the buffer, checked above.
    Some(unsafe { data.add(offset as usize) })
}

extern "system" fn copy_buffer_region(
    this: This,
    dst: This,
    dst_offset: u64,
    src: This,
    src_offset: u64,
    len: u64,
) {
    #[cfg(windows)]
    if let Some(list) = record_host(this)
        && let Some(to) = buffer_range(dst, dst_offset, len)
        && let Some(from) = buffer_range(src, src_offset, len)
    {
        if len == 0 {
            return;
        }
        static DIRECT_BUF_COPY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let direct = *DIRECT_BUF_COPY
            .get_or_init(|| std::env::var_os("OATHKEEP_DIRECT_BUF_COPY").is_some_and(|v| v == "1"));
        if direct {
            let (from_u64, to_u64) = (from as u64, to as u64);
            let (from_u, to_u, len_u) = (from as usize, to as usize, len as usize);
            let src_existing = crate::shadow::find_existing(from_u64, len);
            let dst_existing = crate::shadow::find_existing(to_u64, len);
            let gpu_active = src_existing.as_ref().is_some_and(|s| {
                s.ever_written()
                    && (s.pending_touches(from_u, len_u)
                        || list.touches_gpu_write(from_u, len_u)
                        || crate::host::written_in_flight(from_u, len_u))
            }) || dst_existing.as_ref().is_some_and(|s| {
                s.ever_written()
                    && (s.pending_touches(to_u, len_u)
                        || list.touches_gpu_write(to_u, len_u)
                        || crate::host::written_in_flight(to_u, len_u))
            });
            if gpu_active
                && let Some(src_shadow) = src_existing.or_else(|| crate::shadow::find(from_u64, len))
                && let Some(dst_shadow) = dst_existing.or_else(|| crate::shadow::find(to_u64, len))
                && src_shadow.left(from_u64) >= len
                && dst_shadow.left(to_u64) >= len
            {
                let (src_off, dst_off) = (src_shadow.offset(from_u64), dst_shadow.offset(to_u64));
                let same_overlap = std::sync::Arc::ptr_eq(&src_shadow, &dst_shadow)
                    && src_off < dst_off.saturating_add(len)
                    && dst_off < src_off.saturating_add(len);
                if !same_overlap {
                    src_shadow.refresh(list, from_u64, len);
                    dst_shadow.refresh(list, to_u64, len);
                    list.keep(&src_shadow.buffer);
                    list.keep(&dst_shadow.buffer);
                    // SAFETY: an open list with live shadow buffers; UAV barriers
                    // bracket CopyBufferRegion so prior shader writes are visible
                    // and destination bytes are visible to subsequent shaders.
                    unsafe {
                        use windows::Win32::Graphics::Direct3D12::{
                            D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0,
                            D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_UAV,
                            D3D12_RESOURCE_UAV_BARRIER,
                        };
                        let uav = || D3D12_RESOURCE_BARRIER {
                            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                    pResource: std::mem::ManuallyDrop::new(None),
                                }),
                            },
                        };
                        list.list.ResourceBarrier(&[uav()]);
                        list.list.CopyBufferRegion(
                            &dst_shadow.buffer,
                            dst_off,
                            &src_shadow.buffer,
                            src_off,
                            len,
                        );
                        list.list.ResourceBarrier(&[uav()]);
                    }
                    dst_shadow.gpu_writes(list, to_u64, len);
                    return;
                }
            }
        }
        list.defer_copy(to, from, len as usize);
    }
    #[cfg(not(windows))]
    let _ = (this, dst, dst_offset, src, src_offset, len);
}

#[cfg(windows)]
fn unlike(dst: This, src: This) -> bool {
    let unlike = crate::resource::stands_in(dst) != crate::resource::stands_in(src);
    if unlike {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                ?dst,
                ?src,
                "copy between a stand-in target and title-format texels skipped"
            )
        });
    }
    unlike
}

extern "system" fn copy_resource(this: This, dst: This, src: This) {
    #[cfg(windows)]
    {
        let Some(list) = record_host(this) else {
            return;
        };
        if let (Some(to), Some(from)) = (
            crate::resource::host_resource(dst),
            crate::resource::host_resource(src),
        ) {
            if unlike(dst, src) {
                return;
            }
            if crate::texture::title_written(from) {
                crate::texture::upload(list, src, from);
            }
            list.twin_use(dst, false, true);
            list.twin_use(src, false, false);
            crate::texture::card_writes(list, dst, to, None);
            ensure_state(list, dst, COPY_DEST);
            ensure_state(list, src, COPY_SOURCE);
            crate::texture::host_written(dst);
            #[cfg(feature = "diagnostics")]
            list.note_op(|| format!("copy resource {src:?} to {dst:?}"));
            // SAFETY: an open list and live textures.
            unsafe { list.list.CopyResource(to, from) };
        } else if let (Some((to, a)), Some((from, b))) = (
            crate::resource::buffer_bytes(dst),
            crate::resource::buffer_bytes(src),
        ) {
            list.defer_copy(to, from, a.min(b) as usize);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, dst, src);
}

extern "system" fn copy_resource_x(this: This, dst: This, src: This, _flags: u32) {
    copy_resource(this, dst, src);
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CopyLocation {
    resource: This,
    kind: u32,
    footprint: crate::resource::PlacedFootprint,
}

extern "system" fn copy_texture_region(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::{D3D12_BOX, D3D12_TEXTURE_COPY_LOCATION};
        use windows::core::Interface;
        const FOOTPRINT: u32 = 1;
        // SAFETY: the title passes both locations (null is tolerated).
        let (Some(list), Some(&dst), Some(&src)) =
            (record_host(this), unsafe { dst.as_ref() }, unsafe {
                src.as_ref()
            })
        else {
            return;
        };
        let Some(to) = crate::resource::host_resource(dst.resource) else {
            static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let made = read_back(list, dst, x, y, z, src, source_box);
            if n <= 16 || n.is_multiple_of(4096) || made.is_err() {
                static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if made.is_err() && REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 16
                {
                    return;
                }
                let source = crate::resource::host_resource(src.resource).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!(
                        "{}x{} f{} mips {}",
                        d.Width, d.Height, d.Format.0, d.MipLevels
                    )
                });
                tracing::info!(
                    copies = n,
                    made = ?made,
                    source = ?source,
                    source_kind = src.kind,
                    destination_kind = dst.kind,
                    destination_offset = dst.footprint.offset,
                    destination_bytes = ?dst.footprint.bytes(),
                    "a texture to buffer copy (a read back)"
                );
            }
            return;
        };
        if unlike(dst.resource, src.resource) {
            return;
        }
        let mut from = src;
        let _upload;
        #[cfg(feature = "diagnostics")]
        let mut read = None;
        if src.kind == FOOTPRINT {
            let (Some(host), Some(bytes)) = (crate::host::host(), src.footprint.bytes()) else {
                return;
            };
            let Some(data) = buffer_range(src.resource, src.footprint.offset, bytes) else {
                return;
            };
            static DIRECT_TEX_COPY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let direct = *DIRECT_TEX_COPY.get_or_init(|| {
                std::env::var_os("OATHKEEP_DIRECT_TEX_COPY").is_some_and(|v| v == "1")
            });
            if direct
                && let Some(shadow) = crate::shadow::find(data as u64, bytes)
                && let off = shadow.offset(data as u64)
                && off % 512 == 0
                && from.footprint.row_pitch % 256 == 0
                && shadow.left(data as u64) >= bytes
            {
                shadow.refresh(list, data as u64, bytes);
                list.keep(&shadow.buffer);
                // SAFETY: an open list; ensures any prior shader UAV write in this
                // list is visible before CopyTextureRegion reads `shadow.buffer`.
                unsafe {
                    use windows::Win32::Graphics::Direct3D12::{
                        D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0,
                        D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        D3D12_RESOURCE_UAV_BARRIER,
                    };
                    list.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                    }]);
                }
                from.resource = shadow.buffer.as_raw().cast_const().cast();
                from.footprint.offset = off;
                #[cfg(feature = "diagnostics")]
                {
                    read = Some((shadow.gpu(data as u64), bytes));
                }
                _upload = shadow.buffer.clone();
            } else {
                let Some((buffer, staging)) = list.upload(host, bytes) else {
                    return;
                };
                list.defer_fill(staging, data, bytes as usize);
                from.resource = buffer.as_raw().cast_const().cast();
                from.footprint.offset = 0;
                #[cfg(feature = "diagnostics")]
                {
                    // SAFETY: a live buffer.
                    read = Some((unsafe { buffer.GetGPUVirtualAddress() }, bytes));
                }
                _upload = buffer;
            }
        } else {
            let Some(texture) = crate::resource::host_resource(src.resource) else {
                return;
            };
            list.twin_use(src.resource, false, false);
            if crate::texture::title_written(texture) {
                crate::texture::upload(list, src.resource, texture);
            }
            ensure_state(list, src.resource, COPY_SOURCE);
            from.resource = texture.as_raw().cast_const().cast();
        }
        list.twin_use(dst.resource, false, true);
        crate::texture::card_writes(list, dst.resource, to, Some(dst.footprint.offset as u32));
        ensure_state(list, dst.resource, COPY_DEST);
        crate::texture::host_written(dst.resource);
        let mut to_location = dst;
        to_location.resource = to.as_raw().cast_const().cast();
        let host_box = (!source_box.is_null()).then(|| source_box.cast::<D3D12_BOX>());
        #[cfg(feature = "diagnostics")]
        {
            let text = || {
                // SAFETY: a live texture; the box is the title's or null.
                let (d, b) = unsafe { (to.GetDesc(), host_box.map(|b| *b)) };
                format!(
                    "copy to {:?} {}x{} f{} sub/kind {} at {x},{y},{z} from {:?} kind {} footprint {:?} box {b:?}",
                    dst.resource,
                    d.Width,
                    d.Height,
                    d.Format.0,
                    dst.kind,
                    src.resource,
                    src.kind,
                    (src.kind == FOOTPRINT).then_some(src.footprint)
                )
            };
            match read {
                Some(range) => list.note_read(text, range),
                None => list.note_op(text),
            }
        }
        const _: () =
            assert!(size_of::<CopyLocation>() == size_of::<D3D12_TEXTURE_COPY_LOCATION>());
        // SAFETY: an open list; both locations in the host's layout, naming
        // live resources; the box is the title's (same layout).
        unsafe {
            list.list.CopyTextureRegion(
                (&raw const to_location).cast(),
                x,
                y,
                z,
                (&raw const from).cast(),
                host_box,
            )
        };
    }
    #[cfg(not(windows))]
    let _ = (this, dst, x, y, z, src, source_box);
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn read_back(
    list: &crate::host::HostList,
    dst: CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: CopyLocation,
    source_box: *const [u32; 6],
) -> Result<(), &'static str> {
    use windows::Win32::Graphics::Direct3D12::D3D12_BOX;
    use windows::core::Interface;
    const FOOTPRINT: u32 = 1;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_READ_BACK").is_none_or(|v| v != "0")) {
        return Err("switched off");
    }
    if dst.kind != FOOTPRINT || src.kind == FOOTPRINT {
        return Err("not a texture subresource into a placed footprint");
    }
    if (x, y, z) != (0, 0, 0) {
        return Err("a destination corner other than the footprint's");
    }
    let host = crate::host::host().ok_or("no host")?;
    let (footprint, [size, row, rows, pitch]) =
        dst.footprint.host_rows().ok_or("an unknown format")?;
    let bytes = pitch * (rows - 1) + row;
    let data = buffer_range(dst.resource, dst.footprint.offset, bytes)
        .ok_or("a destination outside its buffer")?;
    let texture = crate::resource::host_resource(src.resource).ok_or("no host texture")?;
    let buffer = host.read_back_buffer(size).ok_or("no read-back buffer")?;
    list.twin_use(src.resource, false, false);
    if crate::texture::title_written(texture) {
        crate::texture::upload(list, src.resource, texture);
    }
    ensure_state(list, src.resource, COPY_SOURCE);
    let mut from = src;
    from.resource = texture.as_raw().cast_const().cast();
    let mut to = dst;
    to.resource = buffer.as_raw().cast_const().cast();
    to.footprint = footprint;
    let host_box = (!source_box.is_null()).then(|| source_box.cast::<D3D12_BOX>());
    #[cfg(feature = "diagnostics")]
    list.note_op(|| {
        format!(
            "read back of {:?} sub {} into buffer {:?}+{:#x}: {rows} rows of {row} bytes",
            src.resource, src.footprint.offset as u32, dst.resource, dst.footprint.offset
        )
    });
    // SAFETY: an open list; both locations in the host's layout (asserted
    // in `copy_texture_region`), naming a live texture and our buffer, which
    // the list keeps until it is reset; the box is the title's.
    unsafe {
        list.list.CopyTextureRegion(
            (&raw const to).cast(),
            0,
            0,
            0,
            (&raw const from).cast(),
            host_box,
        )
    };
    list.defer_read_back(buffer, data, [row as usize, rows as usize, pitch as usize]);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
extern "system" fn copy_texture_region_x(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
    _flags: u32,
) {
    copy_texture_region(this, dst, x, y, z, src, source_box);
}

extern "system" fn resolve(
    this: This,
    dst: This,
    dst_sub: u32,
    src: This,
    src_sub: u32,
    format: u32,
) {
    #[cfg(windows)]
    if let Some(list) = record_host(this)
        && let (Some(to), Some(from)) = (
            crate::resource::host_resource(dst),
            crate::resource::host_resource(src),
        )
    {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_RESOLVE_MODE_AVERAGE, ID3D12GraphicsCommandList1,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
        use windows::core::Interface;
        const RESOLVE_DEST: u32 = 0x1000;
        const RESOLVE_SOURCE: u32 = 0x2000;
        if unlike(dst, src) {
            return;
        }
        list.twin_use(dst, false, true);
        list.twin_use(src, false, false);
        crate::texture::card_writes(list, dst, to, Some(dst_sub));
        ensure_state(list, dst, RESOLVE_DEST);
        ensure_state(list, src, RESOLVE_SOURCE);
        crate::texture::host_written(dst);
        #[cfg(feature = "diagnostics")]
        list.note_op(|| format!("resolve {src:?} to {dst:?} format {format}"));
        // SAFETY: an open list and live textures; the region is inside both.
        unsafe {
            let (a, b) = (to.GetDesc(), from.GetDesc());
            let format = if crate::resource::stands_in(dst) {
                a.Format
            } else {
                DXGI_FORMAT(format as i32)
            };
            if (a.Width, a.Height) == (b.Width, b.Height) {
                list.list
                    .ResolveSubresource(to, dst_sub, from, src_sub, format);
            } else if let Ok(list1) = list.list.cast::<ID3D12GraphicsCommandList1>() {
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: a.Width.min(b.Width) as i32,
                    bottom: a.Height.min(b.Height) as i32,
                };
                list1.ResolveSubresourceRegion(
                    to,
                    dst_sub,
                    0,
                    0,
                    from,
                    src_sub,
                    Some(&rect),
                    format,
                    D3D12_RESOLVE_MODE_AVERAGE,
                );
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (this, dst, dst_sub, src, src_sub, format);
}

#[repr(C)]
pub(crate) struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

extern "system" fn clear_rtv(
    this: This,
    handle: u64,
    color: *const [f32; 4],
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        let (Some(list), Some(record)) = (record_host(this), crate::descriptor::rtv_record(handle))
        else {
            return;
        };
        let (Some(texture), Some(host)) = (
            crate::resource::host_resource(record.resource),
            crate::host::host(),
        ) else {
            return;
        };
        // SAFETY: the title passes four floats (null is tolerated).
        let Some(color) = (unsafe { color.as_ref() }) else {
            return;
        };
        let rects = (count > 0 && !rects.is_null()).then(|| {
            // SAFETY: the title passes `count` rectangles, laid out as the host's.
            unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
        });
        const RENDER_TARGET: u32 = 0x4;
        let rtv = host.rtv(Some(texture), record.desc().as_ref());
        // SAFETY: an open list, a fresh view of a live texture.
        unsafe {
            if let Some((_, before)) = list.transition(record.resource, RENDER_TARGET) {
                use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
                list.list.ResourceBarrier(&[crate::host::transition(
                    texture,
                    State(before as i32),
                    State(RENDER_TARGET as i32),
                )]);
            }
            list.list.ClearRenderTargetView(rtv, color, rects);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, handle, color, count, rects);
}

extern "system" fn clear_dsv(
    this: This,
    handle: u64,
    flags: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::D3D12_CLEAR_FLAGS;
        let (Some(list), Some(record)) = (record_host(this), crate::descriptor::dsv_record(handle))
        else {
            return;
        };
        let (Some(texture), Some(host)) = (
            crate::resource::host_resource(record.resource),
            crate::host::host(),
        ) else {
            return;
        };
        let rects = (count > 0 && !rects.is_null()).then(|| {
            // SAFETY: the title passes `count` rectangles, laid out as the host's.
            unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
        });
        const DEPTH_WRITE: u32 = 0x10;
        list.twin_use(record.resource, false, true);
        ensure_state(list, record.resource, DEPTH_WRITE);
        if crate::resource::depth_and_unordered(record.resource) {
            crate::resource::DEPTH_UNORDERED[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let dsv = host.dsv(texture, record.depth_desc().as_ref());
        // SAFETY: an open list, a fresh view of a live texture.
        unsafe {
            list.list.ClearDepthStencilView(
                dsv,
                D3D12_CLEAR_FLAGS(flags as i32 & 3),
                depth,
                stencil,
                rects,
            )
        };
    }
    #[cfg(not(windows))]
    let _ = (this, handle, flags, depth, stencil, count, rects);
}

extern "system" fn copy_memory(this: This, dst: u64, src: u64, len: u64, _flags: u32) {
    #[cfg(windows)]
    if let Some(list) = record_host(this)
        && dst != 0
        && src != 0
    {
        let len = len as usize;
        list.defer(move || {
            crate::watch::writing(dst as usize, len);
            if crate::texture::committed(src as *const u8, len)
                && crate::texture::committed(dst as *const u8, len)
            {
                // SAFETY: both ranges are committed title memory.
                unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
            }
        });
    }
    #[cfg(not(windows))]
    let _ = (this, dst, src, len);
}

extern "system" fn driver_hint(_: This, _feature: u64, _value: u64) -> u64 {
    0
}

extern "system" fn dispatch_x(this: This, x: u32, y: u32, z: u32, _flags: u32) {
    #[cfg(windows)]
    state::dispatch(this, x, y, z);
    #[cfg(not(windows))]
    {
        let _ = (this, x, y, z);
        count_dispatch();
    }
}

extern "system" fn query_mark(_: This, _heap: This, _kind: u32, _index: u32) {}

extern "system" fn resolve_queries(
    this: This,
    _heap: This,
    kind: u32,
    _first: u32,
    count: u32,
    buffer: This,
    offset: u64,
) {
    const OCCLUSION: u32 = 0;
    const BINARY: u32 = 1;
    const TIMESTAMP: u32 = 2;
    const PIPELINE_STATISTICS: u32 = 3;
    let Some((data, size)) = crate::resource::buffer_bytes(buffer) else {
        return;
    };
    let (what, bytes) = match kind {
        OCCLUSION => (Written::Value(1 << 20), 8),
        BINARY => (Written::Value(1), 8),
        TIMESTAMP => (Written::GpuTimestamp, 8),
        PIPELINE_STATISTICS => (Written::Value(0), 88),
        _ => (Written::Value(0), 16),
    };
    if offset + u64::from(count) * bytes > size {
        return;
    }
    for i in 0..u64::from(count) {
        for piece in 0..bytes / 8 {
            // SAFETY: inside the buffer, checked above.
            let at = unsafe { data.add((offset + i * bytes + piece * 8) as usize) };
            let what = if piece == 0 { what } else { Written::Value(0) };
            defer_write(this, at as u64, what, 8);
        }
    }
}

static ALLOCATOR_FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<ALLOCATOR, 24>());
    f[0] = allocator_query as *const ();
    f[2] = allocator_release as *const ();
    f[7] = object::parent_device as *const ();
    f[8] = allocator_reset as *const ();
    f[9] = object::ignore as *const ();
    Table(f)
};

extern "system" fn set_pipeline(this: This, pipeline: This) {
    crate::pipeline::bound(pipeline);
    #[cfg(windows)]
    with_state(this, |_, state| state.set_pipeline(pipeline));
    #[cfg(not(windows))]
    let _ = this;
}

extern "system" fn sync(_: This, _flags: u32, _control: u32, _address: u64, _size: u64) {}

extern "system" fn reset_x(this: This, allocator: This, pipeline: This, _flags: u32) -> HRESULT {
    reset(this, allocator, pipeline)
}

extern "system" fn allocate_upload(this: This, size: u64, alignment: u64) -> *mut u8 {
    let align = usize::try_from(alignment).unwrap_or(0).max(16);
    let Some(len) = usize::try_from(size)
        .ok()
        .and_then(|s| s.checked_add(align))
    else {
        return std::ptr::null_mut();
    };
    if this.is_null() || !align.is_power_of_two() {
        return std::ptr::null_mut();
    }
    let mut block = vec![0u8; len];
    let start = block.as_mut_ptr();
    // SAFETY: the offset is below `align`, so `size` bytes fit after it.
    let at = unsafe { start.add(start.align_offset(align)) };
    // SAFETY: the title passes one of our lists.
    let uploads = unsafe { &(*this.cast::<List>()).uploads };
    uploads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(block);
    at
}

extern "system" fn ignored_state(_: This) {}

#[cfg(windows)]
mod state {
    use super::{This, with_state};
    use crate::draw::{IndexView, VertexView};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

    fn items<'a, T>(items: *const T, count: u32) -> &'a [T] {
        if items.is_null() || count == 0 {
            return &[];
        }
        // SAFETY: the title passes `count` items.
        unsafe { std::slice::from_raw_parts(items, count as usize) }
    }

    pub(super) extern "system" fn topology(this: This, topology: u32) {
        with_state(this, |list, s| s.set_topology(list, topology));
    }
    pub(super) extern "system" fn viewports(
        this: This,
        count: u32,
        viewports: *const D3D12_VIEWPORT,
    ) {
        with_state(this, |list, s| {
            s.set_viewports(list, items(viewports, count))
        });
    }
    pub(super) extern "system" fn scissors(this: This, count: u32, rects: *const RECT) {
        with_state(this, |list, s| s.set_scissors(list, items(rects, count)));
    }
    pub(super) extern "system" fn blend_factor(this: This, factor: *const [f32; 4]) {
        with_state(this, |list, s| {
            // SAFETY: the title passes four floats or null.
            s.set_blend_factor(list, unsafe { factor.as_ref() })
        });
    }
    pub(super) extern "system" fn stencil_ref(this: This, reference: u32) {
        with_state(this, |list, s| s.set_stencil_ref(list, reference));
    }
    pub(super) extern "system" fn depth_bounds(this: This, min: f32, max: f32) {
        with_state(this, |list, s| s.set_depth_bounds(list, min, max));
    }
    pub(super) extern "system" fn heaps(this: This, count: u32, heaps: *const This) {
        with_state(this, |_, s| s.set_heaps(items(heaps, count)));
    }
    pub(super) extern "system" fn compute_table(this: This, slot: u32, handle: u64) {
        with_state(this, |_, s| s.set_table(true, slot, handle));
    }
    pub(super) extern "system" fn graphics_table(this: This, slot: u32, handle: u64) {
        with_state(this, |_, s| s.set_table(false, slot, handle));
    }
    pub(super) extern "system" fn compute_constant(this: This, slot: u32, value: u32, offset: u32) {
        with_state(this, |_, s| s.set_constants(true, slot, &[value], offset));
    }
    pub(super) extern "system" fn graphics_constant(
        this: This,
        slot: u32,
        value: u32,
        offset: u32,
    ) {
        with_state(this, |_, s| s.set_constants(false, slot, &[value], offset));
    }
    pub(super) extern "system" fn compute_constants(
        this: This,
        slot: u32,
        count: u32,
        values: *const u32,
        offset: u32,
    ) {
        with_state(this, |_, s| {
            s.set_constants(true, slot, items(values, count), offset)
        });
    }
    pub(super) extern "system" fn graphics_constants(
        this: This,
        slot: u32,
        count: u32,
        values: *const u32,
        offset: u32,
    ) {
        with_state(this, |_, s| {
            s.set_constants(false, slot, items(values, count), offset)
        });
    }
    pub(super) extern "system" fn compute_view(this: This, slot: u32, address: u64) {
        with_state(this, |_, s| s.set_view(true, slot, address));
    }
    pub(super) extern "system" fn graphics_view(this: This, slot: u32, address: u64) {
        with_state(this, |_, s| s.set_view(false, slot, address));
    }
    pub(super) extern "system" fn index_buffer(this: This, view: *const IndexView) {
        // SAFETY: the title passes a view or null.
        with_state(this, |_, s| s.set_index_buffer(unsafe { view.as_ref() }));
    }
    pub(super) extern "system" fn vertex_buffers(
        this: This,
        start: u32,
        count: u32,
        views: *const VertexView,
    ) {
        with_state(this, |_, s| {
            if views.is_null() {
                s.clear_vertex_buffers(start, count);
            } else {
                s.set_vertex_buffers(start, items(views, count));
            }
        });
    }
    pub(super) extern "system" fn targets(
        this: This,
        count: u32,
        handles: *const u64,
        one_range: i32,
        depth: *const u64,
    ) {
        let handles: Vec<u64> = if one_range != 0 && !handles.is_null() {
            // SAFETY: the title passes the first handle.
            let first = unsafe { *handles };
            (0..u64::from(count)).map(|i| first + 128 * i).collect()
        } else {
            items(handles, count).to_vec()
        };
        // SAFETY: the title passes a handle or null.
        let depth = unsafe { depth.as_ref() }.copied().unwrap_or(0);
        with_state(this, |list, s| s.set_targets(list, &handles, depth));
    }
    pub(super) extern "system" fn draw(
        this: This,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        super::count_draw();
        with_state(this, |list, s| {
            s.draw(list, vertices, instances, first, first_instance)
        });
    }
    pub(super) extern "system" fn dispatch(this: This, x: u32, y: u32, z: u32) {
        super::count_dispatch();
        with_state(this, |list, s| s.dispatch(list, x, y, z));
    }
    pub(super) extern "system" fn dispatch_mesh(this: This, x: u32, y: u32, z: u32) {
        super::count_draw();
        if y * z != 1 {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(x, y, z, "a mesh dispatch with y or z groups"));
        }
        with_state(this, |list, s| s.dispatch_mesh(list, x));
    }
    pub(super) extern "system" fn dispatch_mesh_x(
        this: This,
        x: u32,
        _flags: u64,
        indirect: u64,
        _payload: u64,
    ) {
        super::count_draw();
        if indirect != 0 {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("indirect mesh dispatches dropped"));
            return;
        }
        with_state(this, |list, s| s.dispatch_mesh(list, x));
    }
    pub(super) extern "system" fn draw_indexed(
        this: This,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        super::count_draw();
        with_state(this, |list, s| {
            s.draw_indexed(list, indices, instances, first, base, first_instance)
        });
    }
}

#[cfg(not(windows))]
mod state {
    pub(super) use super::dispatch;
    pub(super) use super::draw;
    pub(super) use super::draw as draw_indexed;
    pub(super) use super::draw as dispatch_mesh;
    pub(super) use super::draw as dispatch_mesh_x;
    pub(super) use super::ignored_state as blend_factor;
    pub(super) use super::ignored_state as compute_constant;
    pub(super) use super::ignored_state as compute_constants;
    pub(super) use super::ignored_state as compute_table;
    pub(super) use super::ignored_state as compute_view;
    pub(super) use super::ignored_state as graphics_constant;
    pub(super) use super::ignored_state as graphics_constants;
    pub(super) use super::ignored_state as graphics_table;
    pub(super) use super::ignored_state as graphics_view;
    pub(super) use super::ignored_state as heaps;
    pub(super) use super::ignored_state as index_buffer;
    pub(super) use super::ignored_state as scissors;
    pub(super) use super::ignored_state as stencil_ref;
    pub(super) use super::ignored_state as depth_bounds;
    pub(super) use super::ignored_state as targets;
    pub(super) use super::ignored_state as topology;
    pub(super) use super::ignored_state as vertex_buffers;
    pub(super) use super::ignored_state as viewports;
}

fn count_draw() {
    DRAWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(windows))]
extern "system" fn draw(_: This) {
    count_draw();
}

extern "system" fn execute_indirect(
    this: This,
    signature: This,
    max: u32,
    arguments: This,
    offset: u64,
    count: This,
    count_offset: u64,
) {
    count_draw();
    let Some((stride, args)) = crate::signature::layout(signature) else {
        return;
    };
    let address = crate::resource::buffer_bytes(arguments).map(|(p, _)| p as u64 + offset);
    let counter = crate::resource::buffer_bytes(count).map(|(p, _)| p as u64 + count_offset);
    let mut ran = false;
    #[cfg(windows)]
    if let ([[kind, ..]], Some(address)) = (args, address)
        && (count.is_null() || counter.is_some())
    {
        const DISPATCH: u32 = 2;
        const DISPATCH_SINGLE: u32 = 105;
        let kind = if *kind == DISPATCH_SINGLE {
            DISPATCH
        } else {
            *kind
        };
        with_state(this, |list, s| {
            ran = s.execute_indirect(list, (kind, stride, max), address, counter);
        });
    } else {
        #[cfg(all(windows, feature = "diagnostics"))]
        crate::pipeline::note_why(format!(
            "indirect signature {args:?}: arguments in a buffer {}, count in a buffer {}",
            address.is_some(),
            count.is_null() || counter.is_some()
        ));
    }
    if ran {
        return;
    }
    INDIRECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    static SEEN: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&(signature as usize)) {
        seen.push(signature as usize);
        tracing::info!(
            stride,
            ?args,
            max,
            address = ?address.map(|a| format!("{a:#x}")),
            counted = !count.is_null(),
            "indirect execution dropped (first time for this signature)"
        );
    }
}

pub(crate) fn take_indirect() -> u64 {
    INDIRECT.swap(0, std::sync::atomic::Ordering::Relaxed)
}
static INDIRECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn count_dispatch() {
    DISPATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(not(windows))]
extern "system" fn dispatch(_: This) {
    count_dispatch();
}
static DRAWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DISPATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn take_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (DRAWS.swap(0, Relaxed), DISPATCHES.swap(0, Relaxed))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ValueWait {
    address: u64,
    comparison: u32,
    reference: u64,
    wide: bool,
}

impl ValueWait {
    pub(crate) fn met(&self) -> bool {
        #[cfg(all(windows, feature = "diagnostics"))]
        let started = std::time::Instant::now();
        let met = self.holds();
        #[cfg(all(windows, feature = "diagnostics"))]
        {
            use crate::copystat::{C, add};
            add(C::WaitChecks, 1);
            add(C::WaitCheckNs, started.elapsed().as_nanos() as u64);
        }
        met
    }

    fn holds(&self) -> bool {
        let bytes = if self.wide { 8 } else { 4 };
        if self.address == 0 {
            return true;
        }
        static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ASK.get_or_init(|| std::env::var_os("OATHKEEP_WAIT_ASK").is_some_and(|v| v == "1"))
            && !crate::texture::committed(self.address as *const u8, bytes)
        {
            return true;
        }
        let mut raw = [0u8; 8];
        if !crate::host::read_into(self.address as *const u8, &mut raw[..bytes]) {
            use windows_sys::Win32::System::Memory::{
                MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
            };
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            let n =
                unsafe { VirtualQuery(self.address as *const _, &mut info, size_of_val(&info)) };
            if n != 0
                && info.State == MEM_RESERVE
                && (info.Type == MEM_MAPPED
                    || (info.Type == MEM_PRIVATE
                        && crate::watch::is_demand_reserved(self.address as usize)))
            {
                return self.holds_for(0);
            }
            return true;
        }
        self.holds_for(u64::from_le_bytes(raw))
    }

    #[cfg(windows)]
    pub(crate) fn place(&self) -> (usize, usize) {
        (self.address as usize, if self.wide { 8 } else { 4 })
    }

    pub(crate) fn holds_for(&self, value: u64) -> bool {
        let (value, reference) = if self.wide {
            (value, self.reference)
        } else {
            (value & 0xFFFF_FFFF, self.reference & 0xFFFF_FFFF)
        };
        match self.comparison {
            1 => false,
            2 => value < reference,
            3 => value == reference,
            4 => value <= reference,
            5 => value > reference,
            6 => value != reference,
            7 => value >= reference,
            _ => true,
        }
    }
}

static VALUE_WAITS: std::sync::Mutex<Vec<(usize, ValueWait)>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn unmet_waits(lists: &[This]) -> Vec<ValueWait> {
    let mut waits = VALUE_WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let mut unmet = Vec::new();
    waits.retain(|&(list, wait)| {
        if !lists.iter().any(|&l| l as usize == list) {
            return true;
        }
        if !wait.met() {
            unmet.push(wait);
        }
        false
    });
    unmet
}

#[cfg(windows)]
pub(crate) fn waits_by_list(lists: &[This]) -> Vec<Vec<ValueWait>> {
    let mut waits = VALUE_WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let mut by = vec![Vec::new(); lists.len()];
    waits.retain(
        |&(list, wait)| match lists.iter().position(|&l| l as usize == list) {
            Some(i) => {
                by[i].push(wait);
                false
            }
            None => true,
        },
    );
    by
}

fn wait_for(this: This, address: u64, comparison: u32, reference: u64, wide: bool) {
    let wait = ValueWait {
        address,
        comparison,
        reference,
        wide,
    };
    static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 12 {
        tracing::info!(
            address = format_args!("{address:#x}"),
            comparison,
            reference,
            wide,
            met = wait.met(),
            "a list waits for a memory value"
        );
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    if let Some(host) = host_list(this) {
        crate::capture::op(std::ptr::from_ref(host) as usize, || {
            format!(
                "WAIT VALUE at {address:#x} comparison {comparison} reference {reference:#x} wide {wide} met {}",
                wait.met()
            )
        });
    }
    #[cfg(windows)]
    if crate::queue::cuts() {
        cut(this, wait);
        return;
    }
    VALUE_WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((this as usize, wait));
}
extern "system" fn wait_value(this: This, address: u64, comparison: u32, reference: u64) {
    wait_for(this, address, comparison, reference, false);
}
extern "system" fn wait_value64(this: This, address: u64, comparison: u32, reference: u64) {
    wait_for(this, address, comparison, reference, true);
}

extern "system" fn barrier_groups(this: This, count: u32, _groups: *const u8) {
    #[cfg(all(windows, feature = "diagnostics"))]
    if count > 0 {
        crate::copystat::add(crate::copystat::C::WaitsForGroups, 1);
        if crate::copystat::barrier_test() & 1 != 0 {
            return;
        }
    }
    #[cfg(windows)]
    if count > 0
        && let Some(list) = record_host(this)
    {
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
        // SAFETY: the same layout; no resource named.
        let barrier =
            unsafe { std::mem::transmute::<Barrier, D3D12_RESOURCE_BARRIER>(UAV_BARRIER) };
        // SAFETY: an open list.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
        list.mark_uav_clean();
    }
    #[cfg(not(windows))]
    let _ = (this, count);
}

extern "system" fn write32(this: This, address: u64, value: u32, _flush: u32, _flags: u32) {
    defer_write(this, address, Written::Value(u64::from(value)), 4);
    value_written(this, address, u64::from(value));
}
extern "system" fn write64(this: This, address: u64, value: u64, _flush: u32, flags: u32) {
    const GPU_TIMESTAMP: u32 = 1;
    const SYSTEM_TIMESTAMP: u32 = 2;
    let what = match flags {
        GPU_TIMESTAMP => Written::GpuTimestamp,
        SYSTEM_TIMESTAMP => Written::SystemTimestamp,
        _ => Written::Value(value),
    };
    defer_write(this, address, what, 8);
    if let Written::Value(value) = what {
        value_written(this, address, value);
    }
}

fn value_written(this: This, address: u64, value: u64) {
    #[cfg(windows)]
    {
        if address == 0 || this.is_null() {
            return;
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = value;
        #[cfg(feature = "diagnostics")]
        if let Some(host) = host_list(this) {
            crate::capture::op(std::ptr::from_ref(host) as usize, || {
                format!("WRITE VALUE at {address:#x} value {value:#x}")
            });
        }
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_CUT_WRITES").is_some_and(|v| v == "1"))
            || !crate::queue::cuts()
        {
            return;
        }
        // SAFETY: the title passes one of our lists; a list is recorded by
        // one thread at a time and is not submitted while it records.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        let Some(current) = list.host.as_ref() else {
            return;
        };
        let Some(fresh) = fresh_list() else {
            return;
        };
        current.close();
        if let Some(done) = list.host.replace(fresh) {
            let waits = std::mem::take(&mut list.gate);
            list.earlier.push(Part { waits, host: done });
        }
        if let Some(host) = list.host.as_ref() {
            list.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .replay(host);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, address, value);
}

fn defer_write(this: This, address: u64, what: Written, bytes: usize) {
    if address == 0 {
        return;
    }
    #[cfg(windows)]
    if let Some(list) = record_host(this) {
        list.defer_write(address as *mut u8, what, bytes);
        return;
    }
    let _ = this;
    what.write(address as *mut u8, bytes);
}

const METHODS: [(usize, *const ()); 52] = [
    (0, list_query as *const ()),
    (2, list_release as *const ()),
    (7, object::parent_device as *const ()),
    (8, list_type as *const ()),
    (9, close as *const ()),
    (10, reset as *const ()),
    (12, state::draw as *const ()),
    (13, state::draw_indexed as *const ()),
    (14, state::dispatch as *const ()),
    (15, copy_buffer_region as *const ()),
    (16, copy_texture_region as *const ()),
    (17, copy_resource as *const ()),
    (19, resolve as *const ()),
    (20, state::topology as *const ()),
    (21, state::viewports as *const ()),
    (22, state::scissors as *const ()),
    (23, state::blend_factor as *const ()),
    (24, state::stencil_ref as *const ()),
    (25, set_pipeline as *const ()),
    (26, barrier as *const ()),
    (32, state::heaps as *const ()),
    (33, ignored_state as *const ()),
    (34, ignored_state as *const ()),
    (35, state::compute_table as *const ()),
    (36, state::graphics_table as *const ()),
    (37, state::compute_constant as *const ()),
    (38, state::graphics_constant as *const ()),
    (39, state::compute_constants as *const ()),
    (40, state::graphics_constants as *const ()),
    (41, state::compute_view as *const ()),
    (42, state::graphics_view as *const ()),
    (43, state::compute_view as *const ()),
    (44, state::graphics_view as *const ()),
    (45, state::compute_view as *const ()),
    (46, state::graphics_view as *const ()),
    (47, state::index_buffer as *const ()),
    (48, state::vertex_buffers as *const ()),
    (50, state::targets as *const ()),
    (51, clear_dsv as *const ()),
    (52, clear_rtv as *const ()),
    (59, execute_indirect as *const ()),
    (60, copy_memory as *const ()),
    (68, state::depth_bounds as *const ()),
    (69, write32 as *const ()),
    (70, write64 as *const ()),
    (71, wait_value as *const ()),
    (72, wait_value64 as *const ()),
    (73, ignored_state as *const ()),
    (83, copy_texture_region_x as *const ()),
    (84, copy_resource_x as *const ()),
    (124, ignored_state as *const ()),
    (127, state::dispatch_mesh as *const ()),
];
const MORE_METHODS: [(usize, *const ()); 9] = [
    (28, query_mark as *const ()),
    (29, query_mark as *const ()),
    (30, resolve_queries as *const ()),
    (78, dispatch_x as *const ()),
    (98, state::dispatch_mesh_x as *const ()),
    (99, driver_hint as *const ()),
    (129, sync as *const ()),
    (130, sync as *const ()),
    (137, reset_x as *const ()),
];

const fn newer_slot(slot: usize) -> usize {
    if slot >= 59 { slot + 2 } else { slot }
}

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

static LIST_FUNCTIONS: Table<160> = Table(list_table(false));
static NEWER_LIST_FUNCTIONS: Table<160> = Table(list_table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[test]
    fn every_inline_command_falls_back_to_the_table() {
        let null = std::ptr::null();
        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        let hr = create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list);
        assert_eq!(hr, S_OK);
        // SAFETY: the list is 0x560 bytes.
        let (put, draw_limit, api_limit) = unsafe {
            let at = |o: usize| list.cast::<u8>().add(o).cast::<u64>().read();
            (at(0x18), at(0x28), at(0x30))
        };
        assert!(put >= draw_limit && put >= api_limit);
        let mut same = null;
        assert_eq!(list_query(list, &LIST_IIDS[7], &mut same), S_OK);
        assert_eq!(same, list);
        assert_eq!(slot::<extern "system" fn(This) -> u32>(list, 0x10)(list), 1);
        assert_eq!(slot::<extern "system" fn(This) -> u32>(list, 0x40)(list), 0);
        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list),
            S_OK
        );
        let p = allocate_upload(list, 40, 0x100);
        assert!(!p.is_null() && (p as usize).is_multiple_of(0x100));
        // SAFETY: 40 bytes were handed out.
        unsafe { p.write_bytes(0xab, 40) };
        assert!(
            allocate_upload(list, 8, 0x30).is_null(),
            "not a power of two"
        );
        assert_eq!(reset(list, allocator, null), S_OK);
        // SAFETY: the list is ours.
        assert!(
            unsafe { &(*list.cast::<List>()).uploads }
                .lock()
                .unwrap()
                .is_empty()
        );
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(list), 0);
        assert_eq!(release(allocator), 0);
    }
}
