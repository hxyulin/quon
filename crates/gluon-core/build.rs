//! Build script: bake the gluon checkout path into the crate, so a
//! gluon built from a checkout builds the gluon-config library that
//! `gluon.rs` links against from that checkout's sources rather than
//! from crates.io. See `script_runner::rlib`.
//!
//! The path is resolved at compile time by walking up from `CARGO_MANIFEST_DIR`
//! until we find a parent that contains `crates/gluon-config/Cargo.toml`.
//!
//! An installed gluon (`cargo install gluon-cli`) is compiled from an
//! unpacked crate with no such parent. The fallback bakes the manifest
//! dir itself, which has no `crates/gluon-config/Cargo.toml`, so at
//! runtime gluon finds no checkout and uses the published gluon-config
//! of its own version. The same happens when a checkout that gluon was
//! built from is later deleted.

fn main() {
    println!("cargo:rerun-if-env-changed=GLUON_CONFIG_SOURCE_DIR");

    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo");

    // Walk up to find the workspace root (the first ancestor containing
    // `crates/gluon-config/Cargo.toml`). The walk is bounded by the
    // filesystem — `pop()` returns false at the root.
    let mut p = std::path::PathBuf::from(&manifest);
    let resolved = loop {
        if p.join("crates/gluon-config/Cargo.toml").is_file() {
            break p.clone();
        }
        if !p.pop() {
            // No ancestor contained the marker — fall back to the
            // manifest dir. The runtime helper validates this and
            // surfaces a clear error if the marker is missing.
            break std::path::PathBuf::from(&manifest);
        }
    };

    println!(
        "cargo:rustc-env=GLUON_CONFIG_WORKSPACE_DIR={}",
        resolved.display()
    );
}
