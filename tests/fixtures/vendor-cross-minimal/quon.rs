// Cross-target vendored builds fixture.
//
// Declares a single cross-tagged dep — `bitflags` 2.11 — pinned to a
// kernel target. Exercises the full V2 cross pipeline:
//
//   load_build_model
//     → populate_cross_vendor_workspaces (cargo vendor into
//       build/vendor-workspaces/x86_64-unknown-none/vendor/)
//     → build_cross_vendor_context (cargo metadata + graph lowering)
//     → VendorBuildContext entry for bitflags @ Cross(x86_64-unknown-none)
//
// No host-tagged deps and no path deps: keeps the fixture focused on
// the cross pipeline and avoids any interaction with the legacy host
// `vendor_sync` path. Network-gated only — `cargo vendor` and
// `cargo metadata` both shell out for real against crates.io.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("vendor-cross-minimal", "0.1.0");

    let kernel = Target::builtin("x86_64-unknown-none");
    q.add(Dependency::crates_io("bitflags", "2.11").target(&kernel));

    Ok(())
}
