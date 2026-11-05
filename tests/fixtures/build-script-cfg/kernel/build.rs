fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo::rerun-if-changed=kernel.ld");
    println!("cargo::rustc-link-arg=-T{dir}/kernel.ld");
}
