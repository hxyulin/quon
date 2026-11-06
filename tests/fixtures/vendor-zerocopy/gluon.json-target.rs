// `gluon.rs` with a custom JSON target instead of the builtin triple.
//
// zerocopy has a build script that runs for the target unit.
// zerocopy-derive is a proc macro, so it and the syn stack it uses
// (proc-macro2 and quote have build scripts) are host units.
// Network-gated: `cargo vendor` fetches from crates.io.

use gluon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("vendor-zerocopy", "0.1.0");

    // The test writes this spec from `rustc --print target-spec-json`.
    let kernel = cfg.target_spec("x86_64-gluon-test", "targets/x86_64-gluon-test.json");

    cfg.profile("dev")
        .target(kernel)
        .opt_level(0)
        .debug_info(true);

    cfg.dependency("zerocopy")
        .version("0.8")
        .features(&["derive"])
        .target(kernel);

    cfg.group("kernel")
        .target(kernel)
        .edition("2021")
        .add("kernel", "crates/kernel")
        .dep("zerocopy", "zerocopy");

    Ok(())
}
