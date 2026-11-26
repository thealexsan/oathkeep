use std::ffi::{CStr, CString};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

/// The console's own primitive topology types.
pub(super) const RECTANGLES: i32 = 19;
pub(super) const QUADS: i32 = 32;

/// A stream's graphics state, owned (the title's stream lives only for the
/// creation call). Subobjects the stream leaves out keep D3D12's defaults.
pub(crate) struct GraphicsState {
    pub(crate) blend: D3D12_BLEND_DESC,
    pub(crate) sample_mask: u32,
    pub(crate) rasterizer: D3D12_RASTERIZER_DESC,
    pub(crate) depth: D3D12_DEPTH_STENCIL_DESC,
    pub(crate) depth_bounds: bool,
    /// Input elements, their names pointing into `names`.
    pub(crate) inputs: Vec<D3D12_INPUT_ELEMENT_DESC>,
    pub(crate) names: Vec<CString>,
    pub(crate) strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE,
    pub(crate) topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    pub(crate) targets: D3D12_RT_FORMAT_ARRAY,
    pub(crate) depth_format: DXGI_FORMAT,
    pub(crate) samples: DXGI_SAMPLE_DESC,
}

impl Default for GraphicsState {
    fn default() -> Self {
        let target = D3D12_RENDER_TARGET_BLEND_DESC {
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let keep = D3D12_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        };
        GraphicsState {
            blend: D3D12_BLEND_DESC {
                RenderTarget: [target; 8],
                ..Default::default()
            },
            sample_mask: u32::MAX,
            rasterizer: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_BACK,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            depth: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                StencilReadMask: 0xff,
                StencilWriteMask: 0xff,
                FrontFace: keep,
                BackFace: keep,
                ..Default::default()
            },
            depth_bounds: false,
            inputs: Vec::new(),
            names: Vec::new(),
            strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
            topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE_UNDEFINED,
            targets: D3D12_RT_FORMAT_ARRAY::default(),
            depth_format: DXGI_FORMAT_UNKNOWN,
            samples: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
        }
    }
}

/// Integer render target formats: D3D12 cannot blend into them.
fn integer_format(f: DXGI_FORMAT) -> bool {
    matches!(
        f,
        DXGI_FORMAT_R32G32B32A32_UINT
            | DXGI_FORMAT_R32G32B32A32_SINT
            | DXGI_FORMAT_R32G32B32_UINT
            | DXGI_FORMAT_R32G32B32_SINT
            | DXGI_FORMAT_R16G16B16A16_UINT
            | DXGI_FORMAT_R16G16B16A16_SINT
            | DXGI_FORMAT_R32G32_UINT
            | DXGI_FORMAT_R32G32_SINT
            | DXGI_FORMAT_R10G10B10A2_UINT
            | DXGI_FORMAT_R8G8B8A8_UINT
            | DXGI_FORMAT_R8G8B8A8_SINT
            | DXGI_FORMAT_R16G16_UINT
            | DXGI_FORMAT_R16G16_SINT
            | DXGI_FORMAT_R32_UINT
            | DXGI_FORMAT_R32_SINT
            | DXGI_FORMAT_R8G8_UINT
            | DXGI_FORMAT_R8G8_SINT
            | DXGI_FORMAT_R16_UINT
            | DXGI_FORMAT_R16_SINT
            | DXGI_FORMAT_R8_UINT
            | DXGI_FORMAT_R8_SINT
    )
}

/// The title's blend state as the host takes it: blending off on integer
/// render targets. The console ignores blending there, and titles leave it
/// on (the world sets it on an `R8_UINT` target), but D3D12 does not allow
/// it: vkd3d-proton refuses such a pipeline with `E_INVALIDARG` ("using
/// integer format is not supported"), and its draws are lost. With one
/// description for every target, each target gets its own copy so the
/// others keep theirs.
pub(super) fn host_blend(blend: &D3D12_BLEND_DESC, targets: &D3D12_RT_FORMAT_ARRAY) -> D3D12_BLEND_DESC {
    let count = (targets.NumRenderTargets as usize).min(8);
    let per_target = blend.IndependentBlendEnable.as_bool();
    let blends = |i: usize| {
        blend.RenderTarget[if per_target { i } else { 0 }]
            .BlendEnable
            .as_bool()
    };
    if !(0..count).any(|i| integer_format(targets.RTFormats[i]) && blends(i)) {
        return *blend;
    }
    let mut host = *blend;
    if !per_target {
        host.IndependentBlendEnable = true.into();
        host.RenderTarget = [blend.RenderTarget[0]; 8];
        // D3D12 forbids IndependentBlendEnable and LogicOpEnable simultaneously.
        for rt in &mut host.RenderTarget {
            rt.LogicOpEnable = false.into();
        }
    }
    for i in 0..count {
        if integer_format(targets.RTFormats[i]) {
            host.RenderTarget[i].BlendEnable = false.into();
        }
    }
    host
}

/// Sanitizes render target format arrays for D3D12 PSO creation: clamps
/// `NumRenderTargets` to `<= 8` and zeroes trailing `RTFormats[count..8]` to
/// `DXGI_FORMAT_UNKNOWN` (D3D12 refuses non-UNKNOWN trailing formats).
pub(super) fn sanitize_targets(targets: &D3D12_RT_FORMAT_ARRAY) -> D3D12_RT_FORMAT_ARRAY {
    let mut out = *targets;
    let count = (out.NumRenderTargets as usize).min(8);
    out.NumRenderTargets = count as u32;
    for fmt in &mut out.RTFormats[count..] {
        *fmt = DXGI_FORMAT_UNKNOWN;
    }
    out
}

/// Sanitizes sample descriptions for D3D12 PSO creation: `Count == 0` is
/// invalid in D3D12 and normalized to single-sample `(1, 0)`.
pub(super) fn sanitize_samples(samples: DXGI_SAMPLE_DESC) -> DXGI_SAMPLE_DESC {
    if samples.Count == 0 {
        DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        }
    } else {
        samples
    }
}

impl GraphicsState {
    /// Captures graphics pipeline state from a plain
    /// `D3D12_GRAPHICS_PIPELINE_STATE_DESC` (`ID3D12Device::CreateGraphicsPipelineState`).
    ///
    /// # Safety
    /// `desc` points at a valid `D3D12_GRAPHICS_PIPELINE_STATE_DESC`, and if
    /// `desc.InputLayout.pInputElementDescs` is non-null it points at
    /// `desc.InputLayout.NumElements` valid elements with NUL-terminated names.
    pub(crate) unsafe fn from_graphics_desc(desc: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> Self {
        let mut state = GraphicsState {
            blend: desc.BlendState,
            sample_mask: desc.SampleMask,
            rasterizer: desc.RasterizerState,
            depth: desc.DepthStencilState,
            depth_bounds: false,
            inputs: Vec::new(),
            names: Vec::new(),
            strip_cut: desc.IBStripCutValue,
            topology: desc.PrimitiveTopologyType,
            targets: D3D12_RT_FORMAT_ARRAY {
                RTFormats: desc.RTVFormats,
                NumRenderTargets: desc.NumRenderTargets,
            },
            depth_format: DXGI_FORMAT(crate::resource::pc_format(desc.DSVFormat.0 as u32) as i32),
            samples: sanitize_samples(desc.SampleDesc),
        };
        state.targets = sanitize_targets(&state.targets);
        for f in &mut state.targets.RTFormats[..state.targets.NumRenderTargets as usize] {
            f.0 = crate::resource::target_format(crate::resource::pc_format(f.0 as u32)) as i32;
        }
        let layout = desc.InputLayout;
        if !layout.pInputElementDescs.is_null() && layout.NumElements > 0 {
            // SAFETY: caller guarantees valid `NumElements` elements and semantic names.
            unsafe {
                let elements = std::slice::from_raw_parts(
                    layout.pInputElementDescs,
                    layout.NumElements as usize,
                );
                state.names = elements
                    .iter()
                    .map(|e| {
                        if e.SemanticName.0.is_null() {
                            CString::default()
                        } else {
                            CStr::from_ptr(e.SemanticName.0.cast()).to_owned()
                        }
                    })
                    .collect();
                state.inputs = elements
                    .iter()
                    .zip(&state.names)
                    .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                        SemanticName: PCSTR(name.as_ptr().cast()),
                        Format: DXGI_FORMAT(
                            crate::resource::pc_format(e.Format.0 as u32) as i32
                        ),
                        ..*e
                    })
                    .collect();
            }
        }
        state
    }

    /// Whether the draws are a pass over the screen: no vertex input, no
    /// depth or stencil, a target. Such passes fill tables once; their shaders
    /// are few. (After Belobug/Oathkeep-Belobug-AMD 18e57db8.)
    pub(crate) fn screen_pass(&self) -> bool {
        self.inputs.is_empty()
            && (self.depth_format == DXGI_FORMAT_UNKNOWN
                || !self.depth.DepthEnable.as_bool() && !self.depth.StencilEnable.as_bool())
            && self.targets.NumRenderTargets > 0
            && self.topology == D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE
    }

    /// Takes in one subobject of a stream: its type, then its data at +4
    /// (+8 for those holding pointers).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind`.
    pub(crate) unsafe fn read(&mut self, kind: u32, at: *const u8) {
        // SAFETY: the caller passes a whole subobject.
        unsafe {
            let data = at.add(4);
            match kind {
                8 => self.blend = data.cast::<D3D12_BLEND_DESC>().read_unaligned(),
                9 => self.sample_mask = data.cast::<u32>().read_unaligned(),
                10 => self.rasterizer = data.cast::<D3D12_RASTERIZER_DESC>().read_unaligned(),
                11 => {
                    self.depth = data.cast::<D3D12_DEPTH_STENCIL_DESC>().read_unaligned();
                    self.depth_bounds = false;
                }
                21 => {
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC1>().read_unaligned();
                    self.depth_bounds = d.DepthBoundsTestEnable.as_bool();
                    self.depth = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.StencilReadMask,
                        StencilWriteMask: d.StencilWriteMask,
                        FrontFace: d.FrontFace,
                        BackFace: d.BackFace,
                    };
                }
                12 => {
                    let layout = at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned();
                    if layout.pInputElementDescs.is_null() {
                        return;
                    }
                    let elements = std::slice::from_raw_parts(
                        layout.pInputElementDescs,
                        layout.NumElements as usize,
                    );
                    self.names = elements
                        .iter()
                        .map(|e| CStr::from_ptr(e.SemanticName.0.cast()).to_owned())
                        .collect();
                    self.inputs = elements
                        .iter()
                        .zip(&self.names)
                        .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                            SemanticName: PCSTR(name.as_ptr().cast()),
                            Format: DXGI_FORMAT(
                                crate::resource::pc_format(e.Format.0 as u32) as i32
                            ),
                            ..*e
                        })
                        .collect();
                }
                13 => {
                    self.strip_cut =
                        D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(data.cast::<i32>().read_unaligned())
                }
                14 => {
                    self.topology =
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE(data.cast::<i32>().read_unaligned())
                }
                15 => {
                    self.targets = data.cast::<D3D12_RT_FORMAT_ARRAY>().read_unaligned();
                    for f in &mut self.targets.RTFormats {
                        f.0 = crate::resource::target_format(crate::resource::pc_format(
                            f.0 as u32,
                        )) as i32;
                    }
                }
                16 => {
                    let f = data.cast::<u32>().read_unaligned();
                    self.depth_format = DXGI_FORMAT(crate::resource::pc_format(f) as i32);
                }
                17 => self.samples = data.cast::<DXGI_SAMPLE_DESC>().read_unaligned(),
                _ => {}
            }
        }
    }

    /// Everything the state holds, as text: part of a pipeline's name in
    /// the library and of the key a title pipeline is started early by.
    pub(crate) fn describe(&self) -> String {
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .zip(&self.names)
            .map(|(e, name)| {
                format!(
                    "{name:?} {} {} {} {} {} {}",
                    e.SemanticIndex, e.Format.0, e.InputSlot, e.AlignedByteOffset, e.InputSlotClass.0, e.InstanceDataStepRate
                )
            })
            .collect();
        let bounds = if self.depth_bounds {
            " depth-bounds"
        } else {
            ""
        };
        format!(
            "{:?} {} {:?} {:?}{bounds} {inputs:?} {} {} {:?} {} {:?}",
            self.blend,
            self.sample_mask,
            self.rasterizer,
            self.depth,
            self.strip_cut.0,
            self.topology.0,
            self.targets,
            self.depth_format.0,
            self.samples
        )
    }

    pub(super) fn depth1(&self) -> D3D12_DEPTH_STENCIL_DESC1 {
        let d = &self.depth;
        D3D12_DEPTH_STENCIL_DESC1 {
            DepthEnable: d.DepthEnable,
            DepthWriteMask: d.DepthWriteMask,
            DepthFunc: d.DepthFunc,
            StencilEnable: d.StencilEnable,
            StencilReadMask: d.StencilReadMask,
            StencilWriteMask: d.StencilWriteMask,
            FrontFace: d.FrontFace,
            BackFace: d.BackFace,
            DepthBoundsTestEnable: self.depth_bounds.into(),
        }
    }

    /// The vertex buffer slots the input layout reads.
    pub(super) fn slots(&self) -> u32 {
        self.inputs
            .iter()
            .fold(0, |m, e| m | 1 << e.InputSlot.min(31))
    }

    /// The input elements with their byte offsets worked out.
    pub(super) fn elements(&self) -> Vec<Input> {
        let mut next = [0u32; 32];
        self.inputs
            .iter()
            .map(|e| {
                let slot = e.InputSlot.min(31) as usize;
                let format = e.Format.0 as u32;
                let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                // An appended element starts at the next multiple of
                // min(4, its size) after the previous one, where D3D12 (and
                // vkd3d) place it; the host pipeline gets the title's
                // descriptions, so the offsets read here must match.
                let offset = if e.AlignedByteOffset == D3D12_APPEND_ALIGNED_ELEMENT {
                    next[slot]
                        .checked_next_multiple_of(bytes.clamp(1, 4))
                        .unwrap_or(u32::MAX)
                } else {
                    e.AlignedByteOffset
                };
                next[slot] = offset.saturating_add(bytes);
                Input {
                    slot: slot as u32,
                    offset,
                    format,
                    per_instance: e.InputSlotClass == D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                }
            })
            .collect()
    }
}

/// One input element: vertex buffer slot, byte offset, format.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input {
    pub(crate) slot: u32,
    pub(crate) offset: u32,
    pub(crate) format: u32,
    pub(crate) per_instance: bool,
}
