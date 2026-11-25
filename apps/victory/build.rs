fn main() {
    // Export Microsoft DirectX 12 Agility SDK symbols from the Windows binary so
    // Windows 10 (19041+) loads a modern D3D12Core.dll with SM 6.6 dynamic
    // resource support when placed in `.\D3D12\` next to the executable.
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        println!("cargo:rustc-link-arg-bin=oathkeep=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bin=oathkeep=/EXPORT:D3D12SDKPath,DATA");
    }
}
