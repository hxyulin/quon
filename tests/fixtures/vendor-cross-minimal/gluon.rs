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

use gluon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("vendor-cross-minimal", "0.1.0");

    let kernel = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(kernel)
        .opt_level(0)
        .debug_info(true);

    cfg.dependency("bitflags").version("2.11").target(kernel);

    Ok(())
}
