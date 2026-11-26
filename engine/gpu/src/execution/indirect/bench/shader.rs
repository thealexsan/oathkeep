use super::super::*;
use windows::core::PCSTR;
/// Draws a frame (the title's gameplay frames hold about 3,200 indirect
/// executions, #107), records a window holds, and how far apart the
/// records are (the title keeps them 32 bytes apart).
pub(super) const DRAWS: u32 = 3200;
pub(super) const WINDOW: u32 = 32;
pub(super) const STEP: u32 = 32;
pub(super) const RUNS: usize = 9;

const SHADERS: &str = r#"
#define RS "RootFlags(0)"
[RootSignature(RS)]
float4 vs(uint v : SV_VertexID, uint i : SV_InstanceID) : SV_Position {
float2 p = float2((v & 1) ? 0.05 : 0.0, (v & 2) ? 0.05 : 0.0) - 0.5;
return float4(p, 0.5, 1);
}
float4 ps() : SV_Target { return float4(1, 0.5, 0.25, 1); }
"#;

pub(super) fn compile(entry: &str, target: &str) -> Vec<u8> {
    let entry = std::ffi::CString::new(entry).unwrap();
    let target = std::ffi::CString::new(target).unwrap();
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: the source and names outlive the call.
    unsafe {
        D3DCompile(
            SHADERS.as_ptr().cast(),
            SHADERS.len(),
            s!("bench"),
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            0,
            0,
            &mut code,
            None,
        )
        .unwrap();
        let code = code.unwrap();
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
            .to_vec()
    }
}

/// The clamp as it was before it charged the budget once per group
/// (record by record, every thread racing for the budget word), kept
/// here to measure against.
pub(super) const OLD_SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), SRV(t1), UAV(u0), UAV(u1)"
cbuffer Record : register(b0) {
uint kind;
uint stride;
uint records;
uint args_offset;
// Bit 0: a count buffer bounds the records. Bit 1: the records are a
// window clamped ahead of their draws: one that asks for more than can
// run is left empty and costs nothing.
uint has_count;
uint count_offset;
};
ByteAddressBuffer src_args : register(t0);
ByteAddressBuffer count_buf : register(t1);
RWByteAddressBuffer dst_args : register(u0);
// The list's work so far (vertex instances, threads): past the budget,
// records run empty.
RWByteAddressBuffer budget : register(u1);
static const uint BUDGET = 1u << 27;
// Groups of one dispatch; more run empty.
static const uint GROUPS = 1u << 20;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
uint eff_count = records;
bool ahead = (has_count & 2u) != 0;
if ((has_count & 1u) != 0) {
    eff_count = min(eff_count, count_buf.Load(count_offset));
}
if (id.x >= eff_count) return;
uint src_at = args_offset + id.x * stride;
uint dst_at = id.x * stride;
uint cost;
uint2 draw_counts = uint2(0, 0);
uint3 groups = uint3(0, 0, 0);
if (kind <= 1) {
    // Draws: vertex or index count, then instances.
    uint2 asked = src_args.Load2(src_at);
    if (ahead && (asked.x > (1u << 24) || asked.y > (1u << 24))) {
        asked = uint2(0, 0);
    }
    uint count = min(asked.x, 1u << 24);
    uint instances = min(asked.y, (1u << 26) / max(count, 1u));
    cost = count * instances;
    draw_counts = uint2(count, instances);
} else if (kind == 2) {
    uint3 g = min(src_args.Load3(src_at), uint3(65535u, 65535u, 65535u));
    // x * y fits 32 bits; the whole count may not.
    if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
        g = uint3(0, 0, 0);
    }
    cost = g.x * g.y * g.z * 64u;
    groups = g;
} else {
    // Mesh dispatches: the console's are one-dimensional.
    uint3 g = src_args.Load3(src_at);
    // Ahead of its draw, a record over the most a dispatch runs is a
    // slot the title has not filled (the title leaves all
    // ones there): empty, and not counted below either.
    if (ahead && g.x > 65535u) {
        g = uint3(0, 0, 0);
    }
    // Diagnostics, in the budget buffer's next words: the largest
    // group count a mesh record asked for, and the groups asked in all.
    uint was;
    budget.InterlockedMax(4, g.x, was);
    budget.InterlockedAdd(8, min(g.x, 1u << 24), was);
    g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
    cost = g.x * g.y * g.z * 128u;
    groups = g;
}
bool over = false;
if (cost > 0) {
    uint before;
    budget.InterlockedOr(0, 0, before);
    [allow_uav_condition]
    while (before < BUDGET) {
        uint next = (BUDGET - before <= cost) ? BUDGET : (before + cost);
        uint prev;
        budget.InterlockedCompareExchange(0, before, next, prev);
        if (prev == before) break;
        before = prev;
    }
    over = before >= BUDGET || BUDGET - before < cost;
}
if (kind <= 1) {
    if (over) {
        draw_counts = uint2(0, 0);
    }
    dst_args.Store2(dst_at, draw_counts);
    for (uint off = 8; off < stride; off += 4) {
        dst_args.Store(dst_at + off, src_args.Load(src_at + off));
    }
} else {
    if (over) {
        groups = uint3(0, 0, 0);
    }
    dst_args.Store3(dst_at, groups);
    for (uint off = 12; off < stride; off += 4) {
        dst_args.Store(dst_at + off, src_args.Load(src_at + off));
    }
}
}
"#;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Barriers {
    /// Back to the common state after every execution (as before).
    Old,
    /// Readable as arguments until the next clamp, common at the end.
    New,
}
