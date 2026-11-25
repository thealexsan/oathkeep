fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        println!("cargo:rustc-link-arg-bin=oathkeep=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bin=oathkeep=/EXPORT:D3D12SDKPath,DATA");
    }
}
