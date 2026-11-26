use std::ffi::CString;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR};

use super::super::HostShader;
use super::super::container::SigElement;
use crate::dxbc::FourCC;

use super::{Host, err, format, sig, target_format};

impl Host {
    pub fn pipeline(
        &self,
        vs: &HostShader,
        ps: &HostShader,
    ) -> Result<(ID3D12RootSignature, ID3D12PipelineState), String> {
        let root = self.root_signature(vs, ps)?;
        let pso = self.pipeline_with(&root, &vs.dxil, &ps.dxil)?;
        Ok((root, pso))
    }

    /// A graphics pipeline with the given root signature; vertex inputs
    /// packed in register order.
    pub fn pipeline_with(
        &self,
        root: &ID3D12RootSignature,
        vs: &[u8],
        ps: &[u8],
    ) -> Result<ID3D12PipelineState, String> {
        self.pipeline_with_gs(root, vs, None, ps)
    }

    /// As [`Host::pipeline_with`], with a geometry shader between the two
    /// (the rectangle stage, `rect_list_gs`).
    pub fn pipeline_with_gs(
        &self,
        root: &ID3D12RootSignature,
        vs: &[u8],
        gs: Option<&[u8]>,
        ps: &[u8],
    ) -> Result<ID3D12PipelineState, String> {
        let inputs = sig(vs, FourCC::ISG1)?;
        let names: Vec<CString> = inputs
            .iter()
            .map(|e| CString::new(e.name.clone()).unwrap())
            .collect();
        let layout: Vec<D3D12_INPUT_ELEMENT_DESC> = inputs
            .iter()
            .zip(&names)
            .filter(|(e, _)| e.kind == 0)
            .map(|(e, n)| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(n.as_ptr().cast()),
                SemanticIndex: e.index,
                Format: format(e),
                InputSlot: 0,
                AlignedByteOffset: D3D12_APPEND_ALIGNED_ELEMENT,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            })
            .collect();
        let outputs = sig(ps, FourCC::OSG1)?;
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the descriptor never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(root) },
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: vs.as_ptr().cast(),
                BytecodeLength: vs.len(),
            },
            PS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: ps.as_ptr().cast(),
                BytecodeLength: ps.len(),
            },
            GS: gs.map_or(D3D12_SHADER_BYTECODE::default(), |gs| D3D12_SHADER_BYTECODE {
                pShaderBytecode: gs.as_ptr().cast(),
                BytecodeLength: gs.len(),
            }),
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: layout.as_ptr(),
                NumElements: layout.len() as u32,
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        for rt in &mut desc.BlendState.RenderTarget {
            rt.RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        }
        for o in outputs.iter().filter(|o| o.kind == 16) {
            let n = o.row.unwrap_or(0) as usize;
            desc.RTVFormats[n] = target_format(o);
            desc.NumRenderTargets = desc.NumRenderTargets.max(n as u32 + 1);
        }
        if outputs.iter().any(|o| matches!(o.kind, 17..=19)) {
            desc.DSVFormat = DXGI_FORMAT_D32_FLOAT;
            desc.DepthStencilState.DepthEnable = true.into();
            desc.DepthStencilState.DepthWriteMask = D3D12_DEPTH_WRITE_MASK_ALL;
            desc.DepthStencilState.DepthFunc = D3D12_COMPARISON_FUNC_ALWAYS;
        }
        if outputs.iter().any(|o| o.kind == 20) {
            desc.DSVFormat = DXGI_FORMAT_D32_FLOAT_S8X24_UINT;
            desc.DepthStencilState.StencilEnable = true.into();
        }
        // SAFETY: the description points at live data.
        let pso = unsafe { self.device.CreateGraphicsPipelineState(&desc) };
        pso.map_err(err)
    }

    /// A mesh pipeline from a mesh and a pixel shader (user data of the mesh
    /// stage is root parameter 0, the pixel stage's 1).
    pub fn mesh_pipeline(
        &self,
        ms: &HostShader,
        ps: &HostShader,
    ) -> Result<(ID3D12RootSignature, ID3D12PipelineState), String> {
        let root = self.root_signature_for(&[
            (ms, D3D12_SHADER_VISIBILITY_MESH),
            (ps, D3D12_SHADER_VISIBILITY_PIXEL),
        ])?;
        let pso = self.mesh_pipeline_with(&root, &ms.dxil, &ps.dxil)?;
        Ok((root, pso))
    }

    /// A mesh pipeline with the given root signature: the pipeline-state
    /// stream form, with the targets [`Host::pipeline_with`] would use.
    pub fn mesh_pipeline_with(
        &self,
        root: &ID3D12RootSignature,
        ms: &[u8],
        ps: &[u8],
    ) -> Result<ID3D12PipelineState, String> {
        /// A stream subobject: its type, then its description, pointer-aligned.
        #[repr(C, align(8))]
        struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);
        #[repr(C)]
        struct Stream {
            root: Sub<*mut std::ffi::c_void>,
            ms: Sub<D3D12_SHADER_BYTECODE>,
            ps: Sub<D3D12_SHADER_BYTECODE>,
            blend: Sub<D3D12_BLEND_DESC>,
            mask: Sub<u32>,
            raster: Sub<D3D12_RASTERIZER_DESC>,
            depth: Sub<D3D12_DEPTH_STENCIL_DESC>,
            targets: Sub<D3D12_RT_FORMAT_ARRAY>,
            dsv: Sub<DXGI_FORMAT>,
            samples: Sub<DXGI_SAMPLE_DESC>,
        }
        let outputs = sig(ps, FourCC::OSG1)?;
        let mut blend = D3D12_BLEND_DESC::default();
        for rt in &mut blend.RenderTarget {
            rt.RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        }
        let mut targets = D3D12_RT_FORMAT_ARRAY::default();
        for o in outputs.iter().filter(|o| o.kind == 16) {
            let n = o.row.unwrap_or(0) as usize;
            targets.RTFormats[n] = target_format(o);
            targets.NumRenderTargets = targets.NumRenderTargets.max(n as u32 + 1);
        }
        let mut depth = D3D12_DEPTH_STENCIL_DESC::default();
        let mut dsv = DXGI_FORMAT_UNKNOWN;
        if outputs.iter().any(|o| matches!(o.kind, 17..=19)) {
            dsv = DXGI_FORMAT_D32_FLOAT;
            depth.DepthEnable = true.into();
            depth.DepthWriteMask = D3D12_DEPTH_WRITE_MASK_ALL;
            depth.DepthFunc = D3D12_COMPARISON_FUNC_ALWAYS;
        }
        if outputs.iter().any(|o| o.kind == 20) {
            dsv = DXGI_FORMAT_D32_FLOAT_S8X24_UINT;
            depth.StencilEnable = true.into();
        }
        let code = |b: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: b.as_ptr().cast(),
            BytecodeLength: b.len(),
        };
        let mut stream = Stream {
            root: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                root.as_raw(),
            ),
            ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, code(ms)),
            ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, code(ps)),
            blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, blend),
            mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, u32::MAX),
            raster: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER,
                D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
            ),
            depth: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, depth),
            targets: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
                targets,
            ),
            dsv: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
                dsv,
            ),
            samples: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC,
                DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
            ),
        };
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: size_of::<Stream>(),
            pPipelineStateSubobjectStream: (&raw mut stream).cast(),
        };
        let device: ID3D12Device2 = self.device.cast().map_err(err)?;
        // SAFETY: the stream points at live data and outlives the call.
        unsafe { device.CreatePipelineState(&desc) }.map_err(err)
    }
}

/// A vertex shader that feeds `ps`'s inputs (zeros) and a position.

pub fn stub_vs(ps: &HostShader) -> Result<HostShader, String> {
    let inputs = sig(&ps.dxil, FourCC::ISG1)?;
    let mut outputs: Vec<SigElement> = inputs
        .into_iter()
        .filter(|e| matches!(e.kind, 0 | 3 | 6 | 7))
        .collect();
    if !outputs.iter().any(|e| e.kind == 3) {
        let row = outputs
            .iter()
            .filter_map(|e| e.row)
            .map(|r| r + 1)
            .max()
            .unwrap_or(0);
        outputs.push(SigElement {
            name: "SV_Position".into(),
            index: 0,
            kind: 3,
            comp: 9,
            interp: 4,
            row: Some(row),
            col: 0,
            cols: 4,
            usage: 0,
            half: false,
        });
    }
    Ok(stub(false, outputs))
}

/// A pixel shader that reads nothing and writes target 0.
pub fn stub_ps() -> HostShader {
    let target = SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 0,
        half: false,
    };
    stub(true, vec![target])
}

fn stub(pixel: bool, outputs: Vec<SigElement>) -> HostShader {
    use super::super::bitcode::{Attr, Term};
    let mut s = super::super::shader::Shader::new(pixel);
    let b = s.entry;
    for (id, e) in outputs.iter().enumerate() {
        for col in 0..e.cols {
            let (suffix, zero) = if e.comp == 9 {
                (".f32", s.m.f32(0))
            } else {
                (".i32", s.m.i32(0))
            };
            let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(col), zero];
            s.op(
                b,
                &format!("storeOutput{suffix}"),
                Attr::NoUnwind,
                s.t.void,
                5,
                &args,
            );
        }
    }
    s.outputs = outputs
        .into_iter()
        .map(|mut e| {
            e.usage = (1 << e.cols) - 1;
            e
        })
        .collect();
    s.m.terminate(b, Term::Ret);
    HostShader {
        stage: if pixel {
            crate::native::HwStage::Ps
        } else {
            crate::native::HwStage::Vs
        },
        dxil: s.finish(),
        user_data: Vec::new(),
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        root_uavs: Vec::new(),
        root_indexed_views: Vec::new(),
    }
}

