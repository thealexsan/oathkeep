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
7. **Keep the GitHub Page, Progress Treemap, and Discord Bot Live (mandatory):**
   - `cargo xtask progress` scans the live codebase (`crates/oathkeep-hle`, `crates/oathkeep-gpu`, `crates/oathkeep-shader`, `crates/oathkeep-formats`) plus benchmark history in `docs/progress.toml` to generate `docs/progress.svg`, `docs/progress.json`, `docs/index.html` (served at `https://thealexsan.github.io/oathkeep/`), and the progress section in `README.md`.
   - After every verified milestone, performance run, or major fix, update `docs/progress.toml` (benchmark runs/notes), run `cargo xtask progress`, commit and push so GitHub README + Pages + `docs/progress.json` stay current (`.githooks/pre-commit` enforces `cargo xtask progress --check`).
   - Post clean-room, plain-English, user-friendly updates to the Oathkeep Discord server using the local notifier outside the repo: `node "%LOCALAPPDATA%\oathkeep\discord-notify.mjs" post <gpu|benchmarks|dev|announcements> "<title>" "<body>" [color] [nextGoal]`. `discord-notify.mjs` automatically attaches honest moving metrics from `docs/progress.json` (playable FPS target, library/shader/format code coverage, and live dashboard link) — never paste raw commit hashes, internal jargon walls, or fake `% left` numbers.
   - Once the first mission is verified genuinely playable at console-like fidelity (zero freezes/TDRs, crisp 1:1 visuals, 20–30+ FPS), immediately send the owner a Discord DM via `node "%LOCALAPPDATA%\oathkeep\discord-notify.mjs" dm "<title>" "<body>"`.
   - Every GitHub Page entry and Discord message MUST obey Rule #1 (zero commercial titles, franchises, character names, publisher names, or copyrighted assets).
8. **Capture, analyze, and send in-world 3D gameplay screenshots after every fix (mandatory):**
   - After every code fix/change in `crates/` or `apps/` (visual, GPU, shader, descriptor, or performance), capture fresh **in-world 3D gameplay PNG screenshots** from the rebuilt binary using BOTH `OATHKEEP_CAPTURE=<dir>` AND `OATHKEEP_CAPTURE_WORLD='1'`, pick bright rendered 3D in-world frames (`frame >= 1600`, `>= 150 KB` — **NEVER loading screens, menus, or dark fade-to-black transitions**), inspect them yourself with `read_file` to analyze visual fidelity/artifacts (geometry, shading, lighting, bloom, HUD, text), and embed ONLY those in-world 3D screenshots directly in your reply (`![caption](C:/.../frame-XXXX.png)`) with a concise analysis so the user can see the current gameplay visuals.
   - All screenshot captions, filenames, and visual analyses MUST obey Rule #1 (no commercial titles, franchises, character names, or publisher names).
