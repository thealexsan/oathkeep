use super::{Bad, Barrier, Blob, Dec, Loc, Slot, Va, Wire};

wire_enum! {
    /// A command of a list, objects by id, views by what they hold.
    Cmd {
        1 Draw { vertices: u32, instances: u32, first: u32, first_instance: u32 },
        2 DrawIndexed { indices: u32, instances: u32, first: u32, base: i32, first_instance: u32 },
        3 Dispatch { groups: [u32; 3] },
        4 DispatchMesh { groups: [u32; 3] },
        5 CopyBuffer { dst: u32, dst_at: u64, src: u32, src_at: u64, len: u64 },
        6 CopyTexture { dst: Loc, at: [u32; 3], src: Loc, part: Option<[u32; 6]> },
        7 CopyResource { dst: u32, src: u32 },
        8 Resolve { dst: u32, dst_sub: u32, src: u32, src_sub: u32, format: i32 },
        9 ResolveRegion { dst: u32, dst_sub: u32, at: [u32; 2], src: u32, src_sub: u32, part: Option<[i32; 4]>, format: i32, mode: i32 },
        10 Topology { kind: i32 },
        11 Viewports { all: Vec<[f32; 6]> },
        12 Scissors { all: Vec<[i32; 4]> },
        13 BlendFactor { factor: Option<[f32; 4]> },
        14 StencilRef { value: u32 },
        15 Pipeline { pso: u32 },
        16 Barriers { all: Vec<Barrier> },
        17 Heaps { heaps: Vec<u32> },
        18 RootSignature { graphics: bool, root: u32 },
        19 Table { graphics: bool, param: u32, heap: u32, slot: u32 },
        20 Constants { graphics: bool, param: u32, offset: u32, values: Vec<u32> },
        21 RootView { graphics: bool, kind: u8, param: u32, va: Va },
        22 IndexBuffer { view: Option<(Va, u32, i32)> },
        23 VertexBuffers { start: u32, count: u32, views: Option<Vec<(Va, u32, u32)>> },
        24 Targets { colors: Vec<Slot>, depth: Option<Slot> },
        25 ClearDepth { view: Slot, flags: i32, depth: f32, stencil: u8, rects: Vec<[i32; 4]> },
        26 ClearTarget { view: Slot, color: [f32; 4], rects: Vec<[i32; 4]> },
        27 Discard { res: u32, rects: Vec<[i32; 4]>, subs: Option<[u32; 2]> },
        28 BeginQuery { heap: u32, kind: i32, index: u32 },
        29 EndQuery { heap: u32, kind: i32, index: u32 },
        30 ResolveQuery { heap: u32, kind: i32, first: u32, count: u32, dst: u32, at: u64 },
        31 Indirect { signature: u32, most: u32, args: u32, at: u64, count: u32, count_at: u64 },
        32 WriteImmediate { all: Vec<(Va, u32, i32)> },
        33 DepthBounds { min: f32, max: f32 },
    }
}

/// The highest kind number of [`Cmd`].
#[cfg(test)]
pub(super) const LAST_CMD: u8 = 33;

impl Cmd {
    /// The commands of a list's bytes.
    pub fn all(bytes: &[u8]) -> Result<Vec<Cmd>, Bad> {
        let mut from = Dec::new(bytes);
        let mut all = Vec::new();
        while !from.bytes.is_empty() {
            all.push(Cmd::get(&mut from)?);
        }
        Ok(all)
    }

    /// Whether it makes the card work (and is timed on its own).
    pub fn works(&self) -> bool {
        self.runs_shaders()
            || matches!(
                self,
                Cmd::CopyBuffer { .. }
                    | Cmd::CopyTexture { .. }
                    | Cmd::CopyResource { .. }
                    | Cmd::Resolve { .. }
                    | Cmd::ResolveRegion { .. }
                    | Cmd::ClearDepth { .. }
                    | Cmd::ClearTarget { .. }
                    | Cmd::Discard { .. }
                    | Cmd::ResolveQuery { .. }
                    | Cmd::WriteImmediate { .. }
            )
    }

    /// Whether it runs the pipeline bound.
    pub fn runs_shaders(&self) -> bool {
        matches!(
            self,
            Cmd::Draw { .. }
                | Cmd::DrawIndexed { .. }
                | Cmd::Dispatch { .. }
                | Cmd::DispatchMesh { .. }
                | Cmd::Indirect { .. }
        )
    }
}

wire_struct! {
    HeapProps { kind: i32, cpu: i32, pool: i32 }
}

wire_struct! {
    /// A resource description (`D3D12_RESOURCE_DESC`).
    Desc {
        dimension: i32,
        alignment: u64,
        width: u64,
        height: u32,
        depth: u16,
        mips: u16,
        format: i32,
        samples: [u32; 2],
        layout: i32,
        flags: i32,
    }
}

wire_struct! {
    Heap { id: u32, size: u64, props: HeapProps, alignment: u64, flags: i32 }
}

wire_enum! {
    /// How a resource got its memory.
    Place {
        0 Committed { props: HeapProps, flags: i32 },
        1 Placed { heap: u32, offset: u64 },
        /// A swap chain's buffer.
        2 BackBuffer {},
    }
}

wire_struct! {
    Resource {
        id: u32,
        place: Place,
        desc: Desc,
        /// The state it was made in.
        made_in: i32,
        clear: Option<[u32; 5]>,
        /// Its states at the start of the frame: one, or one for each
        /// subresource. Buffers start in the common state.
        state: Vec<u32>,
        /// Not the one in use where placed resources overlap: its contents
        /// are not in the capture.
        idle: bool,
        /// Made after the frame started.
        late: bool,
        name: String,
    }
}

wire_enum! {
    /// A part of a pipeline description.
    Part {
        0 Root { root: u32 },
        /// Shader code: the number of its blob ([`Frame::codes`]).
        1 Code { kind: i32, blob: u32 },
        /// Plain data.
        2 Plain { kind: i32, data: Blob },
        3 Layout { elements: Vec<(String, [u32; 6])> },
    }
}

wire_struct! {
    Pipeline { id: u32, parts: Vec<Part>, name: String }
}

wire_struct! {
    /// Bytes in `shaders.bin`.
    Code { at: u64, len: u64, hash: [u8; 32] }
}

wire_struct! {
    Signature { id: u32, stride: u32, args: Vec<[u32; 4]>, root: u32 }
}

wire_struct! {
    DescriptorHeap { id: u32, kind: i32, count: u32, flags: i32, runs: Vec<(u32, u32, Slot)> }
}

wire_struct! {
    QueryHeap { id: u32, kind: i32, count: u32 }
}

wire_struct! {
    List { kind: i32, commands: Blob }
}

wire_struct! {
    /// Bytes the CPU (or an earlier submission) put into a buffer the CPU
    /// sees, by the time a submission went to the card (in `deltas.bin`).
    Delta { res: u32, at: u64, len: u64, file: u64 }
}

wire_struct! {
    Submit {
        frame: u32,
        /// Lists by number; [`MISSING`] for one that was not recorded.
        lists: Vec<u32>,
        deltas: Vec<Delta>,
        /// Descriptor slots written since the submission before.
        views: Vec<(u32, u32, Slot)>,
    }
}
