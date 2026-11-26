# Third-Party Components & Attribution

Oathkeep is licensed under the GNU General Public License v3.0 or later ([LICENSE](LICENSE)).

## Upstream Projects

- **[VIctory](https://github.com/Marvisak/VIctory)** (Marvisak and contributors) — GPL-3.0-or-later. Original foundation for the PE loader, console HLE runtime, Direct3D 12.x translation layer, and RDNA 2-to-DXIL shader recompiler.
- **[cool_fox (pareto-residency)](https://github.com/avatarmanz/VIctory-7900XTX)** (codec-d, avatarmanz, and contributors) — GPL-3.0-or-later. Direct upstream base of Oathkeep's workspace crates, including pareto-residency GPU memory management, card stepping, thin lists, indirect batching, shader compiler optimizations, ray-tracing/BVH support, replay tooling, and directional indirect-light ambient stand-in (`ambient.rs`).
- **[victory-fork](https://github.com/fxceless/victory-fork)** (fxceless and contributors) — GPL-3.0-or-later. Upstream rendering, shader optimization, memory synchronization, and GPU scheduling improvements.
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)** (DexNom) — GPL-3.0-or-later. Ray tracing fixes, cube gradients, and sparse textures.
- **[VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)** (Belobug, romainhedouin, and contributors) — GPL-3.0-or-later. Directional indirect-light ambient stand-in techniques and pipeline workers.
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)** — GPL-3.0-or-later. Upstream shader and GPU contributions.
- **[victory-amd](https://github.com/myokex/victory-amd)** (myokex, Joshua Odom, Spexivus, joroyt, tightfall) — GPL-3.0-or-later. Multi-view page guard and kernel read-beside fixes.
- **Geckomacabre** and **cxy0072019-sketch** — GPL-3.0-or-later. Texture read-back support.
- **[XCL](https://github.com/ivall/xcl)** (ivall and contributors) — GPL-2.0-only. Referenced upstream for behavioral comparison; no code from XCL is included.
- **[ztrx](https://github.com/Zettrox/ztrx)** (Zettrox) — GPL-3.0-or-later. Inspiration for clean launcher defaults and 8 GB NVIDIA GPU tuning profiles.

## External Runtime (Downloaded Locally by Users, Never Bundled)

- **Microsoft DirectX 12 Agility SDK (`Microsoft.Direct3D.D3D12`)** — Microsoft proprietary redistributable fetched directly from NuGet by `scripts/setup-agility-sdk.ps1` on Windows 10 hosts; never stored in this repository.

## Rust Crate Dependencies

See `Cargo.toml` and `Cargo.lock` for the full list of open-source Rust dependencies (including `windows-rs`, `clap`, `tracing`, `blake3`, `zlib-rs`, `opus-pure`, `object`, `serde`, `toml`, `anyhow`, `thiserror`).
