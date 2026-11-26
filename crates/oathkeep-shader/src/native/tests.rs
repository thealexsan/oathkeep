use crate::dxbc::u64_at;
use super::parse::unpack;
use super::format::*;
use super::*;

const VS_CODE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
const PS_CODE: [u8; 4] = [9, 10, 11, 12];
const VGPRS: u32 = 28;

fn slot(stage: HwStage) -> usize {
    SLOTS.iter().find(|s| s.0 == stage).unwrap().1
}

/// Stage data: no semantics, and every export patch absent.
const STAGE_DATA: usize = PS_PATCHES + MRTS * PATCH_BYTES;

fn empty_stage_data() -> Vec<u8> {
    let mut data = vec![0; STAGE_DATA];
    for mrt in 0..MRTS {
        let at = PS_PATCHES + mrt * PATCH_BYTES;
        data[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    }
    data
}

/// Where a one-program blob's stage data starts.
fn stage_data_at(blob: &[u8], stage: HwStage) -> usize {
    FSSM_HEADER
        + u64_at(blob, FSSM_HEADER + slot(stage)).unwrap() as usize
        + RECORD_SIZE as usize
}

/// FSSM blob with `programs` as (stage, code) pairs.
fn fssm(programs: &[(HwStage, &[u8])]) -> Vec<u8> {
    let mut payload = GRAPHICS_SLOT_COUNT.to_le_bytes().to_vec();
    payload.resize(slot(HwStage::Cs) + 8, 0);
    for (_, at) in SLOTS {
        payload[at..at + 8].copy_from_slice(&ABSENT.to_le_bytes());
    }
    for &(stage, code) in programs {
        let slot = slot(stage);
        let record = payload.len() as u64;
        payload[slot..slot + 8].copy_from_slice(&record.to_le_bytes());
        let code_at = record + RECORD_SIZE + STAGE_DATA as u64;
        payload.extend_from_slice(&code_at.to_le_bytes());
        payload.extend_from_slice(&(code.len() as u64).to_le_bytes());
        let flags = if stage == HwStage::Ps { FLAG_WAVE32 } else { 0 };
        payload.extend_from_slice(&[0; 4]);
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.resize(record as usize + RECORD_REGS, 0);
        payload.extend_from_slice(&(VGPRS | 31 << 20).to_le_bytes());
        payload.extend_from_slice(&empty_stage_data());
        payload.extend_from_slice(code);
    }
    let mut out = b"FSSM".to_vec();
    out.extend_from_slice(&FSSM_VERSION.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

fn xbch(unpacked: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; zlib_rs::compress_bound(unpacked.len())];
    let (stream, rc) =
        zlib_rs::compress_slice(&mut buf, unpacked, zlib_rs::DeflateConfig::new(6));
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    let mut out = b"XBCH".to_vec();
    out.extend_from_slice(&XBCH_VERSION.to_le_bytes());
    out.extend_from_slice(&(stream.len() as u64).to_le_bytes());
    out.extend_from_slice(&(unpacked.len() as u64).to_le_bytes());
    out.extend_from_slice(stream);
    out
}

/// `DXIL` part data: program header, XDAT index, then the blobs.
fn part(blobs: &[(u32, &[u8])]) -> Vec<u8> {
    let mut out = vec![0u8; XDAT_OFFSET];
    out.extend_from_slice(b"XDAT");
    out.extend_from_slice(&(blobs.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x24u32.to_le_bytes());
    let mut offset = out.len() + 12 * blobs.len();
    for (kind, blob) in blobs {
        for v in [*kind, offset as u32, blob.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        offset += blob.len();
    }
    for (_, blob) in blobs {
        out.extend_from_slice(blob);
    }
    out
}

#[test]
fn finds_graphics_programs() {
    let native = xbch(&fssm(&[(HwStage::Vs, &VS_CODE), (HwStage::Ps, &PS_CODE)]));
    let data = part(&[(0x0a, b"xbop"), (KIND_NATIVE, &native)]);
    let found = native_programs(&data).unwrap();
    assert_eq!(
        found,
        [
            NativeProgram {
                stage: HwStage::Vs,
                wave64: true,
                vgprs: 28,
                layout: vec![],
                used: vec![],
                io: StageIo::Vs {
                    inputs: vec![],
                    outputs: vec![],
                },
                code: VS_CODE.to_vec()
            },
            NativeProgram {
                stage: HwStage::Ps,
                wave64: false,
                vgprs: 28,
                layout: vec![],
                used: vec![],
                io: StageIo::Ps {
                    inputs: vec![],
                    input_addr: 0,
                    db_shader_control: 0,
                    patches: vec![],
                },
                code: PS_CODE.to_vec()
            },
        ]
    );
}

#[test]
fn reads_root_tables() {
    let mut blob = fssm(&[(HwStage::Ps, &PS_CODE)]);
    let record = FSSM_HEADER + u64_at(&blob, FSSM_HEADER + slot(HwStage::Ps)).unwrap() as usize;
    let tables = (blob.len() - FSSM_HEADER) as u64;
    // A table range in s9 (root slot 1), and an SRV read from it.
    for v in [3u32, 2, 1, 5] {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    blob.extend_from_slice(&[9, 1, 0, 0, 0, 3, 0, 0]);
    for v in [2u32, 5, 4] {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    blob.extend_from_slice(&[8, 9, 1, 0]);
    let payload = (blob.len() - FSSM_HEADER) as u64;
    blob[8..16].copy_from_slice(&payload.to_le_bytes());
    blob[record + RECORD_LAYOUT..][..8].copy_from_slice(&tables.to_le_bytes());
    blob[record + RECORD_USED..][..8].copy_from_slice(&(tables + ENTRY_SIZE).to_le_bytes());
    blob[record + RECORD_COUNTS..][..8].copy_from_slice(&[1, 0, 0, 0, 1, 0, 0, 0]);

    let p = &native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).unwrap()[0];
    assert_eq!(
        p.layout,
        [RootParam {
            root: RootType::Table,
            table_offset_dwords: 2,
            sgprs: 1,
            base_register: 5,
            user_data: 9,
            slot: 1
        }]
    );
    assert_eq!(
        p.used,
        [UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 2,
            lower_bound: 5,
            stride: 4,
            dwords: 8,
            user_data: 9,
            slot: 1
        }]
    );

    blob[record + RECORD_COUNTS] = 9;
    assert!(native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).is_err());
}

#[test]
fn reads_pixel_io_and_applies_the_full_export_patch() {
    let code = [0xBF80_0000u32; 4]; // s_nop
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut blob = fssm(&[(HwStage::Ps, &bytes)]);
    let e = stage_data_at(&blob, HwStage::Ps);
    // TEXCOORD in row 1 (xy), read as attr0, flat.
    blob[e + PS_INPUTS..][..8].copy_from_slice(&[6, 1, 3, 0, 0, 0, 1, 1]);
    blob[e + PS_INPUT_COUNT..][..4].copy_from_slice(&1u32.to_le_bytes());
    blob[e + PS_INPUT_ADDR..][..4].copy_from_slice(&2u32.to_le_bytes());
    blob[e + PS_PATCH_SIZE..][..4].copy_from_slice(&2u32.to_le_bytes());
    // MRT1 at dword 1: the 32_ABGR variant is `exp mrt1 v0-v3 done`.
    let patch = e + PS_PATCHES + PATCH_BYTES;
    blob[patch..][..4].copy_from_slice(&1u32.to_le_bytes());
    let full = patch + 4 + PATCH_FULL * PATCH_SLOT_DWORDS * 4;
    blob[full..][..8].copy_from_slice(&[0x1f, 0x18, 0, 0xf8, 0, 1, 2, 3]);

    let p = &native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).unwrap()[0];
    let StageIo::Ps {
        inputs,
        input_addr,
        patches,
        ..
    } = &p.io
    else {
        panic!("{:?}", p.io)
    };
    let input = PsInput {
        usage: Usage::Generic,
        row: 1,
        mask: 3,
        attr: 0,
        flat: true,
        fp16: false,
        pack2: false,
        row2: 0,
    };
    assert_eq!((&inputs[..], *input_addr), (&[input][..], 2));
    assert_eq!(patches.len(), 1);
    assert_eq!((patches[0].mrt, patches[0].offset), (1, 1));
    let patched = p.patched_code();
    assert_eq!(patched[..4], p.code[..4]);
    assert_eq!(patched[4..12], [0x1f, 0x18, 0, 0xf8, 0, 1, 2, 3]);
    assert_eq!(patched[12..], p.code[12..]);

    blob[patch..][..4].copy_from_slice(&3u32.to_le_bytes());
    assert!(native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).is_err());
}

#[test]
fn finds_mesh_and_compute_programs() {
    for stage in [HwStage::Ms, HwStage::Cs] {
        let data = part(&[(KIND_NATIVE, &xbch(&fssm(&[(stage, &VS_CODE)])))]);
        let found = native_programs(&data).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].stage, stage);
    }
}

#[test]
fn reads_mesh_outputs_and_limits() {
    let mut blob = fssm(&[(HwStage::Ms, &VS_CODE)]);
    let e = stage_data_at(&blob, HwStage::Ms);
    // param0 is row 1 per vertex, param2 row 0 of the primitive signature.
    blob[e + VS_OUTPUTS..][..12].copy_from_slice(&[6, 1, 0, 0, 0, 0, 6, 0, 0x82, 0, 0, 0]);
    blob[e + VS_OUTPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
    blob[e + MS_CLIP_CNTL..][..4].copy_from_slice(&0x0040_000Fu32.to_le_bytes());
    // 128 lanes, 128 vertices, 64 primitives, 13 user SGPRs, triangles.
    blob[e + MS_INFO..][..8].copy_from_slice(&[0x80, 0, 0x80, 0, 0x40, 0, 13, 2]);
    let record = e - RECORD_SIZE as usize;
    blob[record + RECORD_LDS..][..4].copy_from_slice(&16u32.to_le_bytes());
    let p = &native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).unwrap()[0];
    let out = |row, param| VsOutput {
        usage: Usage::Generic,
        row,
        param,
        fp16: false,
        pack2: false,
    };
    assert_eq!(
        p.io,
        StageIo::Ms(MeshIo {
            outputs: vec![out(1, 0), out(0, 0x82)],
            clip_cntl: 0x0040_000F,
            threads: 128,
            max_vertices: 128,
            max_primitives: 64,
            topology: 2,
            lds_bytes: 16,
        })
    );
}

#[test]
fn mesh_stage_io_ignores_unused_vs_input_count() {
    let mut blob = fssm(&[(HwStage::Ms, &VS_CODE)]);
    let e = stage_data_at(&blob, HwStage::Ms);
    // Mesh stage data has no vertex inputs; E+0xc4 is unused by MS.
    blob[e + VS_INPUT_COUNT..][..4].copy_from_slice(&u32::MAX.to_le_bytes());
    let p = &native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).unwrap()[0];
    assert!(matches!(p.io, StageIo::Ms(_)));
}

#[test]
fn compute_dispatch_settings() {
    // 5 user SGPRs, group id x and y, thread ids in v0-v1, 1 KiB LDS.
    let cs = ComputeIo {
        rsrc2: 0x1098A,
        threads: [16, 16, 1],
    };
    assert_eq!(cs.user_sgprs(), 5);
    assert_eq!(cs.system_sgprs(), ([Some(5), Some(6), None], None));
    assert_eq!((cs.thread_id_vgprs(), cs.lds_bytes()), (2, 1024));
    // 6 user SGPRs, all three group ids, v0-v2, no LDS.
    let cs = ComputeIo {
        rsrc2: 0x138C,
        threads: [8, 8, 1],
    };
    assert_eq!(cs.system_sgprs(), ([Some(6), Some(7), Some(8)], None));
    assert_eq!((cs.thread_id_vgprs(), cs.lds_bytes()), (3, 0));
}

#[test]
fn initialized_sgprs_covers_user_system_and_scratch() {
    // 5 user SGPRs, group ids x and y (s5, s6), no scratch: s0..=s6.
    let cs = ComputeIo {
        rsrc2: 0x1098A,
        threads: [16, 16, 1],
    };
    assert_eq!(cs.initialized_sgprs(), 7);
    // All three group ids after 6 user SGPRs, plus the info SGPR: s0..=s9.
    let cs = ComputeIo {
        rsrc2: 0x78C,
        threads: [8, 8, 1],
    };
    assert_eq!(cs.initialized_sgprs(), 10);
    // The same with scratch enabled adds the wave offset: s0..=s10.
    let cs = ComputeIo {
        rsrc2: 0x78D,
        threads: [8, 8, 1],
    };
    assert_eq!(cs.initialized_sgprs(), 11);
}

#[test]
fn reads_every_native_blob() {
    let es = xbch(&fssm(&[(HwStage::Es, &VS_CODE)]));
    let vs = xbch(&fssm(&[(HwStage::Vs, &VS_CODE)]));
    let data = part(&[(0x12, &[0; 16]), (KIND_NATIVE, &es), (KIND_NATIVE, &vs)]);
    let stages: Vec<_> = native_programs(&data)
        .unwrap()
        .iter()
        .map(|p| p.stage)
        .collect();
    assert_eq!(stages, [HwStage::Es, HwStage::Vs]);
}

#[test]
fn pc_layout_has_none() {
    assert!(native_programs(&[0u8; 64]).unwrap().is_empty());
    assert!(
        native_programs(&part(&[(0x0a, b"xbop")]))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn rejects_corrupt_blobs() {
    let good = xbch(&fssm(&[(HwStage::Ls, &VS_CODE)]));
    let corrupt = |at: usize, byte: u8| {
        let mut blob = good.clone();
        blob[at] = byte;
        native_programs(&part(&[(KIND_NATIVE, &blob)]))
    };
    assert!(corrupt(4, 9).is_err(), "unknown version");
    assert!(corrupt(16, 0xFF).is_err(), "stated size");
    assert!(corrupt(XBCH_HEADER + 2, 0xFF).is_err(), "zlib stream");
    assert!(native_programs(&part(&[(KIND_NATIVE, &good[..20])])).is_err());

    let mut bad_record = fssm(&[(HwStage::Ls, &VS_CODE)]);
    let at = FSSM_HEADER + slot(HwStage::Ls);
    bad_record[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(native_programs(&part(&[(KIND_NATIVE, &xbch(&bad_record))])).is_err());
}

#[test]
fn rejects_truncated_oversized_and_bad_checksum_xbch_streams() {
    let plain = fssm(&[(HwStage::Vs, &VS_CODE)]);
    let good = xbch(&plain);
    assert_eq!(unpack(&good).unwrap(), plain);

    // Truncated zlib stream (drop last byte, update packed length).
    let mut truncated = good[..good.len() - 1].to_vec();
    let new_packed = (truncated.len() - XBCH_HEADER) as u64;
    truncated[8..16].copy_from_slice(&new_packed.to_le_bytes());
    assert!(unpack(&truncated).is_err());

    // Stated unpacked size smaller than the inflated output.
    let mut oversized = good.clone();
    oversized[16..24].copy_from_slice(&((plain.len() - 1) as u64).to_le_bytes());
    assert!(unpack(&oversized).is_err());

    // Stated unpacked size larger than the inflated output.
    let mut underfilled = good.clone();
    underfilled[16..24].copy_from_slice(&((plain.len() + 1) as u64).to_le_bytes());
    assert!(unpack(&underfilled).is_err());

    // Corrupt Adler-32 trailer.
    let mut bad_adler = good.clone();
    *bad_adler.last_mut().unwrap() ^= 1;
    assert!(unpack(&bad_adler).is_err());
}
