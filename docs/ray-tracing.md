# Ray Tracing Support

Status: **disabled by default.** Most of the machinery translates and is unit-tested, but is not yet enabled for live rendering.

## How Console Titles Traverse BVHs

- Traversal runs in compute shaders using a per-thread shared-memory stack and raw 40-bit GPU virtual addresses.
- Acceleration structures use the console GPU's native node layout (64-byte box nodes with 16-bit float bounds, 128-byte box nodes with 32-bit float bounds, triangle nodes, instance nodes).
- Dynamic structures are built/refit on the GPU by compute builder shaders (`XboxBvhBuilder.dll`).
- Hardware intersection uses `image_bvh64_intersect_ray` and `image_bvh_intersect_ray`.

Oathkeep runs builder and traversal shaders as translated DXIL and emulates the ray-intersection instructions directly.

## Implemented Components

1. **Builder shader translation** — spilled user-data parameter tables, root UAVs, byte/16-bit buffer accesses, and structured control-flow lowering.
2. **Raw address memory directory** — maps 64 KiB pages of title address space to host copies for shader loads, stores, and atomics.
3. **Emulated ray-node intersection in DXIL** — four slab tests + sorting network for box nodes, watertight ray/triangle test for triangle nodes, verified against the Rust reference in `crates/oathkeep-shader/src/bvh.rs`.
4. **Trace and shading shader translation** — lane masks and wave-64 emulation on 32-lane host GPUs.
5. **Host safety gating** — builder dispatches only run when all created builder pipelines compile (`OATHKEEP_BVH_BUILDER=1`; `OATHKEEP_BVH_QUARANTINE=1` keeps builder stores isolated from title memory).

## Known Gaps

- Some builder pipelines still require wave-64 or fail translation on 32-lane GPUs.
- Several shadow-mask trace shaders do not translate yet.
- Unmapped title pages return zero on read and drop writes.
- Edge rounding and tie-breaking behavior are inferred rather than vendor-documented.

## Key Files

- `crates/oathkeep-shader/src/bvh.rs` — Rust reference for ray/node intersection.
- `crates/oathkeep-shader/src/emit/lower.rs` — DXIL lowering for BVH instructions and raw memory accesses (`Lower::memory`).
- `crates/oathkeep-shader/src/resource.rs` — descriptor and raw-address tracking.
- `crates/oathkeep-gpu/src/pipeline.rs`, `crates/oathkeep-gpu/src/draw.rs` — builder gating, root views, and title memory binding.
