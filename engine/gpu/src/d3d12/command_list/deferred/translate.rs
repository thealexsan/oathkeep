use super::stream::{
    BARRIER, BARRIER_GROUPS, BLEND_FACTOR, CLEAR_DSV, CLEAR_RTV, CLEAR_UAV_FLOAT, CLEAR_UAV_UINT,
    CLOSE, COMPUTE_CONSTANTS, COPY_TEXTURE, GRAPHICS_CONSTANTS, HEAPS, INDEX_BUFFER, RECORD,
    RESET, SCISSORS, TARGETS, UPLOAD, VERTEX_BUFFERS, VIEWPORTS, Word, at, at_or, run_plain,
};
use super::{CopyLocation, List, This, Upload, draw_state as state, held};

/// Makes the calls of a stream on `list`: how many, and whether the last one
/// closed it. Once `left` says so only what must not be lost is done.
pub(super) fn translate(list: This, stream: &[u64], left: impl Fn() -> bool) -> (u64, bool) {
    let (mut i, mut calls, mut closed) = (0, 0, false);
    let mut skip = left();
    while i < stream.len() {
        let (op, len) = (stream[i] & 0xffff, (stream[i] >> 16) as usize);
        let w = &stream[i + 1..i + 1 + len];
        i += 1 + len;
        calls += 1;
        if calls % 64 == 0 {
            skip = left();
        }
        if op == UPLOAD {
            // SAFETY: made by `allocate_upload`, named once.
            let block = *unsafe { Box::from_raw(w[0] as *mut Upload) };
            // SAFETY: a list of ours.
            let uploads = unsafe { &(*list.cast::<List>()).uploads };
            held(uploads).push(block);
            continue;
        }
        if skip && op != CLOSE && op != RESET {
            continue;
        }
        if run_plain(op, list, w) {
            continue;
        }
        let n = w.first().map_or(0, |&n| n as usize);
        match op {
            RESET => {
                // (Only whether there is an allocator is looked at.)
                let allocator: This = std::ptr::NonNull::<u64>::dangling().as_ptr().cast_const().cast();
                super::super::reset(list, allocator, w[0] as This);
            }
            CLOSE => {
                super::super::close(list);
                closed = true;
            }
            BARRIER => {
                super::super::barrier(list, n as u32, at_or(w, 1));
            }
            BARRIER_GROUPS => super::super::barrier_groups(list, n as u32, std::ptr::null()),
            VIEWPORTS => state::viewports(list, n as u32, at(w, 1, n)),
            SCISSORS => state::scissors(list, n as u32, at(w, 1, n)),
            BLEND_FACTOR => state::blend_factor(list, at(w, 1, n)),
            HEAPS => state::heaps(list, n as u32, at(w, 1, n)),
            COMPUTE_CONSTANTS => {
                state::compute_constants(list, w[0] as u32, w[1] as u32, at(w, 2, w[1] as usize), (w[0] >> 32) as u32)
            }
            GRAPHICS_CONSTANTS => {
                state::graphics_constants(list, w[0] as u32, w[1] as u32, at(w, 2, w[1] as usize), (w[0] >> 32) as u32)
            }
            INDEX_BUFFER => state::index_buffer(list, at(w, 1, n)),
            VERTEX_BUFFERS => {
                let views = if w[1] != 0 { at_or(w, 2) } else { std::ptr::null() };
                state::vertex_buffers(list, w[0] as u32, (w[0] >> 32) as u32, views);
            }
            TARGETS => {
                let mut handles = [0u64; 8];
                let place = |k: usize| w[3 + k * RECORD..].as_ptr() as u64;
                for (k, h) in handles.iter_mut().enumerate().take(n) {
                    if w[1] >> k & 1 != 0 {
                        *h = place(k);
                    }
                }
                let depth = if w[2] != 0 { place(n) } else { 0 };
                state::targets(list, n as u32, handles.as_ptr(), 0, &depth);
            }
            CLEAR_RTV => {
                super::super::clear_rtv(
                    list,
                    w[1..].as_ptr() as u64,
                    at_or(w, 1 + RECORD),
                    n as u32,
                    at(w, 1 + RECORD + 2, n),
                );
            }
            CLEAR_DSV => {
                super::super::clear_dsv(
                    list,
                    w[3..].as_ptr() as u64,
                    w[1] as u32,
                    Word::unword(w[2]),
                    (w[1] >> 32) as u8,
                    n as u32,
                    at(w, 3 + RECORD, n),
                );
            }
            CLEAR_UAV_UINT => {
                super::super::clear_uav_uint(
                    list,
                    w[1],
                    w[2],
                    w[3] as This,
                    at_or(w, 4),
                    n as u32,
                    at(w, 6, n),
                );
            }
            CLEAR_UAV_FLOAT => {
                super::super::clear_uav_float(
                    list,
                    w[1],
                    w[2],
                    w[3] as This,
                    at_or(w, 4),
                    n as u32,
                    at(w, 6, n),
                );
            }
            COPY_TEXTURE => {
                let step = size_of::<CopyLocation>().div_ceil(8);
                let boxed = (w[1] >> 32) as usize;
                super::super::copy_texture_region(
                    list,
                    at_or(w, 2),
                    w[0] as u32,
                    (w[0] >> 32) as u32,
                    w[1] as u32,
                    at_or(w, 2 + step),
                    at(w, 2 + 2 * step, boxed),
                );
            }
            _ => {}
        }
    }
    (calls, closed)
}
