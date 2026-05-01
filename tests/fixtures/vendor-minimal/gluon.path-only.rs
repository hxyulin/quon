// Path-only variant of the vendor-minimal fixture.
//
// Used by the no-network integration test in
// `crates/gluon-core/tests/vendor_e2e.rs`. Identical to `gluon.rhai`
// except the `bitflags` crates.io dep is omitted, so `vendor_sync`
// does not need to shell out to `cargo vendor`.

use gluon_config::{Config, Result};

fn main() -> Result<()> {
    let mut cfg = Config::new();

    cfg.project("vendor-minimal", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true);

    cfg.dependency("vendor_minimal_helper").path("../vendor-minimal-helper");

    cfg.emit()
}
