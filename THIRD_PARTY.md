# Third-Party Components & Attribution

Oathkeep is licensed under the GNU General Public License v3.0 or later ([LICENSE](LICENSE)).

## Upstream Projects

- **[VIctory](https://github.com/Marvisak/VIctory)** (Marvisak and contributors) — GPL-3.0-or-later. Original foundation for the PE loader, console HLE runtime, Direct3D 12.x translation layer, and RDNA 2-to-DXIL shader recompiler.
- **[victory-fork](https://github.com/fxceless/victory-fork)** (fxceless and contributors) — GPL-3.0-or-later. Direct upstream base of Oathkeep, including rendering, shader optimization, memory synchronization, and GPU scheduling improvements.
- **[victory-amd](https://github.com/myokex/victory-amd)** (myokex, Joshua Odom, Spexivus, joroyt, tightfall) — GPL-3.0-or-later. Multi-view page guard and kernel read-beside fixes inherited via `victory-fork`.
- **[XCL](https://github.com/ivall/xcl)** (ivall and contributors) — GPL-2.0-only. Referenced upstream for behavioral comparison; fixes were written independently in VIctory's codebase.
- **[ztrx](https://github.com/Zettrox/ztrx)** (Zettrox) — GPL-3.0-or-later. Inspiration for clean launcher defaults and 8 GB NVIDIA GPU tuning profiles.
- **[VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)** (Belobug and contributors) & **[VIctory-7900XTX / cool_fox](https://github.com/avatarmanz/VIctory-7900XTX)** (avatarmanz, codec-d, and contributors) — GPL-3.0-or-later. Directional indirect-light ambient stand-in (`ambient.rs`) and GPU residency/bake-upload techniques.

## External Runtime (Downloaded Locally by Users, Never Bundled)

- **Microsoft DirectX 12 Agility SDK (`Microsoft.Direct3D.D3D12`)** — Microsoft proprietary redistributable fetched directly from NuGet by `scripts/setup-agility-sdk.ps1` on Windows 10 hosts; never stored in this repository.

## Rust Crate Dependencies

See `Cargo.toml` and `Cargo.lock` for the full list of open-source Rust dependencies (including `windows-rs`, `clap`, `tracing`, `blake3`, `zlib-rs`, `opus-pure`, `object`, `serde`, `toml`, `anyhow`, `thiserror`).
