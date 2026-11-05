fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(qt_built)");
    println!("cargo:rustc-cfg=qt_built");
}
