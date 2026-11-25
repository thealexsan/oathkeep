use crate::ShaderError;
use crate::gfx10::{Fields, Operand};
use crate::ir::{Exit, Inst, Program, Reg, Value, operand_reads};
use crate::native::{HwStage, NativeProgram, PsInput, StageIo, Usage};
use crate::resource::Site;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    VertexId,
    PrimitiveId,
    InstanceId,
    Attribute { row: u8, channel: u8 },
    Barycentric { kind: Barycentric, index: u8 },
    Position(u8),
    FrontFace,
    Ancillary,
    SampleCoverage,
    FixedPosition,
    LineStipple,
    GroupId(u8),
    ThreadIdInGroup(u8),
    GroupInfo,
    MeshGroup,
    MergedWaveInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barycentric {
    PerspectiveSample,
    PerspectiveCenter,
    PerspectiveCentroid,
    PerspectivePullModel,
    LinearSample,
    LinearCenter,
    LinearCentroid,
}

const PS_VGPRS: [(u8, Option<Barycentric>); 16] = [
    (2, Some(Barycentric::PerspectiveSample)),
    (2, Some(Barycentric::PerspectiveCenter)),
    (2, Some(Barycentric::PerspectiveCentroid)),
    (3, Some(Barycentric::PerspectivePullModel)),
    (2, Some(Barycentric::LinearSample)),
    (2, Some(Barycentric::LinearCenter)),
    (2, Some(Barycentric::LinearCentroid)),
    (1, None),
    (1, None),
    (1, None),
    (1, None),
    (1, None),
    (1, None),
    (1, None),
    (1, None),
    (1, None),
];

fn ps_system(bit: usize) -> Input {
    match bit {
        7 => Input::LineStipple,
        8..=11 => Input::Position(bit as u8 - 8),
        12 => Input::FrontFace,
        13 => Input::Ancillary,
        14 => Input::SampleCoverage,
        _ => Input::FixedPosition,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribute {
    pub usage: Usage,
    pub row: u8,
    pub channel: u8,
    pub high: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interp {
    Interpolated {
        attribute: Attribute,
        barycentric: Barycentric,
        first: Vec<Site>,
    },
    Vertex {
        attribute: Attribute,
        vertex: u8,
    },
    Weighted {
        attribute: Attribute,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Color(u8),
    Depth,
    Null,
    Position(u8),
    Param { usage: Usage, row: u8 },
    Unread,
    Primitive,
    PrimitiveParam { usage: Usage, row: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub site: Site,
    pub target: Target,
    pub values: [Option<Value>; 4],
    pub compressed: bool,
    pub done: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Io {
    pub inputs: Vec<(Value, Input)>,
    pub interps: Vec<(Site, Interp)>,
    pub exports: Vec<Export>,
    pub initialized_sgprs: Option<u8>,
}

pub fn raise(p: &Program, native: &NativeProgram) -> Result<Io, ShaderError> {
    let unsupported = |offset, what| ShaderError::Unsupported { offset, what };
    let (loaded, ps_inputs, params) = match (&native.io, native.stage) {
        (
            StageIo::Ps {
                inputs, input_addr, ..
            },
            HwStage::Ps,
        ) => {
            let loaded = ps_vgprs(*input_addr)
                .into_iter()
                .map(|(r, i)| (Reg::V(r), i))
                .collect();
            (loaded, &inputs[..], &[][..])
        }
        (StageIo::Vs { inputs, outputs }, HwStage::Vs) => {
            let mut loaded = vec![
                (Reg::V(0), Input::VertexId),
                (Reg::V(2), Input::PrimitiveId),
                (Reg::V(3), Input::InstanceId),
            ];
            for i in inputs {
                for channel in (0..4).filter(|c| i.mask >> c & 1 != 0) {
                    let input = Input::Attribute {
                        row: i.row,
                        channel,
                    };
                    loaded.push((Reg::V(i.vgpr + channel), input));
                }
            }
            (loaded, &[][..], &outputs[..])
        }
        (StageIo::Cs(cs), HwStage::Cs) => {
            let (ids, info) = cs.system_sgprs();
            let mut loaded: Vec<(Reg, Input)> = (0..cs.thread_id_vgprs())
                .map(|c| (Reg::V(c), Input::ThreadIdInGroup(c)))
                .collect();
            for (c, s) in ids.into_iter().enumerate() {
                if let Some(s) = s {
                    loaded.push((Reg::S(s), Input::GroupId(c as u8)));
                }
            }
            loaded.extend(info.map(|s| (Reg::S(s), Input::GroupInfo)));
            (loaded, &[][..], &[][..])
        }
        (StageIo::Ms(ms), HwStage::Ms) => {
            let mut group: Vec<Value> = p
                .inputs
                .iter()
                .filter(|i| i.0 == Reg::V(5))
                .map(|i| i.1)
                .collect();
            let no_lanes = |q: usize, b: usize| match (
                p.blocks[q].insts.last().map(Inst::name),
                p.blocks[q].exit,
            ) {
                (Some("s_cbranch_execz"), Exit::Branch { taken, not_taken }) => {
                    taken == b && not_taken != b
                }
                (Some("s_cbranch_execnz"), Exit::Branch { taken, not_taken }) => {
                    not_taken == b && taken != b
                }
                _ => false,
            };
            while let Some(v) = p
                .blocks
                .iter()
                .enumerate()
                .flat_map(|(b, k)| k.phis.iter().map(move |f| (b, f)))
                .find_map(|(b, f)| {
                    let mut args = f.args.iter().zip(&p.blocks[b].preds);
                    let carried = args.any(|(a, &q)| group.contains(a) && !no_lanes(q, b));
                    (carried && !group.contains(&f.value)).then_some(f.value)
                })
            {
                group.push(v);
            }
            let per_lane = p.blocks.iter().flat_map(|b| &b.insts).find(|i| {
                i.read(Reg::V(5)).is_some_and(|v| group.contains(&v))
                    && i.name() != "v_readfirstlane_b32"
                    && (i.write(Reg::V(5)).is_none() || operand_reads(p, i).contains(&Reg::V(5)))
            });
            if let Some(i) = per_lane {
                return Err(unsupported(
                    i.machine.offset,
                    "the mesh group input read per lane",
                ));
            }
            let loaded = vec![
                (Reg::V(5), Input::MeshGroup),
                (Reg::S(3), Input::MergedWaveInfo),
            ];
            (loaded, &[][..], &ms.outputs[..])
        }
        _ => return Err(unsupported(0, "I/O of this stage")),
    };
    let inputs: Vec<(Value, Input)> = p
        .inputs
        .iter()
        .filter_map(|&(reg, v)| loaded.iter().find(|l| l.0 == reg).map(|l| (v, l.1)))
        .collect();
    let input_of = |v: Value| inputs.iter().find(|i| i.0 == v).map(|i| i.1);
    let attribute = |inst: &Inst, attr: &Operand| match *attr {
        Operand::Attr { index, chan, high } => ps_inputs
            .iter()
            .find(|i: &&PsInput| i.attr == index)
            .map(|i| Attribute {
                usage: i.usage,
                row: i.row,
                channel: chan,
                high,
            })
            .ok_or(unsupported(
                inst.machine.offset,
                "an interpolated attribute the stage data does not list",
            )),
        _ => Err(unsupported(inst.machine.offset, "an interpolation operand")),
    };
    let barycentric = |v: Option<Value>| match v.and_then(input_of) {
        Some(Input::Barycentric { kind, index }) => Some((kind, index)),
        _ => None,
    };

    let mut io = Io {
        inputs: inputs.clone(),
        initialized_sgprs: match &native.io {
            StageIo::Cs(cs) => Some(cs.initialized_sgprs()),
            _ => None,
        },
        ..Io::default()
    };
    let defs: std::collections::HashMap<Value, Site> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, block)| {
            block
                .insts
                .iter()
                .enumerate()
                .flat_map(move |(i, inst)| inst.writes.iter().map(move |w| (w.1, (b, i))))
        })
        .collect();
    let phis: std::collections::HashMap<Value, (usize, usize)> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, block)| {
            block
                .phis
                .iter()
                .enumerate()
                .map(move |(i, phi)| (phi.value, (b, i)))
        })
        .collect();
    let halves = Halves {
        p,
        defs: &defs,
        phis: &phis,
    };
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            let site = (b, i);
            let at = inst.machine.offset;
            let name = inst.name();
            match &inst.fields {
                Fields::Vintrp { dst, src, attr } => {
                    let attribute = attribute(inst, attr)?;
                    match (name, *src) {
                        ("v_interp_mov_f32", Operand::Const(vertex)) => {
                            io.interps
                                .push((site, Interp::Vertex { attribute, vertex }));
                        }
                        ("v_interp_p1_f32", _) => {}
                        ("v_interp_p2_f32", Operand::Vector { reg, .. }) => {
                            let Operand::Vector { reg: d, .. } = *dst else {
                                return Err(unsupported(at, "an interpolation operand"));
                            };
                            let interp = halves
                                .pair(
                                    attr,
                                    inst.read(Reg::V(d)),
                                    inst.read(Reg::V(reg)),
                                    &barycentric,
                                )
                                .map_err(|what| unsupported(at, what))?;
                            push_pair(&mut io, site, interp, attribute);
                        }
                        _ => return Err(unsupported(at, "an interpolation form")),
                    }
                }
                Fields::Valu(v) if name.starts_with("v_interp_") => match name {
                    "v_interp_p1ll_f16" => {}
                    "v_interp_p2_f16" => {
                        let attribute = attribute(inst, &v.src[0])?;
                        let read = |k: usize| match v.src.get(k) {
                            Some(&Operand::Vector { reg, .. }) => inst.read(Reg::V(reg)),
                            _ => None,
                        };
                        let interp = halves
                            .pair(&v.src[0], read(2), read(1), &barycentric)
                            .map_err(|what| unsupported(at, what))?;
                        push_pair(&mut io, site, interp, attribute);
                    }
                    _ => return Err(unsupported(at, "an interpolation form")),
                },
                Fields::Exp {
                    target,
                    src,
                    compr,
                    done,
                    ..
                } => {
                    let target = match *target {
                        t @ 0..=7 => Target::Color(t),
                        8 => Target::Depth,
                        9 => Target::Null,
                        t @ 12..=16 => Target::Position(t - 12),
                        20 => Target::Primitive,
                        32 if params.is_empty() => Target::Unread,
                        t @ 32..=63 => {
                            let o = params.iter().find(|o| o.param & 0x7F == t - 32).ok_or(
                                unsupported(at, "a parameter export the stage data does not list"),
                            )?;
                            let (usage, row) = (o.usage, o.row);
                            if o.param & 0x80 != 0 {
                                Target::PrimitiveParam { usage, row }
                            } else {
                                Target::Param { usage, row }
                            }
                        }
                        _ => return Err(unsupported(at, "an export target")),
                    };
                    let values = src.map(|s| match s {
                        Some(Operand::Vector { reg, .. }) => inst.read(Reg::V(reg)),
                        _ => None,
                    });
                    io.exports.push(Export {
                        site,
                        target,
                        values,
                        compressed: *compr,
                        done: *done,
                    });
                }
                _ => {}
            }
        }
    }
    Ok(io)
}

fn ps_vgprs(addr: u32) -> Vec<(u8, Input)> {
    let mut out = Vec::new();
    let mut next = 0u8;
    for (bit, &(count, kind)) in PS_VGPRS.iter().enumerate() {
        if addr >> bit & 1 == 0 {
            continue;
        }
        for index in 0..count {
            let input = match kind {
                Some(kind) => Input::Barycentric { kind, index },
                None => ps_system(bit),
            };
            out.push((next + index, input));
        }
        next += count;
    }
    out
}

struct Pair {
    barycentric: Option<Barycentric>,
    first: Vec<Site>,
}

impl Pair {
    fn with(self, attribute: Attribute) -> Interp {
        match self.barycentric {
            Some(barycentric) => Interp::Interpolated {
                attribute,
                barycentric,
                first: self.first,
            },
            None => Interp::Weighted { attribute },
        }
    }
}

fn push_pair(io: &mut Io, site: Site, pair: Pair, attribute: Attribute) {
    if pair.barycentric.is_none() {
        for &f in &pair.first {
            if !io.interps.iter().any(|(s, _)| *s == f) {
                io.interps.push((f, Interp::Weighted { attribute }));
            }
        }
    }
    io.interps.push((site, pair.with(attribute)));
}

struct Halves<'a> {
    p: &'a Program,
    defs: &'a std::collections::HashMap<Value, Site>,
    phis: &'a std::collections::HashMap<Value, (usize, usize)>,
}

impl Halves<'_> {
    fn pair(
        &self,
        attr: &Operand,
        acc: Option<Value>,
        j: Option<Value>,
        barycentric: &impl Fn(Option<Value>) -> Option<(Barycentric, u8)>,
    ) -> Result<Pair, &'static str> {
        let mut first = Vec::new();
        self.sites(
            acc.ok_or("an interpolation without its first half")?,
            &mut Vec::new(),
            &mut first,
        )?;
        let mut kind = match barycentric(j) {
            Some((kind, 1)) => Some(kind),
            _ => None,
        };
        for &(b, i) in &first {
            let inst = &self.p.blocks[b].insts[i];
            let (a, i) = match &inst.fields {
                Fields::Vintrp {
                    src: Operand::Vector { reg, .. },
                    attr,
                    ..
                } if inst.name() == "v_interp_p1_f32" => (*attr, inst.read(Reg::V(*reg))),
                Fields::Valu(v) if inst.name() == "v_interp_p1ll_f16" => match v.src.get(1) {
                    Some(&Operand::Vector { reg, .. }) => (v.src[0], inst.read(Reg::V(reg))),
                    _ => return Err("an interpolation without its first half"),
                },
                _ => return Err("an interpolation without its first half"),
            };
            if a != *attr {
                return Err("interpolation halves of different attributes");
            }
            if kind.is_some_and(|k| barycentric(i) != Some((k, 0))) {
                kind = None;
            }
        }
        Ok(Pair {
            barycentric: kind,
            first,
        })
    }

    fn sites(
        &self,
        v: Value,
        seen: &mut Vec<Value>,
        out: &mut Vec<Site>,
    ) -> Result<(), &'static str> {
        if let Some(&site) = self.defs.get(&v) {
            if !out.contains(&site) {
                out.push(site);
            }
            return Ok(());
        }
        let &(b, i) = self
            .phis
            .get(&v)
            .ok_or("an interpolation without its first half")?;
        if !seen.contains(&v) {
            seen.push(v);
            for &a in &self.p.blocks[b].phis[i].args {
                self.sites(a, seen, out)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ExportPatch;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn program(words: &[u32]) -> Program {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        crate::ir::build(&code, false, 256).unwrap()
    }

    fn pixel(inputs: Vec<PsInput>, input_addr: u32) -> NativeProgram {
        NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 256,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs,
                input_addr,
                db_shader_control: 0,
                patches: Vec::<ExportPatch>::new(),
            },
            code: vec![],
        }
    }

    const TEXCOORD: PsInput = PsInput {
        usage: Usage::Generic,
        row: 1,
        mask: 3,
        attr: 0,
        flat: false,
        fp16: false,
        pack2: false,
        row2: 1,
    };

    #[test]
    fn interpolation_pairs_and_color_exports() {
        let p = program(&[0xC808_0000, 0xC809_0001, 0xF800_180F, 0x0302_0102, S_ENDPGM]);
        let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
        let input = |reg| p.inputs.iter().find(|i| i.0 == Reg::V(reg)).unwrap().1;
        let center = |index| Input::Barycentric {
            kind: Barycentric::PerspectiveCenter,
            index,
        };
        assert!(io.inputs.contains(&(input(0), center(0))));
        assert!(io.inputs.contains(&(input(1), center(1))));
        assert!(!io.inputs.iter().any(|i| i.0 == input(3)));
        assert_eq!(
            io.interps,
            [(
                (0, 1),
                Interp::Interpolated {
                    attribute: Attribute {
                        usage: Usage::Generic,
                        row: 1,
                        channel: 0,
                        high: false
                    },
                    barycentric: Barycentric::PerspectiveCenter,
                    first: vec![(0, 0)],
                }
            )]
        );
        let [e] = &io.exports[..] else {
            panic!("{:?}", io.exports)
        };
        assert_eq!(
            (e.target, e.done, e.compressed),
            (Target::Color(0), true, false)
        );
        assert_eq!(e.values[0], p.blocks[0].insts[1].write(Reg::V(2)));
    }

    #[test]
    fn unpaired_or_unknown_interpolation_fails() {
        let lone = program(&[0xC809_0001, S_ENDPGM]);
        assert!(raise(&lone, &pixel(vec![TEXCOORD], 2)).is_err());
        let other = program(&[0xC808_0400, S_ENDPGM]);
        assert!(raise(&other, &pixel(vec![TEXCOORD], 2)).is_err());
    }

    #[test]
    fn computed_barycentrics_evaluate_each_half() {
        let p = program(&[0x1006_00F0, 0xC808_0003, 0xC809_0001, S_ENDPGM]);
        let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
        let attribute = Attribute {
            usage: Usage::Generic,
            row: 1,
            channel: 0,
            high: false,
        };
        let mut interps = io.interps.clone();
        interps.sort_by_key(|i| i.0);
        assert_eq!(
            interps,
            [
                ((0, 1), Interp::Weighted { attribute }),
                ((0, 2), Interp::Weighted { attribute }),
            ]
        );
    }

    #[test]
    fn mesh_inputs_and_primitive_export() {
        let mesh = |words: &[u32]| {
            let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let p = crate::ir::build(&code, true, 256).unwrap();
            let native = NativeProgram {
                stage: HwStage::Ms,
                wave64: true,
                vgprs: 256,
                layout: vec![],
                used: vec![],
                io: StageIo::Ms(crate::native::MeshIo {
                    outputs: vec![],
                    clip_cntl: 0,
                    threads: 128,
                    max_vertices: 128,
                    max_primitives: 128,
                    topology: 2,
                    lds_bytes: 0,
                }),
                code,
            };
            raise(&p, &native)
        };
        let io = mesh(&[0x7E04_0505, 0xF800_0941, 0x0000_0009, S_ENDPGM]).unwrap();
        assert!(io.inputs.iter().any(|i| i.1 == Input::MeshGroup));
        assert_eq!(io.exports[0].target, Target::Primitive);
        assert!(mesh(&[0x7E02_0305, S_ENDPGM]).is_err());
        let per_lane: [&[u32]; 5] = [
            &[0x4A0A_0305, S_ENDPGM],
            &[0x7E0A_0D05, S_ENDPGM],
            &[0xDC30_8000, 0x0500_0005, S_ENDPGM],
            &[0x7E0A_CB01, S_ENDPGM],
            &[0xBF06_8001, 0xBF85_0001, 0x7E0A_0280, 0x7E02_0305, S_ENDPGM],
        ];
        let accepted: Vec<_> = per_lane.iter().filter(|w| mesh(w).is_ok()).collect();
        assert!(accepted.is_empty(), "accepted: {accepted:08X?}");
        assert!(mesh(&[0x7E0A_0280, 0x7E02_0305, S_ENDPGM]).is_ok());
        let reuse = [
            0x7E04_0505,
            0x7D88_0084,
            0xBE86_246A,
            0xBF88_0001,
            0x7E0A_0280,
            0x88FE_067E,
            0xBE86_246A,
            0xBF88_0001,
            0x7E02_0305,
            0x88FE_067E,
            S_ENDPGM,
        ];
        assert_eq!(mesh(&reuse).map(|_| ()), Ok(()));
    }

    #[test]
    fn pixel_vgprs_follow_the_input_address() {
        let v = ps_vgprs(1 << 5 | 1 << 8 | 1 << 9);
        assert_eq!(
            v,
            [
                (
                    0,
                    Input::Barycentric {
                        kind: Barycentric::LinearCenter,
                        index: 0
                    }
                ),
                (
                    1,
                    Input::Barycentric {
                        kind: Barycentric::LinearCenter,
                        index: 1
                    }
                ),
                (2, Input::Position(0)),
                (3, Input::Position(1)),
            ]
        );
    }
}
