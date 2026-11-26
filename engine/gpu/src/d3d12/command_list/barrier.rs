use super::This;
#[cfg(windows)]
use super::{host_list, with_state};

/// A resource barrier as titles pass it: type, flags, then a resource and
/// either a subresource and two states (transition) or a second resource
/// (aliasing). The same layout as the host's.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Barrier {
    kind: u32,
    flags: u32,
    first: This,
    second: u64,
    last: u64,
}

/// The barrier on host resources, or `None` if it names a resource the host
/// GPU does not have (buffers so far).
#[cfg(windows)]
fn host_barrier(list: &crate::backend::HostList, b: &Barrier) -> Option<Barrier> {
    use windows::core::Interface;
    let host = |r: This| crate::d3d12::resource::host_resource(r).map(|h| h.as_raw().cast_const().cast());
    match b.kind {
        0 => {
            // A split barrier's first half: its second half transitions.
            const BEGIN_ONLY: u32 = 1;
            if b.flags & BEGIN_ONLY != 0 {
                return None;
            }
            // shortcut: whole resources, from the state the list tracked
            // (`HostList::transition`); subresources are not told apart.
            let wanted = crate::backend::texture_state(b.last as u32);
            let (texture, before) = list.transition(b.first, wanted)?;
            Some(Barrier {
                kind: 0,
                flags: 0,
                first: texture.as_raw().cast_const().cast(),
                second: 0xffff_ffff | (u64::from(before) << 32),
                last: u64::from(crate::backend::legal_state(texture, wanted)),
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
                // A depth texture's unordered access is its colour twin's
                // (the host gives the depth texture none): the barrier is
                // on the texture shaders write.
                match crate::d3d12::resource::twin_resource(b.first) {
                    Some(twin) => twin.as_raw().cast_const().cast(),
                    None => host(b.first)?,
                }
            },
            ..*b
        }),
        _ => None,
    }
}

/// An unordered access barrier on every resource: earlier shader writes
/// finish before later work reads or writes.
const UAV_BARRIER: Barrier = Barrier {
    kind: 2,
    flags: 0,
    first: std::ptr::null(),
    second: 0,
    last: 0,
};

/// Returns the console's barrier "actions" (none).
pub(super) extern "system" fn barrier(this: This, count: u32, barriers: *const Barrier) -> u32 {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && !barriers.is_null()
    {
        if count > 0 {
            with_state(this, |_, state| state.synced());
        }
        // SAFETY: the title passes `count` barriers.
        let barriers = unsafe { std::slice::from_raw_parts(barriers, count as usize) };
        let mut host: Vec<Barrier> = barriers
            .iter()
            .filter_map(|b| host_barrier(list, b))
            .collect();
        // Buffers have host copies, not resources the barrier can name:
        // a barrier on one waits for every shader write before it.
        if barriers.iter().any(|b| {
            b.kind != 1 && !b.first.is_null() && crate::d3d12::resource::buffer_bytes(b.first).is_some()
        }) {
            host.push(UAV_BARRIER);
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::WaitsForBuffers, 1);
        }
        if !host.is_empty() {
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
        }
    }
    #[cfg(not(windows))]
    let _ = (this, count, barriers);
    0
}

/// Moves a title texture's host copy to `state` in the list, if needed.
#[cfg(windows)]
pub(crate) fn ensure_state(list: &crate::backend::HostList, resource: This, state: u32) {
    use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
    if let Some((texture, before)) = list.transition(resource, state) {
        let state = crate::backend::legal_state(texture, state);
        let barrier = crate::backend::transition(texture, State(before as i32), State(state as i32));
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
    }
}

/// The enhanced barriers `(group count, groups)`: buffer, texture and
/// global barriers with layouts instead of states. Every host use of a
/// title texture (clears, copies) moves its host copy to the state it needs
/// first, so only their ordering matters: one barrier on every shader
/// write (the title's buffer barriers order compute passes).
pub(super) extern "system" fn barrier_groups(this: This, count: u32, _groups: *const u8) {
    // Diagnostics: counted, and left out under `OATHKEEP_BARRIER_TEST=groups`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if count > 0 {
        crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::WaitsForGroups, 1);
        if crate::diagnostics::copy_stats::barrier_test() & 1 != 0 {
            return;
        }
    }
    #[cfg(windows)]
    if count > 0
        && let Some(list) = host_list(this)
    {
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
        with_state(this, |_, state| state.synced());
        // SAFETY: the same layout; no resource named.
        let barrier = unsafe { std::mem::transmute::<Barrier, D3D12_RESOURCE_BARRIER>(UAV_BARRIER) };
        // SAFETY: an open list.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
    }
    #[cfg(not(windows))]
    let _ = (this, count);
}

/// Pipeline syncs: waits for earlier work and cache flushes, at the top or
/// bottom of the pipe `(flags, control, address, size)`. The host orders
/// nothing between dispatches by itself: what shaders wrote is waited for
/// here as at the title's barriers (the ray-tracing structure builder has
/// no other wait between its stages).
/// (Arguments may be what earlier shaders wrote from here on:
/// `State::synced`.)
pub(super) extern "system" fn sync(this: This, flags: u32, control: u32, address: u64, size: u64) {
    #[cfg(windows)]
    {
        with_state(this, |_, state| state.synced());
        if let Some(list) = host_list(this) {
            use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
            #[cfg(feature = "diagnostics")]
            {
                // The shapes of the title's calls, each once.
                static SEEN: std::sync::Mutex<Vec<(u32, u32, bool)>> = std::sync::Mutex::new(Vec::new());
                let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                let shape = (flags, control, address == 0);
                if seen.len() < 64 && !seen.contains(&shape) {
                    seen.push(shape);
                    tracing::info!(
                        flags = format_args!("{flags:#x}"),
                        control = format_args!("{control:#x}"),
                        address = format_args!("{address:#x}"),
                        size = format_args!("{size:#x}"),
                        "a pipeline sync"
                    );
                }
                crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::WaitsForGroups, 1);
            }
            // SAFETY: the same layout; no resource named.
            let barrier = unsafe { std::mem::transmute::<Barrier, D3D12_RESOURCE_BARRIER>(UAV_BARRIER) };
            // SAFETY: an open list.
            unsafe { list.list.ResourceBarrier(&[barrier]) };
        }
    }
    let _ = (this, flags, control, address, size);
}
