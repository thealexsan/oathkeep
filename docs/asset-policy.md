# Oathkeep Asset & Clean-Room Policy (Rule #1)

**Nothing from any commercial console title, console OS, or proprietary SDK is ever stored in this repository or shared in our community channels.** Oathkeep is a clean-room interoperability project: everything runs locally on the user's machine from files the user supplies.

## Strictly Forbidden

In any form (files, commits, issues, pull requests, string literals, tests, fixtures, snapshots, docs, screenshots, or Discord messages):

- Commercial executables, DLLs, libraries, or any code/data sections extracted from them.
- Console or title assets: models, textures, shaders, shader dumps, audio, video, text, localized strings, or archives (`.rpf`, `.xvd`, `.xvc`, `.ttpf`, `.pak`, `.dds`, `.dxil`, `.dxbc`, `.xma`, etc.).
- Decompiled or disassembled copyrighted code, extracted symbol/string tables, encryption keys, or circumvention tools.
- Instructions, links, or requests for obtaining copyrighted software or console dumps.
- Naming specific commercial titles, franchises, characters, or publishers in code, comments, documentation, commit messages, or community channels. Keep Oathkeep strictly focused on the generic Xbox Series X|S (GDK) platform.

## Allowed

- Clean-room Rust/PowerShell code written from public documentation (AMD RDNA 2 ISA, Microsoft Learn Win32/D3D12/GDK docs, LLVM public specs) and clean-room interoperability testing.
- Cryptographic build identifiers (e.g., BLAKE3 hash, CodeView GUID/age) when needed for compatibility workarounds.
- Small format interoperability constants (magic numbers, enum values) and short, original byte-pattern signatures where strictly necessary for interoperability.
- Synthetic test inputs generated from scratch by our own test suite.
- Runtime telemetry summaries (FPS, draw counts, anonymized shader hashes, HRESULT error codes).

## Binary Exception

`engine/formats/src/bcpack-tables.bin` (~78 KiB) is the single documented exception in `xtask/src/policy.rs`: static entropy/dictionary tables required to decode the BCPack compression format. No further binary exceptions will be accepted.

## Where User Data Lives

All user-supplied title files, caches, saves, and crash dumps must remain **strictly outside the repository**:

- Default cache directory: `%LOCALAPPDATA%\oathkeep\cache`
- Default data/saves directory: `%LOCALAPPDATA%\oathkeep\data`
- User title directories must be treated as **read-only** by all tools and scripts.

## Automated Enforcement

1. **`.gitignore`** blocks executables, DLLs, archives, media, crash dumps, and local config files.
2. **`cargo xtask policy`** (enforced via `.githooks/pre-commit` and `cargo test --workspace`) implements a strict allowlist:
   - Only approved source/doc text extensions are allowed.
   - Maximum file size: 640 KiB.
   - Rejects binary magic headers (`MZ`, `DXBC`, `DDS `, `RIFF`, `BC\xC0\xDE`, `PDB`, `zip`, `7z`, `cab`, `MSI`, etc.) and NUL bytes.
   - Rejects long hex/base64 runs (> 512 chars) to prevent data smuggling.
   - Rejects prohibited commercial title/publisher identifiers.
3. **Runtime boundary guard:** `oathkeep_core::ensure_outside_repo` refuses to write derived cache/data inside the repository tree.

## Remediation if Protected Material Slips Through

A revert commit is **not** sufficient. Maintainers must immediately rewrite git history (`git filter-repo --path <file> --invert-paths`), force-push all branches, notify contributors to re-clone, and purge any cached PR/issue attachments.
