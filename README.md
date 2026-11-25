# Oathkeep

[![Discord](https://img.shields.io/badge/Discord-Oathkeep-5865F2?logo=discord&logoColor=white)](https://discord.gg/PM6pr4Azzk)

**Oathkeep** is an open-source, clean-room Xbox Series X|S (GDK) compatibility and binary translation layer for Windows, written in Rust.

It runs unmodified x86-64 console executables directly on a Windows host by providing high-level emulation (HLE) of console system libraries, Direct3D 12.x GPU translation, and RDNA 2-to-DXIL shader recompilation — strictly from files the user supplies locally.

> **Legal & Clean-Room Notice:** This repository contains **zero** copyrighted software, executables, keys, shaders, textures, audio, archives, or extracted data. You must supply your own legally obtained console software. Oathkeep is an independent open-source interoperability project and is not affiliated with, endorsed by, or connected to any console manufacturer, game developer, or publisher. See [docs/asset-policy.md](docs/asset-policy.md).

<!-- oathkeep:progress:start -->
## Compatibility & Performance Progress (94%)

Live dashboard: [thealexsan.github.io/oathkeep](https://thealexsan.github.io/oathkeep/) • Auto-generated from [docs/progress.toml](docs/progress.toml) via `cargo xtask progress`.

![Oathkeep Compatibility and Performance Progress](docs/progress.svg)

| Run | Commit | FPS | Frame | GPU | Preparing | Status |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| diag-run63-warm | `bc99622` | 5.4 | 183.9 ms | 49.3 ms (3467 draws, 54-59% busy) | 125.3 ms (0 PSOs left, run_deferred 10.4 ms) | run_deferred -43%, copy queue -51% (19.3 ms), copy-back -33% (42.3 ms), fallbacks -85% (35); remaining: GUARDS 55.9 ms (0xae00000 29.5 ms), mirror 18.6 ms, 1519 indirect clamps on GPU |
| diag-run62 | `eb5ce1e` | 6.1 | 162.8 ms | 47.9 ms (4772 draws, 55% busy) | 106.7 ms (warm 5 PSOs left) | Uninstrumented 4772 draws/frame (12x more draws); host-pipelines.bin 258.8 MB; heap_buffers 18.1 ms; next: guard churn + copy-queue stall |
| diag-run61 | `15e6154` | 3.7 | 270.7 ms | 35.2 ms timed (ML <=0.63 ms) | 103.3 ms (cold) | ML VRAM fix verified (4.8x GPU speedup, GPU 33% busy); CPU-bound now |
| diag-run60 | `4a14eaa` | 2.7 | 376.1 ms | 168.7 ms (135.6 ms ML) | 108.2 ms (cold) | Zero TDR; all 9/9 ML shaders dumped for DP4A diff |
| diag-run59b | `40200b5` | 5.8 | 172.1 ms | 27.7 ms (no ML) | 59.7 ms | Zero TDR; 2561af95 0.36 ms/frame; ask_once 4.5 ms |
| diag-run56-gpu | `542b511` | 5.4 | 185.0 ms | 118 ms | 75 ms | DP4A sped 5/9 ML shaders; 4/9 regressed |
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
