// Sub-project #3 fixture — exercises the `dependency()` Rhai builder,
// `vendor_sync`, and `auto_register_vendored_deps` end-to-end.
//
// Two external dependencies:
//
//   - `vendor_minimal_helper` via `.path(...)`. The sibling path target
//     lives at `../vendor-minimal-helper/`. Exercises the DepSource::Path
//     branch of every vendor module — no network access required.
//
//   - `bitflags` via `.version("2.11")`. Exercises the DepSource::CratesIo
//     branch, which shells out to `cargo vendor`. Only evaluated by the
//     network-gated test; the path-only test ignores this dep by loading
//     a version of the model that never got this line evaluated (see
//     `quon.path-only.rhai`).
//
// This fixture intentionally declares no kernel crates: sub-project #3
// does not cross-compile vendored deps, so a full `quon build` run is
// out of scope. The end-to-end test only drives evaluate → vendor_sync →
// auto_register and inspects the resulting model.

use quon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("vendor-minimal", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true);

    cfg.dependency("vendor_minimal_helper").path("../vendor-minimal-helper");
    cfg.dependency("bitflags").version("2.11");

    Ok(())
}
