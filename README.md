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
| diag-run105 | `61ccefc` | 16.6 | 60.3 ms | 0.0 ms median wait (0 hangs/TDRs/exceptions/refusals in 130.2s) | 31.3-56.3 ms (46.2 ms median: batch_flush 17.9-29.4 ms, stale 4.5-7.6 ms, prelude 3.2-5.3 ms, ask_once 3.1-3.9 ms) | 100% CLEAN 130.2s baseline! Fixed relay hot-hash race (f73fd1a) + VEH guard-page TOCTOU in watch.rs::handle_write_fault (61ccefc). Zero CPU exceptions, zero GPU hangs, zero refusals, 7.7-16.6 fps steady 3D (12.8 median / 16.6 peak) -> next: safe CPU optimizations for batch_flush, stale/ask_once, and recording bind/tables to hit 20-30+ FPS |
| diag-run98 | `c059f27` | 17.5 | 57.2 ms | 0.0 ms median wait (80-93% busy; hit unhandled 0xc0000005 at ~125s) | 31.2-53.4 ms (43.9 ms median: batch_flush 22.9 ms, stale 5.8 ms, guards 8.6 ms, cfu 0.8 ms) | Fix AF1-AF2 peak 17.5 fps (57.2 ms/f), but late crash at ~125s traced to AF1/AF2/AG2 races (subsequently reverted & fixed in f73fd1a + 61ccefc) |
| diag-run94 | `a24cbeb` | 16.8 | 59.4 ms | 0.0-2.6 ms wait (0 hangs/TDRs/refusals in 130.3s at default 512 KiB) | 27.5-45.5 ms NEW LOW (batch_flush 14.5-24.8 ms, guards 4.6-15.3 ms, stale 4.2-8.5 ms, sweep 0.08-0.60 ms) | Fix AC1-AC4 verified clean: pre_prep sweep -85% (0.08-0.60 ms), guards -55% (4.6-15.3 ms), alias_apply -65% (1.19-5.21 ms), watches -50% (6.72-17.84 ms), make_at miss -50% (3.16-11.17 ms), true indirect 4.56-11.39 ms, NEW ALL-TIME LOW 27.5 ms preparing & 79.1 ms rec (11.7-16.8 fps steady); exposed next 3 blockers: 13.2-18.1 ms card copy-back (20.8 MB), 14.5-24.8 ms batch_flush, and 10.8-13.7 ms post-present gap -> next: Fix AD |
| diag-run93 | `perf/commit-reduction` | 18.8 | 53.2 ms | 0.0-1.7 ms wait (0 hangs/TDRs/refusals in 135.6s at default 512 KiB) | 30.0-54.0 ms (batch_flush 14.5-27.1 ms, guards 10.9-26.4 ms, stale 4.2-7.6 ms, sweep 0.43-1.72 ms) | Fix AB1-AB4 verified clean: NEW ALL-TIME PEAK 18.8 fps (53.2 ms/f, drw=994), steady 11.3-17.2 fps (17.2 fps drw=1135, 16.9 fps drw=1510, 14.3 fps drw=3140), pre_prep sweep cut to 0.43-1.72 ms, make_at cbv -50% (4.0-7.9 ms), tables non-make_at cut; next: Fix AC (throttle !behind sweep, cut batch_flush/alias_apply + indirect 45-132 ms + make_at miss/post -> 20-30+ FPS) |
| diag-run92 | `ad52400` | 17.9 | 55.8 ms | 0.0 ms wait (0 hangs/TDRs in 130s at default 512 KiB) | 35.0-51.8 ms (batch_flush 18.2-27.6 ms, stale 4.8-6.0 ms, pre_prep sweep 1.7-5.2 ms) | Fix AA1-AA4 verified clean (isolated MIN_KB=128 hang from run91): 17.9 fps peak, drw=5788 batch_flush -39% (30->18.2 ms), stale -57% (11.4->4.9 ms), make_at -45% (77.2->42.3 ms), stage_cache 62% 1-shot bypass, pre_prep exposed WHY_SYNC sweep stall (1.7-5.2 ms/f, lock_wait 0.00 ms) -> next: Fix AB |
| diag-run90 | `perf/commit-reduction` | 16.6 | 60.4 ms | 0.0 ms wait (67-86% busy) | 34.8-46.6 ms (run_deferred 1.5-1.9 ms, snapshots 1.3-1.6 ms, back 13.6-18.4 ms) | Fix Z1-Z4 verified: run_deferred -68% (5.0->1.6 ms), snapshots -53% (3.0->1.4 ms), copy-back -30% (24->16 ms), compared MB -56% (33->14.4 MB), heavy 3D scenes +18-32% (drw=3958 9.8->11.6, drw=3449 11.7->13.8, drw=2473 10.4->13.7 fps); next: Fix AA (2-hit stage-cache admission filter + parallel card stale_under_flights + 128 KB card threshold) |
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
