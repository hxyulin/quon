use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=probe.txt");
    println!("cargo::rustc-cfg=probe_new");
    println!("cargo:rustc-check-cfg=cfg(probe_new)");
    println!("cargo::rustc-env=PROBE_GREETING=hi");
    println!("cargo::metadata=key=42");
    println!(
        "cargo:warning=probe ran for {}",
        env::var("TARGET").unwrap()
    );
    assert_eq!(env::var("CARGO_FEATURE_FANCY").as_deref(), Ok("1"));
    assert_eq!(env::var("CARGO_MANIFEST_LINKS").as_deref(), Ok("probe"));

    let text = fs::read_to_string("probe.txt").unwrap();
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("gen.rs");
    fs::write(
        out,
        format!("pub const GENERATED: &str = {:?};\n", text.trim()),
    )
    .unwrap();
}
