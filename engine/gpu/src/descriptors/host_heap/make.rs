use std::sync::Mutex;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use super::{View, host};
use super::buffer_views::{buffer_srv, buffer_uav, buffer_view_ok, twin_window};
use super::diag::{off_elements_for, twinned, typed_twin_needs_exact, view_bytes, view_home};
use super::kin::placed_texture;
use super::null_views::{null_buffer_view, null_for, null_view};
use super::staged::{Kept, srv_desc, staged, uav_desc};

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::d3d12::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        if unordered {
            crate::d3d12::resource::DEPTH_UNORDERED[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::d3d12::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    // A view of a target whose format the host refuses, in the title's
    // format: in the stand-in the host copy has.
    if view != own && crate::d3d12::resource::target_format(view) == own {
        if let Some(format) = format {
            *format = d.Format;
        }
        return true;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

/// The title's unordered access view of a depth texture, as a view of the
/// texture's colour twin `twin` ([`crate::d3d12::resource::Twin`]): the same mip
/// and slices, in the colour format of the twin's family for a depth or
/// typeless format (R16_UNORM for 16-bit depth, R32_FLOAT for 32-bit; the
/// bits are the same). Without a description: mip 0 of every slice.
/// The twin of a stencil plane (R8) stands for the views of that plane
/// only, which become R8_UINT views of its one plane; `None` for a view of
/// the depth plane, which the twin does not stand for. The depth plane has
/// a twin of its own, viewed as a depth texture's without stencil: a view
/// in the format of the depth plane of a depth-stencil format (21, 46)
/// takes the twin's colour format too.
fn twin_uav(
    twin: &ID3D12Resource,
    desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
) -> Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> {
    // SAFETY: a live resource.
    let d = unsafe { twin.GetDesc() };
    let mut desc = desc.cloned().unwrap_or_else(|| {
        let mut all = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        if d.DepthOrArraySize > 1 {
            all.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            all.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: u32::from(d.DepthOrArraySize),
                ..Default::default()
            };
        }
        all
    });
    // A stencil plane's twin (R8 typeless, 60): the title's view is of the
    // stencil plane if it names plane 1 or a stencil view format (22, 47).
    if d.Format.0 == 60 {
        let mut plane = 0;
        match desc.ViewDimension {
            D3D12_UAV_DIMENSION_TEXTURE2D => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2D };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2D = view;
            }
            D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2DArray };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2DArray = view;
            }
            _ => {}
        }
        if plane != 1 && !matches!(desc.Format.0, 22 | 47) {
            return None;
        }
        desc.Format.0 = 62;
        return Some(desc);
    }
    // None, typeless and depth formats (R16: 0, 53, 55; R32: 0, 39, 40),
    // and those of a depth-stencil format's depth plane (19 to 21, 44 to
    // 46), for the twin of such a plane.
    desc.Format.0 = match (desc.Format.0, d.Format.0) {
        (0 | 53 | 55 | 19..=21 | 44..=46, 53) => 56,
        (0 | 39 | 40 | 19..=21 | 44..=46, 39) => 41,
        (other, _) => other,
    };
    Some(desc)
}

/// The bytes of the host's constant buffer view for a title's of `size`:
/// whole 256s, and at most the 64 KiB a shader can index. The host removes
/// the device for a larger view, and the title's heap holds such sizes in
/// slots it does not use.
pub(super) fn constant_bytes(size: u32) -> u32 {
    const LARGEST: u32 = 64 << 10;
    size.min(LARGEST).next_multiple_of(256)
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
pub(super) fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    make_twinned(view, at, None)
}

/// [`make`]; `twin`: where the raw twin of a buffer view goes, plus whether
/// that handle lies in a CPU-only staging heap (`move_raw` safe)
/// ([`raw_twins`]; a constant buffer's is the same view again, other views
/// have none).
pub(super) fn make_twinned(
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<(D3D12_CPU_DESCRIPTOR_HANDLE, bool)>,
) -> Option<Kept> {
    make_twinned_with_copy(view, at, twin).map(|(kept, _)| kept)
}

pub(super) fn make_twinned_with_copy(
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<(D3D12_CPU_DESCRIPTOR_HANDLE, bool)>,
) -> Option<(Kept, Option<(std::sync::Arc<crate::memory::shadow::Shadow>, u64, u64)>)> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            let need = u64::from(size.min(64 << 10));
            let mut shadow = crate::memory::shadow::find(address, need)?;
            if !shadow.can_bind_cbv(address, size) {
                if need == 0 {
                    return None;
                }
                if let Some(aligned) = crate::memory::shadow::find_aligned_cbv(address, need)
                    && aligned.can_bind_cbv(address, size)
                {
                    shadow = aligned;
                } else {
                    once!(
                        address = format_args!("{address:#x}"),
                        size,
                        "constant buffer not 256-byte aligned on the host"
                    );
                    return None;
                }
            }
            let gpu = shadow.gpu(address);
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: constant_bytes(size),
            };
            // SAFETY: slots of our heap; the range lies in the host copy.
            unsafe {
                device.CreateConstantBufferView(Some(&desc), at);
                if let Some((twin, _)) = twin {
                    device.CreateConstantBufferView(Some(&desc), twin);
                }
            }
            let kept = Some(shadow.buffer.clone());
            let copy = (need > 0).then_some((shadow, address, need));
            Some((kept, copy))
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::d3d12::object::This;
            if let Some(copy) = crate::d3d12::resource::host_copy(resource) {
                let texture = &copy.texture;
                // The title's views of textures with a stencil plane, as it
                // describes them (format, dimension, component mapping, a
                // pad, then first level, levels, plane): each distinct one
                // once. They say how its shaders read the stencil plane
                // (the lighting takes each pixel's material class and its
                // "inside a room" bit from it).
                #[cfg(feature = "diagnostics")]
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() }.Format.0;
                    if matches!(own, 19..=22 | 44..=47) {
                        static SEEN: Mutex<Vec<(i32, Option<[u32; 10]>)>> = Mutex::new(Vec::new());
                        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                        if seen.len() < 64 && !seen.contains(&(own, *desc)) {
                            seen.push((own, *desc));
                            tracing::info!(
                                ?resource,
                                texture_format = own,
                                view = ?desc,
                                "shader view of a depth-stencil texture"
                            );
                        }
                    }
                }
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                // Levels on demand (`crate::memory::tiles`): a texture made level by
                // level is shown from the first level that is the title's
                // and has memory, if the title's view starts before it (a
                // level without memory reads zeros: a black texture). The
                // title's own view mostly starts there already.
                if let Some(first) = crate::d3d12::resource::first_shown(resource)
                    && first > 0
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() };
                    // The texture's own view (the title gave none): all of it.
                    let whole = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: own.Format,
                        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                        Shader4ComponentMapping: 5768,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D12_TEX2D_SRV {
                                MostDetailedMip: 0,
                                MipLevels: u32::MAX,
                                PlaneSlice: 0,
                                ResourceMinLODClamp: 0.0,
                            },
                        },
                    };
                    crate::memory::tiles::show_from(desc.get_or_insert(whole), first, u32::from(own.MipLevels));
                }
                if copy.base > 0
                    && let Some(desc) = &mut desc
                {
                    from_base(desc, copy.base);
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some((Some(texture.clone()), None))
            } else if let (Some((data, len)), Some(desc)) =
                (crate::d3d12::resource::buffer_bytes(resource), desc)
            {
                buffer_srv(data as u64, len, srv_desc(desc), at, twin)
            } else {
                None
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::d3d12::object::This;
            let Some(texture) = crate::d3d12::resource::host_resource(resource) else {
                return match (crate::d3d12::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at, twin)
                    }
                    _ => None,
                };
            };
            // The first such views of depth textures are logged as the
            // title describes them (format, dimension, then mip, slices and
            // plane): they say which planes shaders write, and as what.
            if crate::d3d12::resource::depth_and_unordered(resource) {
                static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 512 {
                    tracing::info!(
                        ?resource,
                        view = ?desc,
                        "unordered access view of a depth texture"
                    );
                }
            }
            let mut desc = desc.as_ref().map(uav_desc);
            // A depth texture's unordered access is its colour twin's: the
            // host gives a depth texture none.
            let twin = crate::d3d12::resource::twin_resource(resource)
                .and_then(|twin| Some((twin, twin_uav(twin, desc.as_ref())?)))
                // A view of the depth plane of a texture with a stencil
                // plane: of that plane's own twin.
                .or_else(|| {
                    let twin = crate::d3d12::resource::twin_plane_resource(resource)?;
                    Some((twin, twin_uav(twin, desc.as_ref())?))
                });
            let texture = match twin {
                Some((twin, view)) => {
                    desc = Some(view);
                    twin
                }
                None => texture,
            };
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some((Some(texture.clone()), None))
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some((None, None))
        }
        View::ConsoleSampler(words) => {
            let desc = crate::d3d12::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some((None, None))
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make(&view, at).map(|k| (k, None));
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                let at = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                if *unordered || at != 0 {
                    // shortcut: unordered access to a texture the title placed
                    // itself is not mirrored; give it a resource as shader
                    // views of such textures get when a dispatch writes one.
                    // Each texture is said once (the first 64), with how many times
                    // such a view was asked for so far: what shaders write through
                    // them is lost.
                    static SAID: Mutex<std::collections::BTreeSet<u64>> = Mutex::new(std::collections::BTreeSet::new());
                    static TIMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let times = TIMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
                    if (said.len() < 64 && said.insert(at)) || times.is_power_of_two() {
                        tracing::warn!(
                            address = format_args!("{at:#x}"),
                            unordered = *unordered,
                            times,
                            "placed texture view not mirrored (null): resource {resource:?}, view {:?}",
                            &desc[..10]
                        );
                    }
                }
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            // An empty title buffer: nothing to view, and no host copy to
            // look for (#70: such views were in flight at device removals).
            if len == 0 {
                return None;
            }
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at, twin)
            } else {
                buffer_srv(address, len, srv_desc(words), at, twin)
            }
        }
    }
}

/// The view `desc` of a texture, of a host copy of its levels from `base`
/// on (`crate::d3d12::resource::held_only`). What it shows of larger ones, if any,
/// is left out.
pub(super) fn from_base(desc: &mut D3D12_SHADER_RESOURCE_VIEW_DESC, base: u32) {
    if desc.ViewDimension != D3D12_SRV_DIMENSION_TEXTURE2D {
        return;
    }
    // SAFETY: the member its dimension says.
    let t = unsafe { &mut desc.Anonymous.Texture2D };
    let lost = base.saturating_sub(t.MostDetailedMip);
    t.MostDetailedMip = t.MostDetailedMip.saturating_sub(base);
    if t.MipLevels != u32::MAX {
        t.MipLevels = t.MipLevels.saturating_sub(lost).max(1);
    }
    t.ResourceMinLODClamp = (t.ResourceMinLODClamp - base as f32).max(0.0);
}

