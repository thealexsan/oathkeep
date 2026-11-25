use std::ffi::c_void;

use oathkeep_gpu::device;
use oathkeep_gpu::object::{HRESULT, Iid, Object};

use crate::{HleExport, HleModule};

extern "system" fn create_device(
    adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut *const Object,
) -> HRESULT {
    oathkeep_gpu::resource::use_allocator(super::xmem::XMemAlloc, super::xmem::XMemFree);
    oathkeep_gpu::resource::use_page_mapper(super::xmem::map_alias);
    #[cfg(windows)]
    oathkeep_gpu::on_failure(host_gpu_failed);
    device::create(adapter, parameters, iid, out)
}

#[cfg(windows)]
fn host_gpu_failed(what: &str) -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    oathkeep_loader::log_crash(&format!("Oathkeep: host GPU failure: {what}"));
    const DXGI_ERROR_DEVICE_REMOVED: u32 = 0x887A_0005;
    // SAFETY: ends our own process.
    unsafe { TerminateProcess(GetCurrentProcess(), DXGI_ERROR_DEVICE_REMOVED) };
    unreachable!("the process has ended")
}

pub fn warm_shader(container: &[u8]) -> Result<bool, String> {
    #[cfg(windows)]
    {
        oathkeep_gpu::shader_cache::warm(container)
    }
    #[cfg(not(windows))]
    {
        let _ = container;
        Err("no shader cache on this host".into())
    }
}

pub fn use_shader_cache(dir: std::path::PathBuf) {
    #[cfg(windows)]
    {
        oathkeep_gpu::pipeline::use_pipeline_cache(dir.with_file_name("pipelines"));
        oathkeep_gpu::shader_cache::use_shader_cache(dir);
    }
    #[cfg(not(windows))]
    let _ = dir;
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "d3d12x",
        dlls: &["d3d12_xs.dll"],
        exports: vec![
            HleExport {
                name: "D3D12ConfigureVirtualMemoryX",
                address: device::configure_virtual_memory as *const () as usize,
            },
            HleExport {
                name: "D3D12XboxCreateDevice",
                address: create_device as *const () as usize,
            },
            HleExport {
                name: "D3D12SerializeRootSignature",
                address: oathkeep_gpu::root::serialize as *const () as usize,
            },
            HleExport {
                name: "D3D12SerializeVersionedRootSignature",
                address: oathkeep_gpu::root::serialize_versioned as *const () as usize,
            },
            HleExport {
                name: "D3D12CreateRootSignatureDeserializer",
                address: oathkeep_gpu::root::create_deserializer as *const () as usize,
            },
            HleExport {
                name: "IID_ID3D12Device",
                address: (&raw const device::IID) as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HleResolver, HostPolicy};
    use oathkeep_loader::ImportResolver;
    use oathkeep_loader::Resolution;
    use oathkeep_pe::ImportSymbol;

    #[test]
    fn the_interface_id_import_is_bound_to_data() {
        let symbol = ImportSymbol::Name {
            hint: 0,
            name: "IID_ID3D12Device".into(),
        };
        let r = HleResolver::new(HostPolicy::none());
        let Resolution::Found { address, .. } = r.resolve("d3d12_xs.dll", &symbol) else {
            panic!("not bound");
        };
        // SAFETY: the address is our static id.
        assert_eq!(unsafe { *(address as *const [u8; 16]) }, device::IID);
    }
}
