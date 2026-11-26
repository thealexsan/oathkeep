use super::*;

use super::*;

fn there_and_back<T: Wire + PartialEq + std::fmt::Debug>(x: &T) {
    let mut out = Vec::new();
    x.put(&mut out);
    let mut from = Dec::new(&out);
    assert_eq!(&T::get(&mut from).unwrap(), x);
    assert!(from.bytes.is_empty(), "bytes left over");
    // Cut anywhere, it is refused and does not panic.
    for n in 0..out.len() {
        assert!(T::get(&mut Dec::new(&out[..n])).is_err());
    }
}

fn view() -> Slot {
    let mut desc = [0; 52];
    desc[3] = 7;
    desc[51] = 9;
    Slot {
        kind: slot::SAMPLER,
        described: 1,
        pad: 0,
        res: 4,
        counter: 5,
        desc,
    }
}

fn at() -> Va {
    Va::At {
        res: 3,
        offset: 1 << 40,
    }
}

pub(crate) fn every_command() -> Vec<Cmd> {
    let placed = Loc::Placed {
        res: 5,
        offset: 512,
        format: 28,
        size: [4, 5, 6],
        pitch: 256,
    };
    let rects = vec![[1, 2, 3, 4]];
    vec![
        Cmd::Draw {
            vertices: 1,
            instances: 2,
            first: 3,
            first_instance: 4,
        },
        Cmd::DrawIndexed {
            indices: 1,
            instances: 2,
            first: 3,
            base: -4,
            first_instance: 5,
        },
        Cmd::Dispatch { groups: [1, 2, 3] },
        Cmd::DispatchMesh { groups: [4, 5, 6] },
        Cmd::CopyBuffer {
            dst: 1,
            dst_at: 2,
            src: 3,
            src_at: 4,
            len: 5,
        },
        Cmd::CopyTexture {
            dst: Loc::Sub { res: 1, sub: 2 },
            at: [3, 4, 5],
            src: placed,
            part: Some([1, 2, 3, 4, 5, 6]),
        },
        Cmd::CopyResource { dst: 1, src: 2 },
        Cmd::Resolve {
            dst: 1,
            dst_sub: 2,
            src: 3,
            src_sub: 4,
            format: 28,
        },
        Cmd::ResolveRegion {
            dst: 1,
            dst_sub: 2,
            at: [3, 4],
            src: 5,
            src_sub: 6,
            part: Some([0, 1, 2, 3]),
            format: 28,
            mode: 1,
        },
        Cmd::Topology { kind: 4 },
        Cmd::Viewports {
            all: vec![[0.0, 1.0, 2.0, 3.0, 0.5, 1.0]; 2],
        },
        Cmd::Scissors { all: rects.clone() },
        Cmd::BlendFactor {
            factor: Some([0.1, 0.2, 0.3, 0.4]),
        },
        Cmd::BlendFactor { factor: None },
        Cmd::StencilRef { value: 255 },
        Cmd::Pipeline { pso: 9 },
        Cmd::Barriers {
            all: vec![
                Barrier::Transition {
                    flags: 1,
                    res: 2,
                    sub: ALL,
                    before: 4,
                    after: 8,
                },
                Barrier::Alias {
                    flags: 0,
                    before: 1,
                    after: 2,
                },
                Barrier::Uav { flags: 0, res: 0 },
            ],
        },
        Cmd::Heaps { heaps: vec![1, 2] },
        Cmd::RootSignature {
            graphics: true,
            root: 3,
        },
        Cmd::Table {
            graphics: false,
            param: 1,
            heap: 2,
            slot: 1 << 20,
        },
        Cmd::Constants {
            graphics: true,
            param: 1,
            offset: 2,
            values: vec![3, 4, 5],
        },
        Cmd::RootView {
            graphics: true,
            kind: slot::CBV,
            param: 1,
            va: at(),
        },
        Cmd::IndexBuffer {
            view: Some((at(), 5, 42)),
        },
        Cmd::IndexBuffer { view: None },
        Cmd::VertexBuffers {
            start: 1,
            count: 1,
            views: Some(vec![(Va::Unknown { address: 5 }, 6, 7)]),
        },
        Cmd::VertexBuffers {
            start: 1,
            count: 3,
            views: None,
        },
        Cmd::Targets {
            colors: vec![view(); 3],
            depth: Some(view()),
        },
        Cmd::ClearDepth {
            view: view(),
            flags: 3,
            depth: 0.5,
            stencil: 7,
            rects: rects.clone(),
        },
        Cmd::ClearTarget {
            view: view(),
            color: [1.0, 2.0, 3.0, 4.0],
            rects: Vec::new(),
        },
        Cmd::Discard {
            res: 1,
            rects,
            subs: Some([0, 2]),
        },
        Cmd::BeginQuery {
            heap: 1,
            kind: 0,
            index: 2,
        },
        Cmd::EndQuery {
            heap: 1,
            kind: 2,
            index: 3,
        },
        Cmd::ResolveQuery {
            heap: 1,
            kind: 2,
            first: 3,
            count: 4,
            dst: 5,
            at: 6,
        },
        Cmd::Indirect {
            signature: 1,
            most: 2,
            args: 3,
            at: 4,
            count: 5,
            count_at: 6,
        },
        Cmd::WriteImmediate {
            all: vec![(at(), 1, 2), (Va::Null {}, 3, 4)],
        },
        Cmd::DepthBounds {
            min: 0.25,
            max: 0.75,
        },
    ]
}

#[test]
fn every_command_comes_back() {
    let all = every_command();
    let mut bytes = Vec::new();
    for c in &all {
        there_and_back(c);
        c.put(&mut bytes);
    }
    assert_eq!(Cmd::all(&bytes).unwrap(), all);
    assert!(Cmd::all(&[250]).is_err());
    // Every kind there is stands in the list above.
    let kinds: std::collections::BTreeSet<u8> = all
        .iter()
        .map(|c| {
            let mut out = Vec::new();
            c.put(&mut out);
            out[0]
        })
        .collect();
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        (1..=LAST_CMD).collect::<Vec<u8>>()
    );
    assert!(
        Cmd::all(&[LAST_CMD + 1]).is_err(),
        "a kind was added: bring LAST_CMD and the list above up to date"
    );
}

#[test]
fn a_frame_comes_back() {
    let props = HeapProps {
        kind: 4,
        cpu: 3,
        pool: 1,
    };
    let desc = Desc {
        dimension: 3,
        alignment: 65536,
        width: 1 << 33,
        height: 5,
        depth: 6,
        mips: 7,
        format: 28,
        samples: [1, 0],
        layout: 1,
        flags: 4,
    };
    let resource = Resource {
        id: 2,
        place: Place::Placed {
            heap: 1,
            offset: 65536,
        },
        desc,
        made_in: 4,
        clear: Some([28, 1, 2, 3, 4]),
        state: vec![1, 2, 3],
        idle: false,
        late: true,
        name: "a texture".into(),
    };
    let frame = Frame {
        adapter: "a card".into(),
        vendor: 0x1002,
        device: 0x744c,
        notes: vec!["nothing".into()],
        heaps: vec![Heap {
            id: 1,
            size: 64 << 20,
            props: props.clone(),
            alignment: 65536,
            flags: 0x84,
        }],
        resources: vec![
            resource.clone(),
            Resource {
                place: Place::Committed { props, flags: 8 },
                ..resource.clone()
            },
            Resource {
                place: Place::BackBuffer {},
                clear: None,
                ..resource
            },
        ],
        roots: vec![(3, Blob(vec![1, 2, 3]))],
        codes: vec![Code {
            at: 1,
            len: 2,
            hash: [7; 32],
        }],
        pipelines: vec![Pipeline {
            id: 4,
            parts: vec![
                Part::Root { root: 3 },
                Part::Code { kind: 25, blob: 0 },
                Part::Plain {
                    kind: 8,
                    data: Blob(vec![9; 20]),
                },
                Part::Layout {
                    elements: vec![("POSITION".into(), [1, 2, 3, 4, 5, 6])],
                },
            ],
            name: "a pipeline".into(),
        }],
        signatures: vec![Signature {
            id: 5,
            stride: 12,
            args: vec![[10, 0, 0, 0]],
            root: 0,
        }],
        descriptor_heaps: vec![DescriptorHeap {
            id: 6,
            kind: 0,
            count: 10,
            flags: 1,
            runs: vec![(0, 9, Slot::EMPTY), (9, 1, view())],
        }],
        query_heaps: vec![QueryHeap {
            id: 7,
            kind: 1,
            count: 8,
        }],
        lists: vec![List {
            kind: 0,
            commands: Blob(vec![1, 2]),
        }],
        submits: vec![Submit {
            frame: 0,
            lists: vec![0, MISSING],
            deltas: vec![Delta {
                res: 2,
                at: 4096,
                len: 8192,
                file: 0,
            }],
            views: vec![(6, 3, view())],
        }],
        contents: vec![Content {
            res: 2,
            first: 0,
            count: 3,
            len: 100,
            file: ZERO,
        }],
        presents: vec![Present {
            submits: 1,
            res: 2,
            width: 4,
            height: 5,
            format: 28,
            pitch: 256,
            file: 0,
        }],
    };
    there_and_back(&frame);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frame.bin");
    frame.write(&path).unwrap();
    assert_eq!(Frame::read(&path).unwrap(), frame);
    std::fs::write(&path, b"VICFRAMEnothing at all").unwrap();
    assert!(Frame::read(&path).is_err());
}
