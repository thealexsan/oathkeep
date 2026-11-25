# Contributing to Oathkeep

Thank you for helping build Oathkeep! To keep the project legally safe, maintainable, and welcoming, every contribution must follow these rules.

## 1. Zero-Tolerance Clean-Room Rule

Read [docs/asset-policy.md](docs/asset-policy.md) before writing a single line of code.

- **Never** commit, paste, or attach copyrighted binaries, shaders, textures, archives, crash dumps containing title memory, decompiled code, keys, or commercial title/publisher names.
- **Never** ask for or link to copyrighted files in issues, PRs, or Discord.
- Run `cargo xtask install-hooks` right after cloning so the pre-commit policy check runs automatically on every commit.

## 2. Development Setup

```powershell
cargo xtask install-hooks
cargo test -j 4 --workspace
cargo xtask policy
```

Windows 10 contributors testing live rendering must install Microsoft's DirectX 12 Agility SDK locally (outside git):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-agility-sdk.ps1
```

## 3. Code & Pull Request Standards

- **Keep changes focused and modular:** Follow the `apps/oathkeep` and `crates/oathkeep-*` workspace structure. Keep comments concise and purposeful (`// SAFETY:` for unsafe invariants).
- **Verify against tests:** New behavior or bug fixes should include a synthetic unit test that does not require external title files.
- **No silent fake success:** Unimplemented APIs must log clearly and return the appropriate error code (e.g., `E_NOTIMPL`).
- **Format & lint before opening a PR:**
  ```powershell
  cargo fmt --check
  cargo clippy -j 4 --workspace --all-targets -- -D warnings
  cargo test -j 4 --workspace
  cargo xtask policy
  ```
- **Commit style:** Conventional prefixes (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`) with clear, concise descriptions.
- **License:** All contributions are licensed under GPL-3.0-or-later. Credit any upstream inspiration or ported work in [THIRD_PARTY.md](THIRD_PARTY.md).
