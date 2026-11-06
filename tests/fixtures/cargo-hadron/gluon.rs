// Hadron-shaped workspace: a kernel on `x86_64-unknown-none` whose
// parser crate derives zerocopy traits and a path proc macro's trait.
// gluon.rs names only the root package; everything else comes from the
// Cargo workspace.

use gluon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("cargo-hadron", "0.1.0");
    let target = cfg.target("x86_64-unknown-none");
    cfg.cargo_workspace(".");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .run_binary("kernel");

    cfg.group("kernel")
        .target(target)
        .package("kernel")
        .linker_script("crates/kernel/kernel/kernel.ld");

    Ok(())
}
