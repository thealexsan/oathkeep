# Oathkeep

[![Discord](https://img.shields.io/badge/Discord-Oathkeep-5865F2?logo=discord&logoColor=white)](https://discord.gg/PM6pr4Azzk)

**Oathkeep** is an open-source, clean-room Xbox Series X|S (GDK) compatibility and binary translation layer for Windows, written in Rust.

It runs unmodified x86-64 console executables directly on a Windows host by providing high-level emulation (HLE) of console system libraries, Direct3D 12.x GPU translation, and RDNA 2-to-DXIL shader recompilation — strictly from files the user supplies locally.

> **Legal & Clean-Room Notice:** This repository contains **zero** copyrighted software, executables, keys, shaders, textures, audio, archives, or extracted data. You must supply your own legally obtained console software. Oathkeep is an independent open-source interoperability project and is not affiliated with, endorsed by, or connected to any console manufacturer, game developer, or publisher. See [docs/asset-policy.md](docs/asset-policy.md).

<!-- oathkeep:progress:start -->
## Compatibility & Code Coverage Progress

Live dashboard: [thealexsan.github.io/oathkeep](https://thealexsan.github.io/oathkeep/) · Libraries: **52.4%** (`688/1314`) · RDNA2 decoded: **100.0%** (`1233/1233`) · RDNA2 → DXIL lowered: **52.1%** (`642/1233`) · Latest world: **5.4 fps (185.5 ms/f)**.

![Oathkeep Codebase Coverage Dashboard](docs/progress.svg)

| Run | Commit | Peak FPS | Frame | Summary |
| --- | --- | ---: | ---: | --- |
| diag-fps-swarm3b | `09a807e` | 5.4 | 185.5 ms | PR #3 Round 3 FPS swarm + CopyMemoryToMemoryX Step::Copy + 16 GB RAM commit caps verified clean! Reached frame-1782 (+124 frames further in… |
| diag-fps-swarm2b | `3a0890d` | 5.4 | 184.0 ms | Post-revert Round 2 baseline verified clean! Peak 5.4 fps (184.0 ms/f) in-world, 0/11,093 refused dynamic tables, 0 exploding triangles, in… |
| diag-fps-swarm2 | `289d0ea` | 3.1 | 320.1 ms | Round 2 capture verified Wave64 LDS fixes (5dc3b25986622949 & 6f225b641c5b0708 compiled/bound cleanly), async sweep (41-54->0.1 ms), and ra… |
| diag-fps-swarm1 | `d1e5000` | 3.9 | 256.1 ms | Round 1 4-worker FPS swarm verified clean (95ccbe8..d1e5000): peak 3.9 fps (256.1 ms/f), steady 2.9-3.9 fps, preparing -37%, copy-back -39%… |
| diag-visual-holereuse | `487d82a` | 2.9 | 347.8 ms | Zero-VRAM-growth hole reuse (pool::NO_GROW + tiles no_grow + 5/6-block staging cap + reuse-only admission) admitted 24,000+ textures into e… |
| diag-visual-vramfix2 | `301d66f` | 3.4 | 295.1 ms | VRAM overshoot & Wave64 shader 02384a60fda6f2d0 verified fixed! Peak DXGI VRAM 6,874/7,164 MB (down from 7,602 MB crash), pool::TEXTURES 4,… |
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

### 4. Launch a title (with automatic shader warming & preloading)

```powershell
# Recommended: pre-translates shaders, pre-warms D3D12 pipelines, and launches with Pareto 8 GB profile:
powershell -ExecutionPolicy Bypass -File scripts\launch.ps1 -Exe "<path-to-your-title-exe>"

# Pre-warm caches only (without launching the title yet):
powershell -ExecutionPolicy Bypass -File scripts\launch.ps1 -Exe "<path-to-your-title-exe>" -NoLaunch

# Direct launch without pre-warming (`play.ps1`):
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Exe "<path-to-your-title-exe>"
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
