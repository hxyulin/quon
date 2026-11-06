// Path-only variant of the vendor-minimal fixture.
//
// Used by the no-network integration test in
// `crates/quon-core/tests/vendor_e2e.rs`. Identical to `quon.rs`
// except the `bitflags` crates.io dep is omitted, so `vendor_sync`
// does not need to shell out to `cargo vendor`.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("vendor-minimal", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    q.add(Dependency::path("vendor_minimal_helper", "../vendor-minimal-helper"));

    Ok(())
}
