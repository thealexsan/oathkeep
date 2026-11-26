//! Resource readiness transitions and `BakingGuard`.

use oathkeep_shader::ResourceType;

use crate::descriptors::View;
use crate::backend::HostList;
use crate::d3d12::object::This;

use super::{DEPTH_READ, SHADER_RESOURCE, STORES, TESTED_DEPTH, write_skip};

/// Gets what a shader reads or writes through a view ready: buffer data
/// copied, textures moved to the state they need.
pub(crate) fn view_kind(view: &View) -> Option<ResourceType> {
    match view {
        View::Shader(..) | View::Placed(false, ..) => Some(ResourceType::Srv),
        View::Unordered(..) | View::Placed(true, ..) => Some(ResourceType::Uav),
        _ => None,
    }
}

/// Temporarily enables `crate::textures::texture::BAKING` (`true`) for the current scope
/// so CPU-written occlusion textures record `CopyTextureRegion` in the same
/// frame instead of waiting until the following frame after submission detiling.
pub(super) struct BakingGuard(bool);

impl BakingGuard {
    pub(super) fn enter() -> Self {
        let prev = crate::textures::texture::BAKING.get();
        crate::textures::texture::BAKING.set(true);
        Self(prev)
    }
}

impl Drop for BakingGuard {
    fn drop(&mut self) {
        crate::textures::texture::BAKING.set(self.0);
    }
}

pub(super) fn readable(list: &HostList, kind: ResourceType, view: &View) {
    const UNORDERED_ACCESS: u32 = 0x8;
    let placed = crate::descriptors::host_heap::placed_texture(view);
    let view = placed.as_ref().unwrap_or(view);
    // Buffer views read title memory: their range is copied at submission;
    // what shaders write through unordered access goes back to it after.
    if let Some((shadow, address, len)) = crate::descriptors::host_heap::buffer_copy(view) {
        shadow.refresh(list, address, len);
        if kind == ResourceType::Uav {
            // A shader that cannot store to buffers leaves the range as
            // the title has it: nothing to find afterwards.
            if STORES.get() || !write_skip() {
                #[cfg(feature = "diagnostics")]
                crate::backend::wide_write(address, len, 0);
                shadow.gpu_writes(list, address, len);
            } else {
                #[cfg(feature = "diagnostics")]
                {
                    crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::GpuWritesLeftOut, 1);
                    crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::GpuWritesLeftOutBytes, len);
                }
            }
        }
    }
    match (view, kind) {
        (View::Shader(resource, _), ResourceType::Srv) => {
            // A view the title placed, in another format, in the memory of
            // a texture the host writes: that texture's texels, copied on
            // the host (title memory does not have them).
            let aliased = crate::d3d12::resource::alias_of(*resource as This)
                .is_some_and(|source| crate::textures::texture::alias(list, *resource as This, source));
            if !aliased
                && let Some(t) = crate::d3d12::resource::host_copy(*resource as This)
                && t.flags() & 0x7 == 0
            {
                // (A texture the title writes: no target, depth or unordered
                // access, `crate::textures::texture::title_written`.)
                // Of a texture the title placed, the levels the view shows.
                let first = match (&placed, view) {
                    (Some(_), View::Shader(_, Some(desc))) => crate::d3d12::resource::first_mip(desc),
                    _ => 0,
                };
                // Small CPU-written R32_FLOAT (`DXGI_FORMAT_R32_FLOAT == 41`,
                // `<= 256x256`) depth pyramids feed same-frame GPU Hi-Z culling
                // and must land in VRAM before the dispatch in `list` runs.
                let sync_cpu_occlusion = crate::textures::texture::PICKING.get().is_none()
                    && crate::d3d12::resource::texture_kind(*resource as This)
                        .is_some_and(|(w, h, fmt)| fmt == 41 && w <= 256 && h <= 256);
                let _guard = sync_cpu_occlusion.then(BakingGuard::enter);
                crate::textures::texture::upload_from(list, *resource as This, first);
            }
            // A depth texture shaders also write: what they wrote, first.
            list.twin_use(*resource as This, false, false);
            // The depth target the draw only tests is read in both ways at
            // once: both read states, which its preparation asks for too.
            let (tested, _) = TESTED_DEPTH.get();
            let state = if tested != 0 && tested == *resource {
                TESTED_DEPTH.set((tested, true));
                DEPTH_READ | SHADER_RESOURCE
            } else {
                SHADER_RESOURCE
            };
            crate::d3d12::command_list::ensure_state(list, *resource as This, state);
            #[cfg(feature = "diagnostics")]
            if tracing::enabled!(tracing::Level::DEBUG)
                && let Some(t) = crate::d3d12::resource::host_resource(*resource as This)
            {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                if d.Flags.0 & 7 == 0 {
                    tracing::debug!(
                        resource = format_args!("{resource:#x}"),
                        ?d,
                        "a shader reads a texture the title fills"
                    );
                }
            }
        }
        (View::Unordered(resource, _, _), ResourceType::Uav) => {
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::capture::note_target(*resource as This);
            // Some compute-written textures are also read by the title CPU.
            list.texture_written(*resource as This);
            // Unordered access to a depth texture is its colour twin's.
            if !list.twin_use(*resource as This, true, true) {
                crate::d3d12::command_list::ensure_state(list, *resource as This, UNORDERED_ACCESS);
            }
        }
        _ => {}
    }
}
