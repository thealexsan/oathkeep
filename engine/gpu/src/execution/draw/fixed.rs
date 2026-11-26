//! Render target binding, fixed-function state forwarding, and replay after a list cut.

use std::sync::atomic::Ordering;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::backend::HostList;

use super::state::State;
use super::{DEPTH_READ, DEPTH_WRITE, scissor_test};

/// What a render target or depth view of the host is made from: the host
/// texture, and [`crate::descriptors::RtvRecord::key`].
pub(super) type TargetKey = (usize, (usize, [u32; 7]));

/// The host state a depth view needs of its texture: the read state if the
/// view writes no plane the format has (the title's read-only flags), else
/// the write state.
/// shortcut: whole textures have one state, so a view that writes one of
/// two planes counts as writing.
pub(super) fn depth_state(
    texture: &ID3D12Resource,
    desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
) -> u32 {
    // D16_UNORM and D32_FLOAT: depth formats without a stencil plane.
    const ONE_PLANE: [i32; 2] = [55, 40];
    let Some(desc) = desc else {
        return DEPTH_WRITE;
    };
    let format = if desc.Format.0 == 0 {
        // SAFETY: a live texture; a view without a format has its own.
        unsafe { texture.GetDesc() }.Format.0
    } else {
        desc.Format.0
    };
    let mut needed = D3D12_DSV_FLAG_READ_ONLY_DEPTH.0;
    if !ONE_PLANE.contains(&format) {
        needed |= D3D12_DSV_FLAG_READ_ONLY_STENCIL.0;
    }
    if desc.Flags.0 & needed == needed {
        DEPTH_READ
    } else {
        DEPTH_WRITE
    }
}

/// Fixed-function state, straight onto the host list.
pub(crate) fn viewports(list: &HostList, viewports: &[D3D12_VIEWPORT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetViewports(viewports) };
}

pub(crate) fn scissors(list: &HostList, rects: &[RECT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetScissorRects(rects) };
}

pub(crate) fn blend_factor(list: &HostList, factor: Option<&[f32; 4]>) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetBlendFactor(factor) };
}

pub(crate) fn stencil_ref(list: &HostList, reference: u32) {
    #[cfg(feature = "diagnostics")]
    crate::diagnostics::capture::op(std::ptr::from_ref(list) as usize, || format!("STENCIL REF {reference:#x}"));
    // SAFETY: an open list.
    unsafe { list.list.OMSetStencilRef(reference) };
}

pub(crate) fn depth_bounds(list: &HostList, min: f32, max: f32) {
    #[cfg(feature = "diagnostics")]
    crate::diagnostics::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("DEPTH BOUNDS {min}..{max}")
    });
    if crate::backend::host().is_some_and(|h| h.depth_bounds())
        && let Ok(l1) = list.list.cast::<ID3D12GraphicsCommandList1>()
    {
        // SAFETY: an open list on a device that supports depth bounds.
        unsafe { l1.OMSetDepthBounds(min, max) };
    }
}

impl State {
    /// Render targets and depth, set on the host list at once, unless it has
    /// the same ones already: the title sets the same targets again and
    /// again, each time a host view per target and a call of the driver
    /// (25,662 times in a frame of another fork). `OATHKEEP_BIND_CACHE=0`
    /// sets them every time.
    pub(crate) fn set_targets(&mut self, list: &HostList, handles: &[u64], depth: u64) {
        let Some(host) = crate::backend::host() else {
            return;
        };
        self.target_handles.clear();
        self.target_handles.extend_from_slice(handles);
        self.depth_handle = depth;
        self.targets_set = true;
        self.targets.clear();
        let mut views = Vec::with_capacity(handles.len());
        let mut keys = Vec::with_capacity(handles.len());
        for &h in handles {
            let Some(record) = crate::descriptors::rtv_record(h) else {
                once!(
                    handle = format_args!("{h:#x}"),
                    "render target without a recorded view"
                );
                continue;
            };
            let Some(texture) = crate::d3d12::resource::host_resource(record.resource) else {
                // A typed null view (or a target without a host copy) still
                // takes its slot: left out, the next live target would move
                // into it and the pixel shader's outputs reach the wrong
                // targets.
                // shortcut: a view without a description or a format has no
                // null view to stand for it and is left out, as before.
                let null = record
                    .desc()
                    .filter(|d| d.Format.0 != 0 && d.ViewDimension.0 != 0);
                if let Some(desc) = null {
                    views.push((None, Some(desc)));
                    keys.push((0, record.key()));
                }
                continue;
            };
            self.targets.push(record.resource as usize);
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::capture::note_target(record.resource);
            views.push((Some(texture), record.desc()));
            keys.push((texture.as_raw() as usize, record.key()));
        }
        self.depth = 0;
        self.depth_state = DEPTH_WRITE;
        let dsv = crate::descriptors::dsv_record(depth).and_then(|record| {
            let texture = crate::d3d12::resource::host_resource(record.resource)?;
            self.depth = record.resource as usize;
            self.depth_state = depth_state(texture, record.depth_desc().as_ref());
            if crate::d3d12::resource::depth_and_unordered(record.resource) {
                crate::d3d12::resource::DEPTH_UNORDERED[1].fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::capture::note_target(record.resource);
            Some((texture, record))
        });
        let now = (keys, dsv.as_ref().map(|(texture, record)| (texture.as_raw() as usize, record.key())));
        static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *CACHE.get_or_init(|| std::env::var("OATHKEEP_BIND_CACHE").map_or(true, |v| v != "0"))
            && self.host_targets.as_ref() == Some(&now)
        {
            return;
        }
        self.host_targets = Some(now);
        let rtvs: Vec<_> = views.iter().map(|(texture, desc)| host.rtv(*texture, desc.as_ref())).collect();
        let dsv = dsv.map(|(texture, record)| host.dsv(texture, record.depth_desc().as_ref()));
        // SAFETY: an open list; the views were just made.
        unsafe {
            list.list.OMSetRenderTargets(
                rtvs.len() as u32,
                (!rtvs.is_empty()).then_some(rtvs.as_ptr()),
                false,
                dsv.as_ref().map(std::ptr::from_ref),
            )
        };
    }

    /// Viewports, scissors, the blend factor and the stencil reference: set
    /// on the host list at once, and kept for [`State::replay`].
    pub(crate) fn set_viewports(&mut self, list: &HostList, viewports: &[D3D12_VIEWPORT]) {
        #[cfg(feature = "diagnostics")]
        if let Some(v) = viewports.first() {
            crate::backend::gpu_spans::viewport(v.Width as u32, v.Height as u32);
        }
        self.viewports.clear();
        self.viewports.extend_from_slice(viewports);
        crate::execution::draw::viewports(list, viewports);
    }

    pub(crate) fn set_scissors(&mut self, list: &HostList, rects: &[RECT]) {
        self.scissors.clear();
        self.scissors.extend_from_slice(rects);
        if scissor_test() {
            // The experiment: every rectangle the title sets is widened to
            // the largest target there is, so nothing is cut by one. What
            // the listing shows (`op_text`) stays the title's own.
            let full = RECT {
                left: 0,
                top: 0,
                right: 16384,
                bottom: 16384,
            };
            crate::execution::draw::scissors(list, &vec![full; rects.len()]);
            return;
        }
        crate::execution::draw::scissors(list, rects);
    }

    pub(crate) fn set_blend_factor(&mut self, list: &HostList, factor: Option<&[f32; 4]>) {
        self.blend = factor.copied();
        crate::execution::draw::blend_factor(list, factor);
    }

    pub(crate) fn set_stencil_ref(&mut self, list: &HostList, reference: u32) {
        self.stencil = Some(reference);
        crate::execution::draw::stencil_ref(list, reference);
    }

    pub(crate) fn set_depth_bounds(&mut self, list: &HostList, min: f32, max: f32) {
        self.depth_bounds = Some([min, max]);
        crate::execution::draw::depth_bounds(list, min, max);
    }

    /// `list` is a new host list that continues the title's list (it was
    /// cut at a wait for a memory value, `crate::d3d12::command_list::cut`): a new host
    /// list has nothing set, so what the title set so far is set again.
    /// The pipeline, the root signatures and the heaps are set by the next
    /// draw or dispatch (`bind`, which is told here that the host list has
    /// none); root arguments, vertex and index buffers are set by every
    /// draw anyway. The dynamic descriptor slots and the tables made in
    /// them stay: the parts before the cut have not run yet, and they are
    /// given back when the title's list is reset.
    /// shortcut: the render target views are made again from the handles'
    /// descriptors as they are now, not as they were when the title set
    /// the targets (a descriptor rewritten in between names another view).
    pub(crate) fn replay(&mut self, list: &HostList) {
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [0; 2];
        self.host_heaps_set = false;
        if self.topology_set {
            self.set_topology(list, self.topology);
        }
        self.host_targets = None;
        if self.targets_set {
            let handles = std::mem::take(&mut self.target_handles);
            self.set_targets(list, &handles, self.depth_handle);
        } else {
            // SAFETY: an open list; clears any DSV left bound by an inline
            // depth-plane pass before a target-less graphics draw.
            unsafe { list.list.OMSetRenderTargets(0, None, false, None) };
        }
        if !self.viewports.is_empty() {
            crate::execution::draw::viewports(list, &self.viewports);
        }
        if !self.scissors.is_empty() {
            let rects = std::mem::take(&mut self.scissors);
            self.set_scissors(list, &rects);
        }
        if let Some(factor) = self.blend {
            crate::execution::draw::blend_factor(list, Some(&factor));
        }
        if let Some(reference) = self.stencil {
            crate::execution::draw::stencil_ref(list, reference);
        }
        if let Some([min, max]) = self.depth_bounds {
            crate::execution::draw::depth_bounds(list, min, max);
        }
    }
}
