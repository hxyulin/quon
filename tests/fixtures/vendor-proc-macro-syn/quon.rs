// A kernel crate that uses a syn-based proc macro from crates.io
// directly, as hadron's crates use their derive crates.
//
// bytemuck_derive is a proc macro, so it and the syn stack it uses
// (proc-macro2 and quote have build scripts) are host units; the kernel
// gets only `--extern bytemuck_derive=<host dylib>`.
// Network-gated: `cargo vendor` fetches from crates.io.

use quon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("vendor-proc-macro-syn", "0.1.0");

    let kernel = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(kernel)
        .opt_level(0)
        .debug_info(true);

    cfg.dependency("bytemuck").version("1").target(kernel);
    cfg.dependency("bytemuck_derive").version("1").target(kernel);

    cfg.group("kernel")
        .target(kernel)
        .edition("2021")
        .add("kernel", "crates/kernel")
        .dep("bytemuck", "bytemuck")
        .dep("bytemuck_derive", "bytemuck_derive");

    Ok(())
}
