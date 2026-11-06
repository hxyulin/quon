// Vendored crates with build scripts, built offline.
//
// The crates under `vendor/` stand in for crates.io packages. The test
// writes the cross vendor workspace for `x86_64-unknown-none` and points
// it at that directory, so `cargo metadata` resolves them without a
// network, the way it resolves a `quon vendor` tree.
//
// - `pm2` and `qt` are shaped like `proc-macro2` and `quote`: libraries
//   of a proc macro, with build scripts that probe `$RUSTC`.
// - `hello-derive` is a proc macro with a build-dependency.
// - `probe-sys` declares `links = "probe"` and passes metadata to
//   `consumer`'s build script.

use quon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("vendor-cross-build-script", "0.1.0");

    let kernel = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(kernel)
        .opt_level(0)
        .debug_info(true);

    cfg.dependency("hello-derive").version("0.1").target(kernel);
    cfg.dependency("consumer").version("0.1").target(kernel);

    cfg.group("kernel")
        .target(kernel)
        .edition("2021")
        .add("kernel", "crates/kernel")
        .dep("hello_derive", "hello-derive")
        .dep("consumer", "consumer");

    Ok(())
}
