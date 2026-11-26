# Oathkeep

[![Discord](https://img.shields.io/badge/Discord-Oathkeep-5865F2?logo=discord&logoColor=white)](https://discord.gg/PM6pr4Azzk)

**Oathkeep** is an open-source, clean-room Xbox Series X|S (GDK) compatibility and binary translation layer for Windows, written in Rust.

It runs unmodified x86-64 console executables directly on a Windows host by providing high-level emulation (HLE) of console system libraries, Direct3D 12.x GPU translation, and RDNA 2-to-DXIL shader recompilation — strictly from files the user supplies locally.

> **Legal & Clean-Room Notice:** This repository contains **zero** copyrighted software, executables, keys, shaders, textures, audio, archives, or extracted data. You must supply your own legally obtained console software. Oathkeep is an independent open-source interoperability project and is not affiliated with, endorsed by, or connected to any console manufacturer, game developer, or publisher. See [docs/asset-policy.md](docs/asset-policy.md).

<!-- oathkeep:progress:start -->
## Compatibility & Performance Progress (95%)

Live dashboard: [thealexsan.github.io/oathkeep](https://thealexsan.github.io/oathkeep/) • Auto-generated from [docs/progress.toml](docs/progress.toml) via `cargo xtask progress`.

![Oathkeep Compatibility and Performance Progress](docs/progress.svg)

| Run | Commit | FPS | Frame | GPU | Preparing | Status |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| diag-visual-holereuse | `perf/commit-reduction` | 2.9 | 347.8 ms | 17.7-100.1 ms (150.8s TIMEOUT_CLEAN, peak VRAM 7,148/7,161 MB, 0 crashes/TDRs/OOMs) | 157.4 ms (2324-3044 draws/f, staging 57-112 MB, tiles 305/320 MB reused) | 1:1 CONSOLE VISUALS VERIFIED! Zero-VRAM-growth hole reuse (pool::NO_GROW + tiles no_grow + 5/6-block staging cap + reuse-only admission) admitted 24,000+ textures into existing pool holes with 0 extra VRAM heaps and 0 staging spikes; secondary NPC faces, background patrons, and interior walls/posters are now 100% textured and naturally lit (frame-1928..1990). Ready for 20-30+ FPS optimization swarm! |
| diag-visual-vramfix2 | `301d66f` | 3.4 | 295.1 ms | 54.8 ms (146.1s TIMEOUT_CLEAN, peak VRAM 6,874/7,164 MB, 0 crashes/TDRs) | 70.8 ms (3062 draws/f, heap buffers 9.9 ms, copy-back 32.4 ms) | VRAM overshoot & Wave64 shader 02384a60fda6f2d0 verified fixed! Peak DXGI VRAM 6,874/7,164 MB (down from 7,602 MB crash), pool::TEXTURES 4,928->4,224 MB, tiles 512->256 MB, 18,785 idle textures freed (3,461 MB), 0 non-RT dropped shaders, 0 capped-view-heap hits; foreground character/hair/wall posters/outdoor vehicles crisp, secondary NPC faces & dark interiors remaining before 20-30+ FPS swarm |
| diag-visual-merged | `b6b9b0e` | 3.0 | 335.3 ms | 0.0-110.5 ms (149.1s TIMEOUT_CLEAN, 0 exceptions/TDRs/refusals) | 135.1-210.5 ms (all 4 visual-artifact swarm workers merged; 0 depth-plane misses, 0 CBV/buffer-view/0x1c8/0x218/0x1a8 warnings, 6b82bc7d22016cb5 translated) | 4-worker visual swarm verified in-world (frame-2100..2148): crisp foreground character/hair/eyes/DOF/outdoor vehicles, 0 depth-plane misses (79/81->0/0), 10330 aligned view copies (0 refused); 3 visual blockers isolated for final polish: VRAM LRU eviction under levels-on-demand (35132 waiting), 1M descriptor heap cap (1245184 requested, 737855 direct), and 1 Wave64 compute shader (02384a60fda6f2d0) |
| diag-run109 | `7f206eb` | 9.0 | 111.6 ms | 0.0-10.2 ms (130.5s TIMEOUT_CLEAN, 0 exceptions/TDRs/refusals) | 55.2-145.1 ms (align=256 CBVs still created 2048 overlapping 64 KiB windows -> replace with CBV slab/skip next) | Visual Fixes 3+4 + Worker 1 phase-windowing verified clean! Inline pass_plane + compute depth-to-plane copy-back eliminated both depth-twin counters (0 not given back, 0 used before pass) and fresh texture queue anti-starvation landed cleanly; ruled out both texture backlog (diag-run108: 39.5k uploaded) and depth-twin sync for zero-specular dark faces; next: integrate Worker 3 CPU perf + Worker 4 face/wall pass diagnosis + fix align=256 CBV window explosion |
| diag-run107 | `perf/commit-reduction` | 10.4 | 96.1 ms | 6.6-15.1 ms (0 hangs/TDRs/exceptions/refusals in 130.4s) | 34.6-80.5 ms (4096+ per-view aligned shadows added copy/alloc overhead) | Visual Fixes 1+2 verified! Integer-RTV blend sanitization fixed mesh PSO f5e50eb2108c425b (0 refused PSOs) and shadow::find_aligned fixed R32G32B32_FLOAT (element=12) SRVs & static unaligned CBVs (0 warnings). Exploding stretched triangles and 2x2 stippling completely gone in 31 captured in-world 3D frames (crisp characters, hair, eyes, stitching, DOF, lighting); remaining: dark faces/interiors (32.6k due textures / SSS check) + phase-window aligned shadows to recover 12.4-15.7+ fps |
| diag-run106 | `77f2abc` | 15.7 | 63.7 ms | 0.0 ms median wait (0 hangs/TDRs/exceptions/refusals in 130.3s) | 32.3-51.1 ms (batch_flush 18.6-25.5 ms, stale 6.0-8.2 ms, prelude 2.5-4.3 ms, ask_once 3.1-3.3 ms) | Fix AH1+AH2+AH3a verified clean! Heavy 4010-draw scene +61% (7.7->12.4 fps, 130.2->80.6 ms/f), moves -32% (36.5->24.7 / 17.9->12.8 ms), guards -29%, rec -10%; pinned down visual root causes: refused mesh PSO R8_UINT RTV (format=62) + R32G32B32_FLOAT (element=12) null SRV |
<!-- oathkeep:progress:end -->

## Requirements

- **OS:** Windows 10 (19041+) or Windows 11, x64
- **CPU:** x86-64 with AVX2
- **GPU:** Direct3D 12 GPU with Shader Model 6.6 and Mesh Shaders (NVIDIA RTX 20-series+ or AMD RDNA 2+)
- **Memory:** 16–32 GB RAM and a sufficiently sized Windows paging file (if `crashes.log` reports Windows error `1455`, close background apps or increase the paging file)
- **Toolchain:** Rust (pinned in `rust-toolchain.toml`) and MSVC Build Tools

## Quick Start

### 1. Install the asset-policy pre-commit hook (recommended for contributors)

```powershell
cargo xtask install-hooks
```

### 2. Build Oathkeep

```powershell
cargo build -j 4 --release -p oathkeep
# Or with diagnostics enabled (captures, DRED, scripted keys, frame timing):
cargo build -j 4 --profile diag -p oathkeep --features diagnostics
```

### 3. Windows 10 only: install the Microsoft DirectX 12 Agility SDK

Windows 10's inbox `D3D12Core.dll` (`10.0.19041`) does not support Shader Model 6.6 dynamic resource root signatures (`0xc01`). Oathkeep exports `D3D12SDKVersion = 620` and `D3D12SDKPath = ".\D3D12\"`. Run our setup script once to fetch Microsoft's official `Microsoft.Direct3D.D3D12` `1.620.2` runtime DLLs into `target\release\D3D12\` and `target\diag\D3D12\` (outside git):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-agility-sdk.ps1
```

Windows 11 ships SM 6.6 inbox and does not require this step.

### 4. Launch a title

```powershell
# Safe default profile (recommended for 8 GB NVIDIA GPUs and first-run stability):
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Exe "<path-to-your-title-exe>"

# Faster GPU scheduling profile (late waits + card buffers):
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Exe "<path-to-your-title-exe>" -Profile fast

# 8 GB VRAM profile (enables levels-on-demand texture budgeting):
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Exe "<path-to-your-title-exe>" -Profile nvidia8gb
```

Caches and saves are stored outside the repository under `%LOCALAPPDATA%\oathkeep\cache` and `%LOCALAPPDATA%\oathkeep\data` (overridable via `-Cache` / `-Data` or `oathkeep.toml`).

## Input Mapping

XInput controllers work natively. Without a controller connected, the keyboard acts as a controller while the title window has focus:

| Keys | Controller |
| --- | --- |
| `W A S D` / `I J K L` | Left / Right stick |
| Arrow keys | D-pad |
| `Enter`, `Backspace`, `X`, `Y` | A, B, X, Y |
| `Q` / `E`, `Z` / `C` | Shoulders, Triggers |
| `F` / `H` | Stick clicks |
| `Esc` / `Tab` | Menu / View |

## Repository Structure

See [docs/architecture.md](docs/architecture.md) for subsystem details:

- `apps/oathkeep` — `oathkeep` CLI (`run`, `inspect`, `census`, `translate`, `shader-info`, `lib-exports`, `config`)
- `crates/oathkeep-core` — configuration, logging, repo-boundary enforcement (`ensure_outside_repo`)
- `crates/oathkeep-pe` — read-only PE32+ and import-library parser
- `crates/oathkeep-loader` — PE32+ image preparation, TLS redirection, console CPU instruction emulation
- `crates/oathkeep-hle` — console OS, filesystem, DirectStorage, input, audio (XMA/Opus), and runtime HLE
- `crates/oathkeep-gpu` — Direct3D 12.x device, command lists, bindless heaps, texture/memory sync, host D3D12 backend
- `crates/oathkeep-shader` — RDNA 2 decoder, IR, optimization passes, and DXIL emitter
- `crates/oathkeep-formats` — texture layouts, BCPack, and audio codecs
- `crates/oathkeep-pipeline` — converter trait and content-addressed cache
- `crates/oathkeep-game` — build identification and format helpers
- `scripts/` — launcher (`play.ps1`) and Windows 10 Agility SDK setup (`setup-agility-sdk.ps1`)
- `xtask` — repository asset-policy enforcement (`cargo xtask policy`) and hook installer

## Diagnostics & Environment Switches

Build with `--profile diag --features diagnostics` (or pass `-Diag` to `scripts/play.ps1`) to enable diagnostic switches:

- `OATHKEEP_LOG=<target>=debug,info` — structured trace logging
- `OATHKEEP_D3D12_DRED=1` — Device Removed Extended Data (slows frames; use only to diagnose GPU hangs)
- `OATHKEEP_D3D12_WARP=1` — run on Microsoft's WARP software adapter
- `OATHKEEP_SHADER_LOOP_LIMIT=<n>` — bound translated shader loops
- `OATHKEEP_LATE_WAIT=3`, `OATHKEEP_WIDE_BACK=1`, `OATHKEEP_RELAY=2` — asynchronous GPU submission tuning
- `OATHKEEP_LEVELS_ON_DEMAND=1` — upload only populated mip levels (reduces VRAM usage on 8 GB GPUs)
- `OATHKEEP_PIPELINE_WAIT_MS=<ms>` — cap per-frame wait for background PSO compilation

## Contributing & Governance

- **Asset Policy (mandatory):** [docs/asset-policy.md](docs/asset-policy.md)
- **Contributing Guide:** [CONTRIBUTING.md](CONTRIBUTING.md)
- **AI Agent Guidelines:** [AGENTS.md](AGENTS.md)
- **Code of Conduct:** [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- **Security & Legal Contact:** [SECURITY.md](SECURITY.md)
- **Third-Party Credits:** [THIRD_PARTY.md](THIRD_PARTY.md)

## License

Oathkeep is licensed under the **GNU General Public License v3.0 or later** ([LICENSE](LICENSE)). See [THIRD_PARTY.md](THIRD_PARTY.md) for upstream and dependency attribution.
