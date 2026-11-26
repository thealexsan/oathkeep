use crate::dxbc::{Container, FourCC};
use crate::emit::{emit, validator};
use crate::native::{ComputeIo, RootParam, HwStage, NativeProgram, RootType, StageIo};
use crate::{io, ir, resource, structure};

#[test]

fn heap_texture_atomic_widths() {
    // A synthetic compute shader loads a 2D UAV from a root table,
    // compares or adds at (0, 0), then uses the returned register(s).
    for (opcode, mask, wide) in [
        (16u32, 3, false),
        (16, 15, true),
        (17, 1, false),
        (17, 3, true),
    ] {
        let words = [
            0xBE81_0380, // s_mov_b32 s1, 0 (table address high)
            0xF40C_0200,
            0xFA00_0000, // s_load_dwordx8 s[8:15], s[0:1], 0
            0x7E00_0280,
            0x7E02_0280, // v0, v1 = coordinates
            0x7E04_0281,
            0x7E06_0282,
            0x7E08_0283,
            0x7E0A_0284, // v2..v5 = 1..4
            0xF000_2008 | opcode << 18 | mask << 8,
            0x0002_0200,
            // Use the old value in a second atomic at the same location.
            0xF044_0008 | (if wide { 3 } else { 1 }) << 8,
            0x0002_0200,
            0xBF81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 6,
            layout: vec![RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            }],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 2,
                threads: [32, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = ir::build(&native.code, false, native.vgprs).unwrap();
        let bytes = crate::dxbc::build(&[]);
        let shader = emit(
            &Container::parse(&bytes).unwrap(),
            &native,
            &p,
            &structure::structure(&p).unwrap(),
            &io::raise(&p, &native).unwrap(),
            &resource::track(&p, &native.layout, native.stage, None).unwrap(),
        )
        .unwrap();
        let container = Container::parse(&shader.dxil).unwrap();
        let features = u64::from_le_bytes(
            container
                .part(FourCC::SFI0)
                .unwrap()
                .data
                .try_into()
                .unwrap(),
        );
        let required = 0x1040_8000; // heap atomics, typed atomics, Int64Ops
        assert_eq!(features & required, if wide { required } else { 0 });
        #[cfg(windows)]
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            v.validate(&mut shader.dxil.clone()).unwrap();
        }
    }
}
