use super::bitcode::{Attr, Bin, Term, V, pred};
use super::container::SigElement;
use super::shader::{Geometry, Shader};
use crate::ShaderError;
use crate::dxbc::{Container, FourCC};

const VERTICES: u32 = 6;

const MAX_SCALARS: u32 = 1024;

pub fn rect_list_gs(vs: &[u8], ps: Option<&[u8]>) -> Result<Vec<u8>, ShaderError> {
    let sig = |dxil: &[u8], fourcc| -> Result<Vec<SigElement>, ShaderError> {
        let c = Container::parse(dxil)?;
        match c.part(fourcc) {
            Some(part) => super::signature(part.data),
            None => Ok(Vec::new()),
        }
    };
    let outputs = sig(vs, FourCC::OSG1)?;
    let wanted = match ps {
        Some(ps) => sig(ps, FourCC::ISG1)?,
        None => Vec::new(),
    };
    build(outputs, &wanted)
}

fn build(mut outputs: Vec<SigElement>, ps_inputs: &[SigElement]) -> Result<Vec<u8>, ShaderError> {
    let unsupported = |what| ShaderError::Unsupported { offset: 0, what };
    if outputs.iter().any(|e| e.row.is_none()) {
        return Err(unsupported("a rectangle list output without a register"));
    }
    let Some(position) = outputs
        .iter()
        .position(|e| e.kind == 3 && e.comp == 9 && e.cols == 4)
    else {
        return Err(unsupported(
            "a rectangle list without a four-component position",
        ));
    };
    for e in &mut outputs {
        e.interp = match (e.kind, e.comp) {
            (3, _) => 4,
            (0 | 6 | 7, 9) => 2,
            (0, _) => 1,
            _ => 0,
        };
        e.usage = (1 << e.cols) - 1;
    }
    let mut s = Shader::new(false);
    s.geometry = Some(Geometry {
        max_vertices: VERTICES,
    });
    s.inputs = outputs.clone();
    let wanted = ps_inputs
        .iter()
        .find(|e| e.kind == 10 && e.row.is_some())
        .filter(|_| !outputs.iter().any(|o| o.kind == 10));
    let primitive = match wanted {
        Some(e) => {
            let clash = outputs.iter().any(|o| {
                let overlap = o.col < e.col + e.cols && e.col < o.col + o.cols;
                o.row == e.row && (o.interp != 1 || overlap)
            });
            if clash {
                return Err(unsupported(
                    "a rectangle list whose primitive id shares a register with a vertex output",
                ));
            }
            outputs.push(SigElement {
                interp: 1,
                usage: (1 << e.cols) - 1,
                ..e.clone()
            });
            Some(outputs.len() - 1)
        }
        None => None,
    };
    let scalars: u32 = outputs.iter().map(|e| u32::from(e.cols)).sum();
    if scalars * VERTICES > MAX_SCALARS {
        return Err(unsupported(
            "a rectangle list with more output components than a geometry shader emits",
        ));
    }
    s.outputs = outputs;
    let b = s.entry;

    let mut corners: Vec<Vec<[V; 3]>> = Vec::new();
    for id in 0..s.inputs.len() {
        let mut columns = Vec::new();
        for col in 0..s.inputs[id].cols {
            columns.push([0, 1, 2].map(|vertex| load(&mut s, id, col, vertex)));
        }
        corners.push(columns);
    }

    let (x, y, w) = (
        corners[position][0],
        corners[position][1],
        corners[position][3],
    );
    let zero = s.m.f32(0);
    let mut plain = s.m.cmp(b, pred::OGT, w[0], zero);
    for &other in &w[1..] {
        let one_w = same(&mut s, other, w[0]);
        plain = both(&mut s, plain, one_w);
    }
    for (i, j) in [(0, 1), (0, 2), (1, 2)] {
        let (same_x, same_y) = (same(&mut s, x[i], x[j]), same(&mut s, y[i], y[j]));
        let together = both(&mut s, same_x, same_y);
        let apart = not(&mut s, together);
        plain = both(&mut s, plain, apart);
    }
    let mut corner: Vec<V> = Vec::new();
    for i in 0..3 {
        let (a, c) = ((i + 1) % 3, (i + 2) % 3);
        let (xa, yc) = (same(&mut s, x[i], x[a]), same(&mut s, y[i], y[c]));
        let (xc, ya) = (same(&mut s, x[i], x[c]), same(&mut s, y[i], y[a]));
        let (one, other) = (both(&mut s, xa, yc), both(&mut s, xc, ya));
        let shares = either(&mut s, one, other);
        let mut here = both(&mut s, shares, plain);
        for &found in &corner {
            let earlier = not(&mut s, found);
            here = both(&mut s, here, earlier);
        }
        corner.push(here);
    }

    let mut emitted: Vec<Vec<[V; 6]>> = Vec::new();
    for (id, columns) in corners.iter().enumerate() {
        let float = s.inputs[id].comp == 9;
        let mut out = Vec::new();
        for &[p0, p1, p2] in columns {
            let mut fourth = p0;
            if float {
                for (k, (a, c, at)) in [(p1, p2, p0), (p2, p0, p1), (p0, p1, p2)]
                    .into_iter()
                    .enumerate()
                {
                    let sum = s.m.bin(b, Bin::Add, a, c);
                    let opposite = s.m.bin(b, Bin::Sub, sum, at);
                    fourth = s.m.select(b, corner[k], opposite, fourth);
                }
            }
            let third = s.m.select(b, corner[0], fourth, p2);
            let fifth = s.m.select(b, corner[1], p2, fourth);
            let sixth = s.m.select(b, corner[0], p2, p1);
            let sixth = s.m.select(b, corner[1], fourth, sixth);
            out.push([p0, p1, third, p0, fifth, sixth]);
        }
        emitted.push(out);
    }

    let mut number = None;
    if let Some(id) = primitive {
        let n = s.op(b, "primitiveID.i32", Attr::ReadNone, s.t.i32, 108, &[]);
        number = Some((id, n));
    }
    let stream = s.m.i8(0);
    for vertex in 0..VERTICES as usize {
        for (id, columns) in emitted.iter().enumerate() {
            let name = if s.outputs[id].comp == 9 {
                "storeOutput.f32"
            } else {
                "storeOutput.i32"
            };
            for (col, values) in columns.iter().enumerate() {
                let args = [
                    s.m.i32(id as u32),
                    s.m.i32(0),
                    s.m.i8(col as u8),
                    values[vertex],
                ];
                s.op(b, name, Attr::NoUnwind, s.t.void, 5, &args);
            }
        }
        if let Some((id, n)) = number {
            let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(0), n];
            s.op(b, "storeOutput.i32", Attr::NoUnwind, s.t.void, 5, &args);
        }
        s.op(b, "emitStream", Attr::NoUnwind, s.t.void, 97, &[stream]);
        if vertex == 2 {
            s.op(b, "cutStream", Attr::NoUnwind, s.t.void, 98, &[stream]);
        }
    }
    s.m.terminate(b, Term::Ret);
    Ok(s.finish())
}

fn load(s: &mut Shader, id: usize, col: u8, vertex: u32) -> V {
    let (name, ty) = if s.inputs[id].comp == 9 {
        ("loadInput.f32", s.t.f32)
    } else {
        ("loadInput.i32", s.t.i32)
    };
    let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(col), s.m.i32(vertex)];
    let entry = s.entry;
    s.op(entry, name, Attr::ReadNone, ty, 4, &args)
}

fn same(s: &mut Shader, a: V, b: V) -> V {
    let entry = s.entry;
    s.m.cmp(entry, pred::OEQ, a, b)
}

fn both(s: &mut Shader, a: V, b: V) -> V {
    let entry = s.entry;
    s.m.bin(entry, Bin::And, a, b)
}

fn either(s: &mut Shader, a: V, b: V) -> V {
    let entry = s.entry;
    s.m.bin(entry, Bin::Or, a, b)
}

fn not(s: &mut Shader, a: V) -> V {
    let (entry, on) = (s.entry, s.m.bool(true));
    s.m.bin(entry, Bin::Xor, a, on)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str, index: u32, kind: u8, comp: u8, row: u32, cols: u8) -> SigElement {
        SigElement {
            name: name.into(),
            index,
            kind,
            comp,
            interp: 0,
            row: Some(row),
            col: 0,
            cols,
            usage: 0,
            half: false,
        }
    }

    fn outputs() -> Vec<SigElement> {
        vec![
            element("SV_Position", 0, 3, 9, 0, 4),
            element("TEXCOORD", 0, 0, 9, 1, 4),
            element("TEXCOORD", 1, 0, 5, 2, 2),
        ]
    }

    #[test]
    fn the_rectangle_stage_is_a_geometry_shader() {
        let wanted = [element("SV_PrimitiveID", 0, 10, 5, 3, 1)];
        let gs = build(outputs(), &wanted).unwrap();
        let c = Container::parse(&gs).unwrap();
        assert_eq!(c.part(FourCC::DXIL).unwrap().data[..4], [0x66, 0, 2, 0]);
        let psv = c.part(FourCC::PSV0).unwrap().data;
        assert_eq!(psv[4..17], [3, 0, 0, 0, 5, 0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(psv[28..34], [2, 0, 6, 0, 3, 4]);
        let sig = |fourcc| super::super::signature(c.part(fourcc).unwrap().data).unwrap();
        let (ins, outs) = (sig(FourCC::ISG1), sig(FourCC::OSG1));
        assert_eq!(ins.len(), 3);
        assert_eq!(outs.len(), 4);
        for (i, o) in ins.iter().zip(&outs) {
            assert_eq!((&i.name, i.index, i.row), (&o.name, o.index, o.row));
            assert_eq!((i.col, i.cols, i.comp), (o.col, o.cols, o.comp));
        }
        assert_eq!((outs[3].kind, outs[3].row), (10, Some(3)));
        #[cfg(windows)]
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut gs.clone()).unwrap();
            let plain = build(outputs(), &[]).unwrap();
            v.validate(&mut plain.clone()).unwrap();
        }
    }

    #[test]
    fn rectangle_stages_that_cannot_be_made_are_refused() {
        assert!(build(outputs()[1..].to_vec(), &[]).is_err());
        let taken = [element("SV_PrimitiveID", 0, 10, 5, 1, 1)];
        assert!(build(outputs(), &taken).is_err());
        let mut many = vec![element("SV_Position", 0, 3, 9, 0, 4)];
        many.extend((1..=43).map(|row| element("TEXCOORD", row - 1, 0, 9, row, 4)));
        assert!(build(many, &[]).is_err());
    }

    #[cfg(windows)]
    fn pass_through() -> (super::super::HostShader, super::super::HostShader) {
        let position = SigElement {
            interp: 4,
            usage: 15,
            ..element("SV_Position", 0, 3, 9, 0, 4)
        };
        let coord = SigElement {
            interp: 2,
            usage: 15,
            ..element("TEXCOORD", 0, 0, 9, 1, 4)
        };
        let mut vs = Shader::new(false);
        vs.inputs = vec![
            SigElement {
                usage: 15,
                ..element("POSITION", 0, 0, 9, 0, 4)
            },
            SigElement {
                interp: 0,
                ..coord.clone()
            },
        ];
        vs.outputs = vec![position.clone(), coord.clone()];
        let mut ps = Shader::new(true);
        ps.inputs = vec![
            SigElement {
                usage: 0,
                ..position
            },
            coord,
        ];
        ps.outputs = vec![SigElement {
            usage: 15,
            ..element("SV_Target", 0, 16, 9, 0, 4)
        }];
        for (s, copies) in [(&mut vs, &[(0, 0), (1, 1)][..]), (&mut ps, &[(1, 0)][..])] {
            let b = s.entry;
            for &(from, to) in copies {
                for col in 0..4 {
                    let args = [s.m.i32(from), s.m.i32(0), s.m.i8(col), s.m.undef(s.t.i32)];
                    let v = s.op(b, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args);
                    let args = [s.m.i32(to), s.m.i32(0), s.m.i8(col), v];
                    s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
                }
            }
            s.m.terminate(b, Term::Ret);
        }
        let host = |stage, s: Shader| super::super::HostShader {
            stage,
            dxil: s.finish(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            root_indexed_views: Vec::new(),
        };
        (
            host(crate::native::HwStage::Vs, vs),
            host(crate::native::HwStage::Ps, ps),
        )
    }

    #[test]
    #[cfg(windows)]
    fn rectangles_are_drawn_whole() {
        use super::super::host::{Draw, Host};
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 device; skipped");
            return;
        };
        let (vs, ps) = pass_through();
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut vs.dxil.clone()).unwrap();
            v.validate(&mut ps.dxil.clone()).unwrap();
        }
        let gs = rect_list_gs(&vs.dxil, Some(&ps.dxil[..])).unwrap();
        let root = host.root_signature(&vs, &ps).unwrap();
        let vertex = |(x, y): (f32, f32)| [x, y, 0.5, 1.0, x, y, 0.0, 1.0];
        for (name, corners) in [
            ("corner first", [(-1.0, 1.0), (-1.0, -1.0), (1.0, 1.0)]),
            ("corner second", [(-1.0, 1.0), (-1.0, -1.0), (1.0, -1.0)]),
            ("corner third", [(-1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]),
        ] {
            let vertices = corners.map(vertex).concat();
            let draw = |gs: Option<&[u8]>| {
                let pso = host
                    .pipeline_with_gs(&root, &vs.dxil, gs, &ps.dxil)
                    .unwrap();
                let d = Draw {
                    roots: Vec::new(),
                    textures: &[],
                    samplers: &[],
                    vertices: &vertices,
                    stride: 32,
                    size: (8, 8),
                    mesh_groups: 0,
                };
                host.draw(&root, &pso, &d).unwrap()
            };
            let whole = draw(Some(&gs[..]));
            for (i, texel) in whole.iter().enumerate() {
                let (x, y) = ((i % 8) as f32, (i / 8) as f32);
                let want = [(x + 0.5) / 4.0 - 1.0, 1.0 - (y + 0.5) / 4.0, 0.0, 1.0];
                assert!(
                    texel.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-4),
                    "{name}: texel {i} is {texel:?}, not {want:?}"
                );
            }
            let half = draw(None);
            let missing = half.iter().zip(&whole).filter(|(a, b)| a != b).count();
            assert!(
                missing >= 24,
                "{name}: only {missing} texels need the stage"
            );
        }
    }
}
