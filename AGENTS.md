# AGENTS.md — Oathkeep

Instructions for AI coding agents working in this repository.

## Project Summary

Oathkeep is a GPL-3.0-or-later clean-room Xbox Series X|S (GDK) compatibility and binary translation layer for Windows written in Rust. The CLI binary is `oathkeep` (`apps/victory`), backed by workspace crates under `crates/victory-*`.

## Non-Negotiable Rules

1. **Clean-room asset policy ([docs/asset-policy.md](docs/asset-policy.md)):**
   - NEVER add, copy, paste, or commit copyrighted executables, DLLs, shaders, textures, archives, keys, memory dumps, decompilations, or extracted strings.
   - NEVER include commercial game titles, franchise names, character names, or publisher names in code, comments, docs, commits, PRs, or replies.
   - Treat any user-supplied local title directory as **strictly read-only**.
2. **Always run policy and tests before committing:**
   ```powershell
   cargo xtask policy
   cargo test -j 4 --workspace
   ```
3. **Cap local build parallelism at `-j 4`** to avoid exhausting host memory or freezing the workstation.
4. **Windows 10 Agility SDK:** `apps/victory/src/main.rs` and `apps/victory/build.rs` export `D3D12SDKVersion` (`620`) and `D3D12SDKPath` (`.\D3D12\`). Never commit `D3D12Core.dll` into git; users fetch it locally via `scripts/setup-agility-sdk.ps1`.
5. **Preserve GPL-3.0 attribution:** Keep `LICENSE` and `THIRD_PARTY.md` accurate when porting fixes from upstream projects (`Marvisak/VIctory`, `fxceless/victory-fork`, etc.).
6. **Keep workspace crate names (`crates/victory-*`) stable** so upstream bugfixes remain easy to cherry-pick.
7. **Never commit secrets or tokens** (Discord tokens, API keys, local machine paths in `victory.toml` / `oathkeep.toml`).
