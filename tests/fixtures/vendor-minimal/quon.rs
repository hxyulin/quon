// Sub-project #3 fixture — exercises `Dependency` declarations,
// `vendor_sync`, and `auto_register_vendored_deps` end-to-end.
//
// Two external dependencies:
//
//   - `vendor_minimal_helper` via `Dependency::path`. The sibling path target
//     lives at `../vendor-minimal-helper/`. Exercises the DepSource::Path
//     branch of every vendor module — no network access required.
//
//   - `bitflags` via `Dependency::crates_io`. Exercises the DepSource::CratesIo
//     branch, which shells out to `cargo vendor`. Only evaluated by the
//     network-gated test; the path-only test ignores this dep by loading
//     a version of the model that never got this line evaluated (see
//     `quon.path-only.rs`).
//
// This fixture intentionally declares no kernel crates: sub-project #3
// does not cross-compile vendored deps, so a full `quon build` run is
// out of scope. The end-to-end test only drives evaluate → vendor_sync →
// auto_register and inspects the resulting model.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("vendor-minimal", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    q.add(Dependency::path("vendor_minimal_helper", "../vendor-minimal-helper"));
    q.add(Dependency::crates_io("bitflags", "2.11"));

    Ok(())
}
