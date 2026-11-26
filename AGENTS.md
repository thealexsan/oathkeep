# AGENTS.md — Oathkeep

Instructions for AI coding agents working in this repository.

## Project Summary

Oathkeep is a GPL-3.0-or-later clean-room Xbox Series X|S (GDK) compatibility and binary translation layer for Windows written in Rust. The CLI binary is `oathkeep` (`apps/oathkeep`), backed by workspace crates under `crates/oathkeep-*`.

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
4. **Windows 10 Agility SDK:** `apps/oathkeep/src/main.rs` and `apps/oathkeep/build.rs` export `D3D12SDKVersion` (`620`) and `D3D12SDKPath` (`.\D3D12\`). Never commit `D3D12Core.dll` into git; users fetch it locally via `scripts/setup-agility-sdk.ps1`.
5. **Preserve GPL-3.0 attribution:** Keep `LICENSE` and `THIRD_PARTY.md` accurate.
6. **Never commit secrets or tokens** (Discord tokens, API keys, local machine paths in `oathkeep.toml`).
7. **Keep the GitHub Page, Progress Graph, and Discord Bot Live (mandatory):**
   - `docs/progress.toml` is the single source of truth for the green-box compatibility/performance matrix (`docs/progress.svg`), `README.md`, and GitHub Pages (`docs/index.html`, served at `https://thealexsan.github.io/oathkeep/`).
   - After every verified milestone, performance run, or major fix, update `docs/progress.toml`, run `cargo xtask progress`, commit and push so GitHub README + Pages stay current (`.githooks/pre-commit` enforces `cargo xtask progress --check`).
   - Post clean-room dev/benchmark updates to the Oathkeep Discord server using the local notifier outside the repo: `node "%LOCALAPPDATA%\oathkeep\discord-notify.mjs" post <gpu|benchmarks|dev|announcements> "<title>" "<body>" [color] "<eta>"`. **Every Discord update MUST include current progress (`% done`, `% left` to playable console parity) and an honest `ETA`** (`discord-notify.mjs` automatically computes `% done / % left` and renders a green-box progress bar from `docs/progress.toml`).
   - Once the first mission is verified genuinely playable at console-like fidelity (zero freezes/TDRs, crisp 1:1 visuals, 20–30+ FPS), immediately send the owner a Discord DM via `node "%LOCALAPPDATA%\oathkeep\discord-notify.mjs" dm "<title>" "<body>"`.
   - Every GitHub Page entry and Discord message MUST obey Rule #1 (zero commercial titles, franchises, character names, publisher names, or copyrighted assets).
8. **Capture, analyze, and send screenshots after every fix (mandatory):**
   - After every code fix/change in `crates/` or `apps/` (visual, GPU, shader, descriptor, or performance), capture fresh in-world 3D PNG screenshots from the rebuilt binary (`OATHKEEP_CAPTURE` + `OATHKEEP_CAPTURE_FRAMES`), inspect them yourself with `read_file` to analyze visual fidelity/artifacts (geometry, shading, lighting, bloom, HUD, text), and embed the fresh screenshots directly in your reply (`![caption](C:/.../frame-XXXX.png)`) with a concise analysis so the user can see the current visual state.
   - All screenshot captions, filenames, and visual analyses MUST obey Rule #1 (no commercial titles, franchises, character names, or publisher names).
