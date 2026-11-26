use super::{Barrier, CopyLocation, E_INVALIDARG, HRESULT, List, Rect, S_OK, This, Upload, draw_state as state, front};
use crate::descriptors::RtvRecord;
use crate::execution::draw::{IndexView, VertexView};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

/// An argument as a word.
pub(super) trait Word: Copy {
    fn word(self) -> u64;
    fn unword(w: u64) -> Self;
}
macro_rules! words {
    ($($t:ty),*) => { $(impl Word for $t {
        #[inline]
        fn word(self) -> u64 { self as u64 }
        #[inline]
        fn unword(w: u64) -> Self { w as $t }
    })* };
}
words!(u8, u32, i32, u64, This);
impl Word for f32 {
    #[inline]
    fn word(self) -> u64 {
        u64::from(self.to_bits())
    }
    #[inline]
    fn unword(w: u64) -> Self {
        f32::from_bits(w as u32)
    }
}

/// The methods whose arguments are all plain values: the one the title calls,
/// and the call made of it later.
macro_rules! plain {
    ($($op:ident $name:ident = $was:path, ($($a:ident: $t:ty),*);)*) => {
        #[allow(clippy::upper_case_acronyms, non_camel_case_types)]
        #[repr(u64)]
        pub(super) enum Plain { $($op),* , END }
        $(pub(super) extern "system" fn $name(this: This $(, $a: $t)*) {
            front(this).put(Plain::$op as u64, &[$(Word::word($a)),*]);
        })*
        /// Whether `op` was one of them; done on `list`.
        #[inline]
        pub(super) fn run_plain(op: u64, list: This, w: &[u64]) -> bool {
            match op {
                $(n if n == Plain::$op as u64 => {
                    let mut words = w.iter();
                    $(let $a: $t = Word::unword(words.next().copied().unwrap_or(0));)*
                    let _ = words;
                    $was(list $(, $a)*);
                })*
                _ => return false,
            }
            true
        }
    };
}

plain! {
    TOPOLOGY topology = state::topology, (v: u32);
    STENCIL_REF stencil_ref = state::stencil_ref, (v: u32);
    DEPTH_BOUNDS depth_bounds = state::depth_bounds, (min: f32, max: f32);
    COMPUTE_TABLE compute_table = state::compute_table, (slot: u32, handle: u64);
    GRAPHICS_TABLE graphics_table = state::graphics_table, (slot: u32, handle: u64);
    COMPUTE_CONSTANT compute_constant = state::compute_constant, (slot: u32, value: u32, offset: u32);
    GRAPHICS_CONSTANT graphics_constant = state::graphics_constant, (slot: u32, value: u32, offset: u32);
    COMPUTE_VIEW compute_view = state::compute_view, (slot: u32, address: u64);
    GRAPHICS_VIEW graphics_view = state::graphics_view, (slot: u32, address: u64);
    SET_PIPELINE set_pipeline = super::super::set_pipeline, (pipeline: This);
    DRAW draw = state::draw, (vertices: u32, instances: u32, first: u32, first_instance: u32);
    DRAW_INDEXED draw_indexed = state::draw_indexed, (indices: u32, instances: u32, first: u32, base: i32, first_instance: u32);
    DISPATCH dispatch = state::dispatch, (x: u32, y: u32, z: u32);
    DISPATCH_X dispatch_x = super::super::dispatch_x, (x: u32, y: u32, z: u32, flags: u32);
    DISPATCH_MESH dispatch_mesh = state::dispatch_mesh, (x: u32, y: u32, z: u32);
    DISPATCH_MESH_X dispatch_mesh_x = state::dispatch_mesh_x, (x: u32, flags: u64, indirect: u64, payload: u64);
    COPY_BUFFER copy_buffer_region = super::super::copy_buffer_region, (dst: This, dst_offset: u64, src: This, src_offset: u64, len: u64);
    COPY_RESOURCE copy_resource = super::super::copy_resource, (dst: This, src: This);
    COPY_RESOURCE_X copy_resource_x = super::super::copy_resource_x, (dst: This, src: This, flags: u32);
    COPY_MEMORY copy_memory = super::super::copy_memory, (dst: u64, src: u64, len: u64, flags: u32);
    RESOLVE resolve = super::super::resolve, (dst: This, dst_sub: u32, src: This, src_sub: u32, format: u32);
    RESOLVE_QUERIES resolve_queries = super::super::resolve_queries, (heap: This, kind: u32, first: u32, count: u32, buffer: This, offset: u64);
    EXECUTE_INDIRECT execute_indirect = super::super::execute_indirect, (signature: This, max: u32, arguments: This, offset: u64, count: This, count_offset: u64);
    WRITE32 write32 = super::super::write32, (address: u64, value: u32, flush: u32, flags: u32);
    WRITE64 write64 = super::super::write64, (address: u64, value: u64, flush: u32, flags: u32);
    WAIT_VALUE wait_value = super::super::wait_value, (address: u64, comparison: u32, reference: u64);
    WAIT_VALUE64 wait_value64 = super::super::wait_value64, (address: u64, comparison: u32, reference: u64);
    SYNC sync = super::super::sync, (flags: u32, control: u32, address: u64, size: u64);
}

const fn after(n: u64) -> u64 {
    Plain::END as u64 + n
}
pub(super) const RESET: u64 = after(0);
pub(super) const CLOSE: u64 = after(1);
pub(super) const UPLOAD: u64 = after(2);
pub(super) const BARRIER: u64 = after(3);
pub(super) const BARRIER_GROUPS: u64 = after(4);
pub(super) const VIEWPORTS: u64 = after(5);
pub(super) const SCISSORS: u64 = after(6);
pub(super) const BLEND_FACTOR: u64 = after(7);
pub(super) const HEAPS: u64 = after(8);
pub(super) const COMPUTE_CONSTANTS: u64 = after(9);
pub(super) const GRAPHICS_CONSTANTS: u64 = after(10);
pub(super) const INDEX_BUFFER: u64 = after(11);
pub(super) const VERTEX_BUFFERS: u64 = after(12);
pub(super) const TARGETS: u64 = after(13);
pub(super) const CLEAR_RTV: u64 = after(14);
pub(super) const CLEAR_DSV: u64 = after(15);
pub(super) const COPY_TEXTURE: u64 = after(16);
pub(super) const CLEAR_UAV_UINT: u64 = after(17);
pub(super) const CLEAR_UAV_FLOAT: u64 = after(18);

/// The bytes of `count` things at `items` (none if null).
pub(super) fn bytes_of<'a, T>(items: *const T, count: usize) -> &'a [u8] {
    if items.is_null() || count == 0 {
        return &[];
    }
    // SAFETY: the title passes `count` items.
    unsafe { std::slice::from_raw_parts(items.cast(), count * size_of::<T>()) }
}

/// Where the things are in a stream (null for none).
pub(super) fn at<T>(w: &[u64], from: usize, count: usize) -> *const T {
    if count == 0 || from >= w.len() { std::ptr::null() } else { w[from..].as_ptr().cast() }
}

/// [`at`] where an empty array is not null.
pub(super) fn at_or<T>(w: &[u64], from: usize) -> *const T {
    // SAFETY: at most one past the end.
    unsafe { w.as_ptr().add(from.min(w.len())).cast() }
}

/// A view's record as it is now (zeros for no view), so that its slot may be
/// written again: the copy stands for the handle.
pub(super) fn record(handle: u64) -> [u64; RECORD] {
    let mut words = [0u64; RECORD];
    if handle != 0 {
        // SAFETY: a descriptor slot the title passes (they are larger).
        unsafe {
            std::ptr::copy_nonoverlapping(handle as *const u8, words.as_mut_ptr().cast(), size_of::<RtvRecord>())
        };
    }
    words
}
pub(super) const RECORD: usize = size_of::<RtvRecord>().div_ceil(8);

pub(super) extern "system" fn list_close(this: This) -> HRESULT {
    front(this).close();
    S_OK
}
pub(super) extern "system" fn list_reset(this: This, allocator: This, pipeline: This) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes one of our lists.
    let kind = unsafe { (*this.cast::<List>()).kind };
    front(this).begin(kind, pipeline);
    S_OK
}
pub(super) extern "system" fn list_reset_x(this: This, allocator: This, pipeline: This, _flags: u32) -> HRESULT {
    list_reset(this, allocator, pipeline)
}
pub(super) extern "system" fn allocate_upload(this: This, size: u64, alignment: u64) -> *mut u8 {
    let align = usize::try_from(alignment).unwrap_or(0).max(16);
    let Some(len) = usize::try_from(size).ok().and_then(|s| s.checked_add(align)) else {
        return std::ptr::null_mut();
    };
    if this.is_null() || !align.is_power_of_two() {
        return std::ptr::null_mut();
    }
    let front = front(this);
    if front.closed || front.recording.is_none() {
        return std::ptr::null_mut();
    }
    let Some(mut block) = Upload::new(len) else {
        return std::ptr::null_mut();
    };
    let start = block.start();
    // SAFETY: the offset is below `align`, so `size` bytes fit after it.
    let place = unsafe { start.add(start.align_offset(align)) };
    front.put(UPLOAD, &[Box::into_raw(Box::new(block)) as u64]);
    place
}
pub(super) extern "system" fn barrier(this: This, count: u32, barriers: *const Barrier) -> u32 {
    if !barriers.is_null() {
        front(this).put_with(BARRIER, &[u64::from(count)], &[bytes_of(barriers, count as usize)]);
    }
    0
}
pub(super) extern "system" fn barrier_groups(this: This, count: u32, _groups: *const u8) {
    front(this).put(BARRIER_GROUPS, &[u64::from(count)]);
}
pub(super) extern "system" fn viewports(this: This, count: u32, items: *const D3D12_VIEWPORT) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(VIEWPORTS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
pub(super) extern "system" fn scissors(this: This, count: u32, items: *const RECT) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(SCISSORS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
pub(super) extern "system" fn blend_factor(this: This, factor: *const [f32; 4]) {
    let given = !factor.is_null();
    front(this).put_with(BLEND_FACTOR, &[u64::from(given)], &[bytes_of(factor, usize::from(given))]);
}
pub(super) extern "system" fn heaps(this: This, count: u32, items: *const This) {
    let count = if items.is_null() { 0 } else { count };
    front(this).put_with(HEAPS, &[u64::from(count)], &[bytes_of(items, count as usize)]);
}
fn constants(this: This, op: u64, slot: u32, count: u32, values: *const u32, offset: u32) {
    let count = if values.is_null() { 0 } else { count };
    front(this).put_with(
        op,
        &[u64::from(slot) | u64::from(offset) << 32, u64::from(count)],
        &[bytes_of(values, count as usize)],
    );
}
pub(super) extern "system" fn compute_constants(this: This, slot: u32, count: u32, values: *const u32, offset: u32) {
    constants(this, COMPUTE_CONSTANTS, slot, count, values, offset);
}
pub(super) extern "system" fn graphics_constants(this: This, slot: u32, count: u32, values: *const u32, offset: u32) {
    constants(this, GRAPHICS_CONSTANTS, slot, count, values, offset);
}
pub(super) extern "system" fn index_buffer(this: This, view: *const IndexView) {
    let given = !view.is_null();
    front(this).put_with(INDEX_BUFFER, &[u64::from(given)], &[bytes_of(view, usize::from(given))]);
}
pub(super) extern "system" fn vertex_buffers(this: This, start: u32, count: u32, views: *const VertexView) {
    let given = !views.is_null();
    front(this).put_with(
        VERTEX_BUFFERS,
        &[u64::from(start) | u64::from(count) << 32, u64::from(given)],
        &[bytes_of(views, count as usize)],
    );
}
pub(super) extern "system" fn targets(this: This, count: u32, handles: *const u64, one_range: i32, depth: *const u64) {
    let count = if handles.is_null() { 0 } else { (count as usize).min(8) };
    // SAFETY: the title passes a handle or null.
    let depth = unsafe { depth.as_ref() }.copied().unwrap_or(0);
    let mut records = [0u64; RECORD * 9];
    let mut present = 0u64;
    for i in 0..count {
        // SAFETY: the title passes `count` handles, or the first of a range
        // of views of 128 bytes.
        let handle = unsafe { if one_range != 0 { *handles + 128 * i as u64 } else { *handles.add(i) } };
        present |= u64::from(handle != 0) << i;
        records[i * RECORD..(i + 1) * RECORD].copy_from_slice(&record(handle));
    }
    records[count * RECORD..(count + 1) * RECORD].copy_from_slice(&record(depth));
    let records = &records[..(count + 1) * RECORD];
    front(this).put_with(
        TARGETS,
        &[count as u64, present, u64::from(depth != 0)],
        &[bytes_of(records.as_ptr(), records.len())],
    );
}
pub(super) extern "system" fn clear_rtv(this: This, handle: u64, color: *const [f32; 4], count: u32, rects: *const Rect) {
    if handle == 0 || color.is_null() {
        return;
    }
    let count = if rects.is_null() { 0 } else { count };
    let view = record(handle);
    front(this).put_with(
        CLEAR_RTV,
        &[u64::from(count)],
        &[bytes_of(view.as_ptr(), RECORD), bytes_of(color, 1), bytes_of(rects, count as usize)],
    );
}
pub(super) extern "system" fn clear_dsv(
    this: This,
    handle: u64,
    flags: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rects: *const Rect,
) {
    if handle == 0 {
        return;
    }
    let count = if rects.is_null() { 0 } else { count };
    let view = record(handle);
    front(this).put_with(
        CLEAR_DSV,
        &[u64::from(count), u64::from(flags) | u64::from(stencil) << 32, Word::word(depth)],
        &[bytes_of(view.as_ptr(), RECORD), bytes_of(rects, count as usize)],
    );
}
pub(super) extern "system" fn clear_uav_uint(
    this: This,
    gpu: u64,
    cpu: u64,
    resource: This,
    values: *const [u32; 4],
    count: u32,
    rects: *const Rect,
) {
    if values.is_null() {
        return;
    }
    let count = if rects.is_null() { 0 } else { count };
    front(this).put_with(
        CLEAR_UAV_UINT,
        &[u64::from(count), gpu, cpu, resource as u64],
        &[bytes_of(values, 1), bytes_of(rects, count as usize)],
    );
}
pub(super) extern "system" fn clear_uav_float(
    this: This,
    gpu: u64,
    cpu: u64,
    resource: This,
    values: *const [f32; 4],
    count: u32,
    rects: *const Rect,
) {
    if values.is_null() {
        return;
    }
    let count = if rects.is_null() { 0 } else { count };
    front(this).put_with(
        CLEAR_UAV_FLOAT,
        &[u64::from(count), gpu, cpu, resource as u64],
        &[bytes_of(values, 1), bytes_of(rects, count as usize)],
    );
}
pub(super) extern "system" fn copy_texture_region(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
) {
    if dst.is_null() || src.is_null() {
        return;
    }
    let boxed = !source_box.is_null();
    front(this).put_with(
        COPY_TEXTURE,
        &[u64::from(x) | u64::from(y) << 32, u64::from(z) | u64::from(boxed) << 32],
        &[bytes_of(dst, 1), bytes_of(src, 1), bytes_of(source_box, usize::from(boxed))],
    );
}
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(super) extern "system" fn copy_texture_region_x(
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
