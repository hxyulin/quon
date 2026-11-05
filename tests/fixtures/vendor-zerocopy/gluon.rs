// zerocopy 0.8 with `derive`, vendored for a kernel target.
//
// zerocopy has a build script that runs for the target unit.
// zerocopy-derive is a proc macro, so it and the syn stack it uses
// (proc-macro2 and quote have build scripts) are host units.
// Network-gated: `cargo vendor` fetches from crates.io. The test swaps in
// `gluon.json-target.rs` to cover a custom JSON target.

use gluon_config::{Config, Result};

fn main() -> Result<()> {
    let mut cfg = Config::new();

    cfg.project("vendor-zerocopy", "0.1.0");

    let kernel = cfg.target("x86_64-unknown-none");

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

    cfg.emit()
}
