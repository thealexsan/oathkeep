fn main() {
    println!("cargo:rustc-link-arg-cdylib=/DELAYLOAD:oathkeep-probe-delayed.dll");
    println!("cargo:rustc-link-lib=delayimp");
}
