//! Indirect-light stand-in hook after matching compute dispatches.

use oathkeep_shader::{ResourceType, RootType};
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DIMENSION_TEXTURE3D;

use crate::descriptor::View;
use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

use super::state::{Root, State};

impl State {
    /// The stand-in for the indirect light after a dispatch of `p`
    /// (`crate::ambient`). Whether it recorded anything.
    pub(super) fn ambient(
        &self,
        list: &HostList,
        p: &HostPipeline,
        role: crate::ambient::Role,
    ) -> bool {
        use crate::ambient::{AIR, DEPTH, LIGHT, RADIANCE, Role, Views};
        let Some(t) = p.stages.first().map(|s| s.translated()) else {
            return false;
        };
        let Some(heap) = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten()
        else {
            return false;
        };
        let entry = |kind: ResourceType, (slot, offset): (u8, u32)| {
            t.used.iter().find(|u| {
                u.kind == kind
                    && u.root == RootType::Table
                    && (u.slot, u.table_offset_dwords) == (slot, offset)
            })?;
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return None;
            };
            let resource = match crate::descriptor::view(table + u64::from(offset) * 4)? {
                View::Shader(r, _) | View::Unordered(r, ..) => r,
                _ => return None,
            };
            let start = match self.ambient_tables.iter().find(|m| m.0 == slot) {
                Some(m) => m.1,
                None => self.user_value(true, slot, RootType::Table, 0),
            };
            let host = crate::resource::host_resource(resource as This)?;
            // SAFETY: a live resource.
            let desc = unsafe { host.GetDesc() };
            Some((desc, heap.gpu_handle((start + offset * 4) / 32)))
        };
        let at = match role {
            Role::Resolve => LIGHT,
            Role::Radiance => RADIANCE,
            Role::Air => AIR,
        };
        let image = entry(ResourceType::Uav, at);
        let depth = match role {
            Role::Resolve => entry(ResourceType::Srv, DEPTH),
            Role::Radiance | Role::Air => image,
        };
        let (Some((of, image)), Some((seen, depth))) = (image, depth) else {
            once!(?role, "no stand-in for the indirect light: other tables");
            return false;
        };
        // (R32_UINT holding R9G9B9E5; R11G11B10_FLOAT.)
        let format = if role == Role::Resolve { 42 } else { 26 };
        let volume = of.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        if of.Format.0 != format
            || (seen.Width, seen.Height) != (of.Width, of.Height)
            || volume != (role == Role::Air)
        {
            once!(
                ?role,
                format = of.Format.0,
                "no stand-in for the indirect light: another image"
            );
            return false;
        }
        crate::ambient::after(
            list,
            role,
            &Views {
                image,
                depth,
                size: [of.Width as u32, of.Height],
                slices: u32::from(of.DepthOrArraySize),
            },
        )
    }
}
