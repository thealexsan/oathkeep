use std::sync::Arc;

use oathkeep_shader::emit::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;

use super::{Shader, Stage, bounded_root_srvs};

pub(super) fn build_root_signature(
    device: &ID3D12Device,
    stages: &[(&Arc<Shader>, D3D12_SHADER_VISIBILITY)],
    compute: bool,
    aniso_point_mip_supported: bool,
) -> Option<(ID3D12RootSignature, Vec<u8>, Vec<Stage>)> {
    // The root signature the translated shaders' bindings define.
    let mut params: Vec<D3D12_ROOT_PARAMETER1> = Vec::new();
    // The ranges table parameters point at, alive until the root
    // signature is made.
    let mut ranges: Vec<Box<D3D12_DESCRIPTOR_RANGE1>> = Vec::new();
    let mut samplers = Vec::new();
    let mut bound = Vec::new();
    for &(shader, visibility) in stages {
        let t = &shader.translated()?.host;
        let mut stage = Stage {
            shader: shader.clone(),
            user_data: None,
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
        };
        let dwords = t.user_data_dwords();
        if dwords > 0 {
            stage.user_data = Some((params.len() as u32, dwords));
            params.push(D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: USER_DATA_SPACE,
                        Num32BitValues: dwords,
                    },
                },
                ShaderVisibility: visibility,
            });
        }
        for (slots, kind, out) in [
            (
                &t.root_cbvs,
                D3D12_ROOT_PARAMETER_TYPE_CBV,
                &mut stage.root_cbvs,
            ),
            (
                &t.root_srvs,
                D3D12_ROOT_PARAMETER_TYPE_SRV,
                &mut stage.root_srvs,
            ),
            (
                &t.root_uavs,
                D3D12_ROOT_PARAMETER_TYPE_UAV,
                &mut stage.root_uavs,
            ),
        ] {
            for &slot in slots {
                let uav = kind == D3D12_ROOT_PARAMETER_TYPE_UAV;
                // Ours, not the title's, stay root descriptors: the
                // table of strides, the directory of title memory by
                // address and its fault notes.
                let ours = if uav {
                    slot == oathkeep_shader::emit::SLOT_FAULTS
                } else {
                    slot == oathkeep_shader::emit::SLOT_STRIDES
                        || slot == oathkeep_shader::emit::SLOT_TABLE_STRIDES
                        || slot == oathkeep_shader::emit::SLOT_MEMORY
                };
                if (uav || kind == D3D12_ROOT_PARAMETER_TYPE_SRV) && !ours && bounded_root_srvs() {
                    // A one-view table: the view's size bounds the
                    // reads (and a root UAV's writes), which a root
                    // descriptor leaves unbounded.
                    // (Our own table of strides stays a descriptor.)
                    ranges.push(Box::new(D3D12_DESCRIPTOR_RANGE1 {
                        RangeType: if uav {
                            D3D12_DESCRIPTOR_RANGE_TYPE_UAV
                        } else {
                            D3D12_DESCRIPTOR_RANGE_TYPE_SRV
                        },
                        NumDescriptors: 1,
                        BaseShaderRegister: u32::from(slot),
                        RegisterSpace: ROOT_CBV_SPACE,
                        Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                            | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                        OffsetInDescriptorsFromTableStart: 0,
                    }));
                    let range: *const D3D12_DESCRIPTOR_RANGE1 = &**ranges.last()?;
                    out.push((slot, params.len() as u32));
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                                NumDescriptorRanges: 1,
                                pDescriptorRanges: range,
                            },
                        },
                        ShaderVisibility: visibility,
                    });
                    continue;
                }
                out.push((slot, params.len() as u32));
                params.push(D3D12_ROOT_PARAMETER1 {
                    ParameterType: kind,
                    Anonymous: D3D12_ROOT_PARAMETER1_0 {
                        Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                            ShaderRegister: u32::from(slot),
                            RegisterSpace: ROOT_CBV_SPACE,
                            Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                        },
                    },
                    ShaderVisibility: visibility,
                });
            }
        }
        for (n, s) in t.static_samplers.iter().enumerate() {
            samplers.push(D3D12_STATIC_SAMPLER_DESC {
                Filter: D3D12_FILTER(s.filter_for_host(aniso_point_mip_supported) as i32),
                AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.address[0] as i32),
                AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.address[1] as i32),
                AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.address[2] as i32),
                MipLODBias: s.mip_lod_bias,
                MaxAnisotropy: s.max_anisotropy,
                ComparisonFunc: D3D12_COMPARISON_FUNC(s.comparison as i32),
                BorderColor: D3D12_STATIC_BORDER_COLOR(s.border as i32),
                MinLOD: s.min_lod,
                MaxLOD: s.max_lod,
                ShaderRegister: n as u32,
                RegisterSpace: SAMPLER_SPACE,
                ShaderVisibility: visibility,
            });
        }
        bound.push(stage);
    }
    match root_signature(device, &params, &samplers, compute) {
        Ok((root, root_blob)) => Some((root, root_blob, bound)),
        Err(e) => {
            tracing::warn!(error = %e, "host root signature refused");
            None
        }
    }
}

pub(super) fn root_signature(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
) -> windows::core::Result<(ID3D12RootSignature, Vec<u8>)> {
    let input = if compute {
        D3D12_ROOT_SIGNATURE_FLAG_NONE
    } else {
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: input
                    | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                    | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
            },
        },
    };
    // SAFETY: the description points at live arrays; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut error: Option<ID3DBlob> = None;
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) {
            if let Some(error) = error {
                let text = std::slice::from_raw_parts(
                    error.GetBufferPointer().cast::<u8>(),
                    error.GetBufferSize(),
                );
                tracing::warn!("root signature: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        Ok((device.CreateRootSignature(0, bytes)?, bytes.to_vec()))
    }
}
