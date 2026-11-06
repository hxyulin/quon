// Manifest build fixture: the kernel and everything it depends on come
// from the Cargo workspace in this directory. gluon.rs names only the
// root package and the settings Cargo has no concept of.
//
// The workspace covers, in one small build:
//   - a package with both a lib and a bin (`kernel`, so its lib is the
//     unit `kernel@lib`),
//   - a renamed path dependency (`helpers = { package = "util-helpers" }`),
//   - `[workspace.dependencies]` with `default-features = false` and a
//     member-side feature,
//   - a proc macro (`fixture-derive`) whose dependency `util` is built a
//     second time for the host (`util@host`),
//   - `[workspace.package]` inheritance read through `CARGO_PKG_*`,
//   - `[workspace.lints]`, including `unexpected_cfgs` check-cfg.

use gluon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("cargo-build", "0.1.0");
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
