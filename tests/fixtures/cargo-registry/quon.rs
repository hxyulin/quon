// Registry dependency fixture: the kernel depends on the registry
// package `fakeflags`. Its sources come from Cargo's registry cache by
// default; the tests point Cargo at `registry/`, a directory source, so
// no network is needed. Tests that switch to vendor mode append
// `.vendored("vendor")` to the `cargo_workspace` line.

use quon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("cargo-registry", "0.1.0");
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
        .linker_script("crates/kernel/kernel.ld");

    Ok(())
}
