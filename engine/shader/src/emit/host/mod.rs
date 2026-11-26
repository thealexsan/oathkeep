//! Host D3D12 checks for emitted shaders: pipeline creation and, for
//! tests, drawing a full-screen triangle and reading the target back.


use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use super::HostShader;
use super::container::SigElement;
use crate::dxbc::{Container, FourCC};

pub struct Host {
    pub device: ID3D12Device,
}

/// What the stage declares that a pipeline must match.
pub(super) fn sig(dxil: &[u8], fourcc: FourCC) -> Result<Vec<SigElement>, String> {
    let c = Container::parse(dxil).map_err(|e| e.to_string())?;
    match c.part(fourcc) {
        Some(p) => super::signature(p.data).map_err(|e| e.to_string()),
        None => Ok(Vec::new()),
    }
}

pub(super) fn format(e: &SigElement) -> DXGI_FORMAT {
    let i = usize::from(e.cols.clamp(1, 4)) - 1;
    match e.comp {
        9 => [
            DXGI_FORMAT_R32_FLOAT,
            DXGI_FORMAT_R32G32_FLOAT,
            DXGI_FORMAT_R32G32B32_FLOAT,
            DXGI_FORMAT_R32G32B32A32_FLOAT,
        ][i],
        4 => [
            DXGI_FORMAT_R32_SINT,
            DXGI_FORMAT_R32G32_SINT,
            DXGI_FORMAT_R32G32B32_SINT,
            DXGI_FORMAT_R32G32B32A32_SINT,
        ][i],
        _ => [
            DXGI_FORMAT_R32_UINT,
            DXGI_FORMAT_R32G32_UINT,
            DXGI_FORMAT_R32G32B32_UINT,
            DXGI_FORMAT_R32G32B32A32_UINT,
        ][i],
    }
}

pub(super) fn target_format(e: &SigElement) -> DXGI_FORMAT {
    match e.comp {
        9 => DXGI_FORMAT_R32G32B32A32_FLOAT,
        4 => DXGI_FORMAT_R32G32B32A32_SINT,
        _ => DXGI_FORMAT_R32G32B32A32_UINT,
    }
}

pub(super) fn err(e: windows::core::Error) -> String {
    e.message().to_string()
}

impl Host {
    /// The default adapter, or WARP with `warp` (or `OATHKEEP_WARP=1`).
    pub fn new(warp: bool) -> Result<Self, String> {
        let warp = warp || std::env::var("OATHKEEP_WARP").is_ok_and(|v| v == "1");
        // SAFETY: plain D3D12/DXGI calls with valid out pointers.
        unsafe {
            // `OATHKEEP_D3D12_DEBUG`: the debug layer, whose messages a
            // refused pipeline's error then carries.
            if std::env::var_os("OATHKEEP_D3D12_DEBUG").is_some() {
                let mut debug: Option<ID3D12Debug> = None;
                if D3D12GetDebugInterface(&mut debug).is_ok()
                    && let Some(debug) = debug
                {
                    debug.EnableDebugLayer();
                }
            }
            let mut device: Option<ID3D12Device> = None;
            if warp {
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(err)?;
                let adapter: IDXGIAdapter = factory.EnumWarpAdapter().map_err(err)?;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).map_err(err)?;
            } else {
                D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).map_err(err)?;
                // A machine without a graphics card gives the software
                // adapter as its default device (a hosted test runner,
                // whose first adapter is a display stub that cannot make a
                // device): that is no hardware device, and the tests that
                // ask for one skip it, as they do where there is none.
                if let Some(made) = &device {
                    let factory: IDXGIFactory4 =
                        CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(err)?;
                    if let Ok(adapter) = factory.EnumAdapterByLuid::<IDXGIAdapter1>(made.GetAdapterLuid())
                        && let Ok(desc) = adapter.GetDesc1()
                        && (desc.Flags & 2 != 0 || (desc.VendorId == 0x1414 && desc.DeviceId == 0x8c))
                    {
                        return Err("the default device is the software adapter".to_owned());
                    }
                }
            }
            let device = device.ok_or("no device")?;
            let mut sm = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            device
                .CheckFeatureSupport(
                    D3D12_FEATURE_SHADER_MODEL,
                    (&raw mut sm).cast(),
                    size_of_val(&sm) as u32,
                )
                .map_err(err)?;
            if sm.HighestShaderModel.0 < D3D_SHADER_MODEL_6_6.0 {
                return Err(format!(
                    "the device supports SM {:#x} only",
                    sm.HighestShaderModel.0
                ));
            }
            Ok(Host { device })
        }
    }

    /// The root signature translated shaders expect: user data per stage
    /// at `b0`, immediate samplers per stage at `s<n>`, heaps directly
    /// indexed.
    pub fn root_signature(
        &self,
        vs: &HostShader,
        ps: &HostShader,
    ) -> Result<ID3D12RootSignature, String> {
        self.root_signature_for(&[
            (vs, D3D12_SHADER_VISIBILITY_VERTEX),
            (ps, D3D12_SHADER_VISIBILITY_PIXEL),
        ])
    }

    /// The root signature for these stages: user data of stage k is root
    /// parameter k, root views follow.
    pub fn root_signature_for(
        &self,
        stages: &[(&HostShader, D3D12_SHADER_VISIBILITY)],
    ) -> Result<ID3D12RootSignature, String> {
        let mut params: Vec<D3D12_ROOT_PARAMETER1> = stages
            .iter()
            .map(|&(s, vis)| D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: super::USER_DATA_SPACE,
                        Num32BitValues: s.user_data_dwords().max(1),
                    },
                },
                ShaderVisibility: vis,
            })
            .collect();
        // By slot, then kind: 0 CBV, 1 SRV, 2 UAV.
        let mut views: Vec<(u8, u8)> = stages
            .iter()
            .flat_map(|(s, _)| {
                let cbvs = s.root_cbvs.iter().map(|&r| (r, 0));
                let srvs = s.root_srvs.iter().map(|&r| (r, 1));
                cbvs.chain(srvs).chain(s.root_uavs.iter().map(|&r| (r, 2)))
            })
            .collect();
        views.sort_unstable();
        views.dedup();
        for (slot, kind) in views {
            params.push(D3D12_ROOT_PARAMETER1 {
                ParameterType: match kind {
                    1 => D3D12_ROOT_PARAMETER_TYPE_SRV,
                    2 => D3D12_ROOT_PARAMETER_TYPE_UAV,
                    _ => D3D12_ROOT_PARAMETER_TYPE_CBV,
                },
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                        ShaderRegister: u32::from(slot),
                        RegisterSpace: super::ROOT_CBV_SPACE,
                        Flags: D3D12_ROOT_DESCRIPTOR_FLAG_NONE,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            });
        }
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
        // SAFETY: the feature query writes the structure of the given size.
        // Older runtimes may not recognize OPTIONS19; that means no support.
        let aniso_point_mip_supported = unsafe {
            self.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS19,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        }
        .is_ok()
            && options.AnisoFilterWithPointMipSupported.as_bool();
        let mut samplers = Vec::new();
        for &(s, vis) in stages {
            for (n, d) in s.static_samplers.iter().enumerate() {
                let [u, v, w] = d.address.map(|a| D3D12_TEXTURE_ADDRESS_MODE(a as i32));
                samplers.push(D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER(d.filter_for_host(aniso_point_mip_supported) as i32),
                    AddressU: u,
                    AddressV: v,
                    AddressW: w,
                    MipLODBias: d.mip_lod_bias,
                    MaxAnisotropy: d.max_anisotropy,
                    ComparisonFunc: D3D12_COMPARISON_FUNC(d.comparison as i32),
                    BorderColor: D3D12_STATIC_BORDER_COLOR(d.border as i32),
                    MinLOD: d.min_lod,
                    MaxLOD: d.max_lod,
                    ShaderRegister: n as u32,
                    RegisterSpace: super::SAMPLER_SPACE,
                    ShaderVisibility: vis,
                });
            }
        }
        let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    NumParameters: params.len() as u32,
                    pParameters: params.as_ptr(),
                    NumStaticSamplers: samplers.len() as u32,
                    pStaticSamplers: samplers.as_ptr(),
                    Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
                        | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                        | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
                },
            },
        };
        // SAFETY: the description points at live arrays.
        unsafe {
            let mut blob = None;
            let mut error = None;
            D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error))
                .map_err(err)?;
            let blob: ID3DBlob = blob.ok_or("no root signature blob")?;
            let bytes = std::slice::from_raw_parts(
                blob.GetBufferPointer().cast::<u8>(),
                blob.GetBufferSize(),
            );
            self.device.CreateRootSignature(0, bytes).map_err(err)
        }
    }

    /// A compute pipeline; user data is root parameter 0.
    pub fn compute_pipeline(
        &self,
        cs: &HostShader,
    ) -> Result<(ID3D12RootSignature, ID3D12PipelineState), String> {
        let root = self.root_signature_for(&[(cs, D3D12_SHADER_VISIBILITY_ALL)])?;
        let pso = self.compute_pipeline_with(&root, &cs.dxil)?;
        Ok((root, pso))
    }

    pub fn compute_pipeline_with(
        &self,
        root: &ID3D12RootSignature,
        cs: &[u8],
    ) -> Result<ID3D12PipelineState, String> {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the descriptor never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(root) },
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: cs.as_ptr().cast(),
                BytecodeLength: cs.len(),
            },
            ..Default::default()
        };
        // SAFETY: the description points at live data.
        unsafe { self.device.CreateComputePipelineState(&desc) }.map_err(|e| self.explained(e))
    }

    /// The error with the debug layer's messages since the last call, if
    /// the layer is on (`OATHKEEP_D3D12_DEBUG`).
    pub(super) fn explained(&self, e: windows::core::Error) -> String {
        let mut text = err(e);
        let Ok(queue) = self.device.cast::<ID3D12InfoQueue>() else {
            return text;
        };
        // SAFETY: each message is read into a buffer of the size the queue
        // asks for, aligned for the message header.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut len = 0usize;
                let _ = queue.GetMessage(i, None, &mut len);
                let mut buffer = vec![0u64; len.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut len).is_ok() {
                    let m = &*message;
                    let bytes = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength.saturating_sub(1));
                    text.push_str(" | ");
                    text.push_str(&String::from_utf8_lossy(bytes));
                }
            }
            queue.ClearStoredMessages();
        }
        text
    }
}

mod pipeline;
#[cfg(test)]
mod dispatch;
#[cfg(test)]
mod draw;
#[cfg(test)]
mod views;

pub use pipeline::{stub_ps, stub_vs};
#[cfg(test)]
pub use draw::{Draw, Root};
#[cfg(test)]
pub use views::{Dispatch, Stride, View};
