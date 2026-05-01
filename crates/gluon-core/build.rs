//! Build script: bake the gluon workspace path into the crate so the
//! `script_runner` can locate `crates/gluon-config` at runtime without
//! a config file.
//!
//! The path is resolved at compile time by walking up from `CARGO_MANIFEST_DIR`
//! until we find a parent that contains `crates/gluon-config/Cargo.toml`. This
//! works for `cargo build`, `cargo test`, and any in-tree invocation.
//!
//! Limitations:
//!
//! - After `cargo install gluon-cli`, the baked path points at the build
//!   machine's checkout — which won't exist on the user's machine. That's
//!   why `script_runner::rlib::workspace_dir` consults the
//!   `GLUON_CONFIG_SOURCE_DIR` env var first: install-time deployments must
//!   set that variable (or pre-build the rlib via the Stage 1.7 follow-up).
//!
//! - If the manifest dir is somehow not nested under the workspace (unusual
//!   custom layouts), the fallback bakes in the manifest dir itself — which
//!   will fail at runtime with a clear error message rather than crash.

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
