//! Differential tests: HLSL compiled by the console's shader compiler and
//! translated, against the same HLSL compiled for PC, both drawn (or
//! dispatched) on the host GPU with the same inputs.
//!
//! Needs `OATHKEEP_CONSOLE_DXC` (the console SDK's `dxc.exe`) and a PC
//! `dxc.exe` (`OATHKEEP_PC_DXC`, else the newest Windows SDK's); skipped
//! without them. Set `OATHKEEP_WARP=1` to draw on WARP.





pub(super) const CS_RS: &str = "RootConstants(num32BitConstants=4, b0), \
                     DescriptorTable(SRV(t0, numDescriptors=4)), DescriptorTable(Sampler(s0)), \
                     DescriptorTable(UAV(u0, numDescriptors=4)), CBV(b1), SRV(t4), \
                     DescriptorTable(SRV(t0, space=2, numDescriptors=9)), \
\
                     StaticSampler(s3, filter=FILTER_COMPARISON_MIN_MAG_LINEAR_MIP_POINT, \
                         comparisonFunc=COMPARISON_LESS_EQUAL, \
                         addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)";

pub(super) const CS_COMMON: &str = r#"
cbuffer K : register(b0) { float4 k; };
Texture2D tex : register(t0);
Buffer<float4> typed : register(t1);
struct S { float a; uint b; float2 c; };
StructuredBuffer<S> structured : register(t2);
ByteAddressBuffer raw : register(t3);
SamplerState smp : register(s0);
SamplerComparisonState cmp : register(s3);
RWByteAddressBuffer outb : register(u0);
RWTexture2D<float4> outt : register(u1);
cbuffer R : register(b1) { float4 table[16]; };
ByteAddressBuffer rootraw : register(t4);
// `raw`'s bytes in typed views (the formats the console's typed loads name).
Buffer<float4> unorm8x4 : register(t0, space2);
Buffer<float4> snorm16x2 : register(t1, space2);
Buffer<uint4> uint8x4 : register(t2, space2);
Buffer<float4> unorm10x3_2 : register(t3, space2);
Buffer<uint4> uint16x4 : register(t4, space2);
Buffer<float> unorm8 : register(t5, space2);
Buffer<float> half16 : register(t6, space2);
Buffer<float4> snorm8x4 : register(t7, space2);
RWTexture2D<uint> ut : register(u2);
TextureCube cubet : register(t8, space2);
#define FMT(numeric, data) ((numeric) << 4 | (data))
#define CS [RootSignature(RS)] [numthreads(8, 8, 1)] \
    void cs(uint3 g : SV_GroupID, uint3 t : SV_GroupThreadID, \
            uint3 d : SV_DispatchThreadID, uint i : SV_GroupIndex)
"#;

/// Compute shaders, each `cs`, dispatched as 2×2 groups of 8×8 threads
/// over a 16×16 texture and a 256-dword buffer.
pub(super) const CS_CASES: &[(&str, &str)] = &[
    (
        "ids and stores",
        r#"CS {
            outb.Store(4 * (d.y * 16 + d.x), asuint(d.x * k.x + d.y));
            outt[d.xy] = float4(g.x, g.y, t.x, t.y + i);
        }"#,
    ),
    (
        "shared memory and barrier",
        r#"groupshared float lds[64];
        groupshared uint cnt;
        CS {
            if (i == 0) cnt = 0;
            lds[i] = d.x * k.x + d.y;
            GroupMemoryBarrierWithGroupSync();
            InterlockedAdd(cnt, i);
            float v = lds[63 - i] + lds[(i + 9) & 63];
            GroupMemoryBarrierWithGroupSync();
            outb.Store(4 * (d.y * 16 + d.x), asuint(v));
            outt[d.xy] = float4(v, cnt, 0, 1);
        }"#,
    ),
    (
        "sample and divergent store",
        r#"CS {
            float4 s = tex.SampleLevel(smp, (d.xy + 0.5) / 16.0, 0);
            if ((d.x ^ d.y) & 1) outt[d.xy] = s * k.y;
            else outb.Store(4 * (d.y * 16 + d.x), asuint(s.x + s.w));
        }"#,
    ),
    (
        "buffers and root views",
        r#"CS {
            float4 a = typed[i & 15];
            S s = structured[(i * 3) & 7];
            uint r = raw.Load(4 * (i & 31));
            float4 c = table[d.x & 15];
            uint q = rootraw.Load(4 * ((i + 5) & 31));
            outb.Store(4 * (d.y * 16 + d.x), asuint(a.x + a.w * s.a) ^ s.b ^ r ^ q);
            outt[d.xy] = c + float4(s.c, a.yz);
        }"#,
    ),
    // (The 2020 compiler branches around every narrowed region, so these
    // become branches, not guards.)
    (
        "conditional memory access",
        r#"CS {
            float v = d.x * k.x + d.y;
            if (i & 1) v = v * 3 + 1;
            if (i & 4) v += tex.Load(int3(d.xy & 3, 0)).y;
            if (i & 2) outb.Store(4 * (d.y * 16 + d.x), asuint(v));
            if (i & 8) outt[d.xy] = float4(v, 0, 0, 1);
        }"#,
    ),
    // Each side of a per-group branch leaves a compare in VCC; the select
    // after the join reads the joined mask (as `postfx_splines` does).
    (
        "select on joined compares",
        r#"CS {
            float v = d.x * k.x + d.y;
            float r;
            bool ok;
            [branch] if ((g.x ^ g.y) & 1) {
                r = sqrt(v) * k.z + sin(v * k.w);
                ok = r < 5;
            } else {
                r = v * k.y + 1;
                ok = r > -4;
            }
            float o = ok ? r : -1;
            outb.Store(4 * (d.y * 16 + d.x), asuint(o));
            outt[d.xy] = float4(o, r, v, 1);
        }"#,
    ),
    // One host wave of 32 lanes holds the group, as the console's wave of 64
    // does, so lane numbers agree with the PC compile.
    (
        "lane reads",
        r#"[RootSignature(RS)] [numthreads(8, 4, 1)]
        void cs(uint3 d : SV_DispatchThreadID) {
            float v = d.x * k.x + d.y;
            float w = WaveReadLaneAt(v, 5);
            uint lane = WaveGetLaneIndex();
            uint q = QuadReadAcrossX(lane * 10) + QuadReadAcrossDiagonal(lane);
            outb.Store(4 * (d.y * 16 + d.x), q);
            outt[d.xy] = float4(v, w, QuadReadAcrossY(v), lane);
        }"#,
    ),
    // Bytes and halves: SDWA operand and result selection, byte permutes,
    // f16 conversions.
    (
        "sub-dword operations",
        r#"CS {
            uint n = d.y * 16 + d.x;
            uint x = raw.Load(4 * (n & 31)), y = raw.Load(4 * ((n * 7) & 31));
            float a = float(x >> 16) + float((y >> 8) & 255) * k.x;
            float b = f16tof32(x >> 16) + f16tof32(y);
            uint p = (x & 0xff) | ((y & 0xff) << 8) | (x & 0xff0000) | ((y >> 8) << 24);
            uint q = f32tof16(a) | (f32tof16(b) << 16);
            int s = int(x << 24) >> 24;
            uint u = (x & 0xffff) + (y >> 16) + ((x >> 24) ^ (y & 0xffff)) + asuint(k.w) * 1000 + 77;
            outb.Store4(16 * n, uint4(asuint(a), asuint(b) ^ p, q, asuint(float(s)) + u));
        }"#,
    ),
    // 64-bit additions (VALU carries as lane masks) and explicit gradients.
    (
        "carries and gradients",
        r#"CS {
            uint n = d.y * 16 + d.x;
            uint64_t a = (uint64_t(raw.Load(4 * (n & 31))) << 32) | raw.Load(4 * ((n * 3) & 31));
            uint64_t b = (uint64_t(raw.Load(4 * ((n * 5) & 31))) << 32) | raw.Load(4 * ((n * 7) & 31));
            uint64_t s = a + b, u = a - b;
            float4 gr = tex.SampleGrad(smp, (d.xy + 0.5) / 16.0, float2(k.x, 0) * 0.1, float2(0, k.z) * 0.1);
            outb.Store4(16 * n, uint4(uint(s), uint(s >> 32) ^ uint(u), uint(u >> 32), asuint(gr.x + gr.y * 3)));
        }"#,
    ),
    // Comparison sampling at level 0 with an immediate comparison sampler.
    (
        "comparison sampling",
        r#"CS {
            float r = tex.SampleCmpLevelZero(cmp, (d.xy + 0.25) / 15.0, k.x * d.x / 16.0);
            outb.Store(4 * (d.y * 16 + d.x), asuint(r));
        }"#,
    ),
    // 16-bit texture results, two channels per register (`d16`).
    (
        "16-bit texture reads",
        r#"Texture2D<half4> htex : register(t0);
        CS {
            half4 a = htex.SampleLevel(smp, (d.xy + 0.5) / 16.0, 0);
            half3 b = htex.Load(int3(d.xy & 3, 0)).xyz;
            outb.Store4(16 * (d.y * 16 + d.x), uint4(asuint(float(a.x) + a.w),
                asuint(float(a.y) * a.z), asuint(float(b.x)), asuint(float(b.y) - b.z)));
        }"#,
    ),
    // Sizes: the console compiler computes them from descriptor bits
    // (width, height, levels, record counts).
    (
        "sizes from descriptors",
        r#"CS {
            uint w, h, l, n, b, ns, st;
            tex.GetDimensions(0, w, h, l);
            typed.GetDimensions(n);
            raw.GetDimensions(b);
            structured.GetDimensions(ns, st);
            // Read too: the translation takes the views' kinds from those reads.
            float r = tex.Load(int3(d.xy & 3, 0)).x + typed[i & 15].y + structured[i & 7].a;
            outb.Store4(16 * (d.y * 16 + d.x),
                uint4(w * 7 + h, l + asuint(r), n * 5 + ns * 3 + st, b + raw.Load(4 * (i & 31))));
        }"#,
    ),
    // Typed loads (`tbuffer_load_format_*`, the title's vertex
    // fetch) convert by the format the instruction names: the PC reads the
    // same bytes through typed views in those formats. Channels past the
    // format's read (0, 0, 0, 1); a byte format at unaligned offsets.
    (
        "typed loads",
        r#"CS {
            uint n = d.y * 16 + d.x, e = n & 31, f = n >> 5;
            uint4 a, b, c, dd, ee, ff, gg, hh;
        #ifdef CONSOLE
            a = raw.XB_TypedLoad4(4 * e, FMT(0, 10));
            b = raw.XB_TypedLoad4(4 * e, FMT(1, 5));
            c = raw.XB_TypedLoad4(4 * e, FMT(4, 10));
            dd = raw.XB_TypedLoad4(4 * e, FMT(0, 9));
            ee = raw.XB_TypedLoad4(8 * (e & 15), FMT(4, 12));
            ff = uint4(raw.XB_TypedLoad((3 * e) & 127, FMT(0, 1)), 0, 0, 0);
            gg = uint4(raw.XB_TypedLoad2(2 * e, FMT(7, 2)), 0, 0);
            hh = raw.XB_TypedLoad4(4 * e, FMT(1, 10));
        #else
            a = asuint(unorm8x4[e]);
            b = asuint(snorm16x2[e]);
            c = uint8x4[e];
            dd = asuint(unorm10x3_2[e]);
            ee = uint16x4[e & 15];
            ff = uint4(asuint(unorm8[(3 * e) & 127]), 0, 0, 0);
            gg = uint4(asuint(half16[e]), 0, 0, 0);
            hh = asuint(snorm8x4[e]);
        #endif
            uint4 v = f == 0 ? a : f == 1 ? b : f == 2 ? c : f == 3 ? dd
                : f == 4 ? ee : f == 5 ? ff : f == 6 ? gg : hh;
            outb.Store4(16 * n, v);
        }"#,
    ),
    // Wave operations: the console expands them into whole-wave sections
    // (DPP, `v_permlanex16`, lane reads) and lane masks read as numbers
    // (`s_bcnt1`, `s_ff1`, `v_mbcnt`). 32 threads: one wave on the console
    // (lanes 32-63 inactive) and on the host; the branch leaves lanes out.
    (
        "wave operations",
        r#"[RootSignature(RS)] [numthreads(8, 4, 1)]
        void cs(uint3 d : SV_DispatchThreadID, uint i : SV_GroupIndex) {
            uint n = d.y * 16 + d.x;
            float v = d.x * k.x + d.y;
            uint u = raw.Load(4 * (i & 31));
            uint4 o = 0;
            if (u & 3) {
                o.x = WaveActiveSum(u >> 8) + WaveActiveCountBits((u & 4) != 0);
                o.y = asuint(WaveActiveMax(v)) ^ WaveActiveMin(u) ^ WaveActiveBitOr(u)
                    ^ WaveActiveBitAnd(u | 1) ^ asuint(WaveActiveMin(v)) ^ WaveActiveMax(int(u));
                o.z = WavePrefixSum(u & 255) + WavePrefixCountBits(v > 2) * 1000;
                // (The PC compile evaluates a wave operation in a `?:` arm
                // in that arm's lanes only, so it stays outside.)
                o.w = asuint(WaveActiveSum(v)) + (WaveIsFirstLane() ? WaveReadLaneFirst(u) : 0);
            }
            outb.Store4(16 * n, o);
        }"#,
    ),
    // Atomics the console aggregates per wave (one add of the lane count,
    // each lane's offset from `v_mbcnt`), and texture atomics. Results
    // that depend on the lanes' order are left out.
    (
        "aggregated and texture atomics",
        r#"CS {
            uint u = raw.Load(4 * (i & 31)), r, q;
            outb.InterlockedAdd(0, 1, r);
            outb.InterlockedAdd(4 * (1 + r % 8), 1);
            if (u & 1) outb.InterlockedAdd(64, u & 255);
            uint2 p = d.xy & 3;
            InterlockedMax(ut[p], u);
            InterlockedOr(ut[p + uint2(4, 0)], u >> (i & 7));
            InterlockedMin(ut[p + uint2(8, 0)], u);
            InterlockedAdd(ut[p + uint2(12, 0)], u & 15, q);
            InterlockedXor(ut[p + uint2(0, 4)], u);
        }"#,
    ),
    // A loop whose count comes from a view that reads zeros (as a null or
    // not yet mirrored one does) runs four billion times on the console;
    // the translation leaves it after `LOOP_LIMIT` iterations. Run on WARP
    // only (`OATHKEEP_WARP=1`), ours only: without the limit it would hang
    // the GPU.
    (
        "terminates on zero data",
        r#"CS {
            uint cnt = outb.Load(4 * 1000), s = d.x;
            for (uint j = 0; j != cnt - 1; j++) s = s * 3 + raw.Load(4 * (j & 31));
            outb.Store(4 * (d.y * 16 + d.x), s);
        }"#,
    ),
    // Cube sampling: the console computes face coordinates (`v_cube*`)
    // and samples the face; the host samples the direction rebuilt from
    // them. Every face and texel: a major axis of ±2 and the others at
    // texel centres, so both sides compute them exactly.
    (
        "cube sampling",
        r#"CS {
            float sg = (i & 8) ? -2 : 2;
            float a = (d.x & 3) - 1.5, b = (d.y & 3) - 1.5;
            uint ax = i % 3;
            float3 dir = ax == 0 ? float3(sg, a, b) : ax == 1 ? float3(a, sg, b) : float3(a, b, sg);
            float4 p = cubet.SampleLevel(smp, dir, 0);
            float4 q = cubet.SampleLevel(smp, -dir.zxy, 0);
            outb.Store4(16 * (d.y * 16 + d.x), asuint(p + q * 3));
        }"#,
    ),
    // A local array indexed per lane (M0-relative VGPR moves), bit fields
    // and first bits from the top.
    (
        "indexed registers and bit fields",
        r#"CS {
            uint n = d.y * 16 + d.x;
            uint x = raw.Load(4 * (n & 31)), y = raw.Load(4 * ((n * 5) & 31));
            uint arr[24];
            [unroll] for (uint j = 0; j < 24; j++) arr[j] = x ^ (j * 0x01010101);
            [loop] for (uint m = 0; m < (y & 3); m++) arr[(y >> (m * 5)) % 24] = y + m;
            uint v = arr[(x >> 3) % 24];
            uint w = asuint(k.x) & 31, o = n & 15;
            uint bf = (x & ((1u << w) - 1)) << o;
            uint fb = firstbithigh(asuint(k.y)) + firstbithigh(x) * 3 + firstbithigh(int(y)) * 7;
            outb.Store4(16 * n, uint4(v, bf, fb, arr[n % 24]));
        }"#,
    ),
    // Size queries at a level known per lane (`image_get_resinfo`).
    (
        "size queries",
        r#"CS {
            uint w, h, l, cw, ch, cl;
            tex.GetDimensions(d.x >> 4, w, h, l);
            cubet.GetDimensions(d.y >> 4, cw, ch, cl);
            float r = tex.Load(int3(d.xy & 3, 0)).x;
            outb.Store4(16 * (d.y * 16 + d.x), uint4(w * 7 + h, l + asuint(r), cw * 3 + ch, cl));
        }"#,
    ),
    // Gathers with offsets computed per lane (programmable offsets).
    (
        "gather with offsets",
        r#"CS {
            int2 o = int2(int(d.x & 7) - 4, int(d.y % 5) - 2);
            float4 r = tex.GatherRed(smp, (d.xy + 0.5) / 16.0, o);
            float4 q = tex.GatherGreen(smp, (d.yx + 0.25) / 16.0, -o);
            outb.Store4(16 * (d.y * 16 + d.x), asuint(r + q * 3));
        }"#,
    ),
    // Constant arrays: the console compiler appends them to the code and
    // reads them through `s_getpc_b64` (scalar loads for a uniform index,
    // global loads per lane, 16-bit ones too).
    (
        "constant tables",
        r#"static const uint tbl[16] = { 3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5, 8, 9, 7, 9, 3 };
        static const float ftbl[8] = { 0.5, -1, 2.25, 3, -4.5, 6, 7.75, -8 };
        // (16-bit types: compiled with half precision enabled.)
        static const uint16_t stbl[12] = { 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 1100, 65535 };
        CS {
            uint n = d.y * 16 + d.x;
            uint u = asuint(k.x) & 15;
            uint a = tbl[i & 15] ^ tbl[u] << 8;
            float f = ftbl[(i * 3) & 7] * ftbl[u & 7];
            uint h = stbl[(n * 5) % 12] + stbl[u % 12];
            outb.Store4(16 * n, uint4(a, asuint(f), h, tbl[(n + u) & 15]));
        }"#,
    ),
    // 16-bit arithmetic (scalar and packed), 16-bit compares, typed stores
    // of halves (`buffer_store_format_d16`), 64-bit shifts and compares.
    (
        "16-bit and 64-bit arithmetic",
        r#"RWBuffer<half4> hb : register(u3);
        CS {
            uint n = d.y * 16 + d.x;
            uint x = raw.Load(4 * (n & 31)), y = raw.Load(4 * ((n * 7) & 31));
            half a = half(d.x * k.x - 2), b = half(d.y + k.y), c = half(k.z);
            half2 p = half2(a, b) * half2(b, c) + half2(c, a);
            half m = max(a, b) - min(a, c) * a;
            bool lt = a < b, ge = p.x >= p.y;
            hb[n] = half4(p.x, m, lt ? a : b, ge ? p.y : a - b);
            uint64_t q = (uint64_t(x) << (n & 31)) | y, w = uint64_t(y) << 3;
            uint z = q < w ? 1 : q == w ? 2 : 3;
            outb.Store2(8 * n, uint2(uint(q >> 7) ^ z, uint(w >> (n & 15))));
        }"#,
    ),
];

