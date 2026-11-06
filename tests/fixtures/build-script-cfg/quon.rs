// Build-script fixture: every package is a path package with a build
// script, except `tool`.
//
//   - `probe` (`links = "probe"`) sets a cfg, its check-cfg, an env var
//     and `links` metadata, warns, and writes `$OUT_DIR/gen.rs` from
//     `probe.txt`, its only `rerun-if-changed` input.
//   - `consumer` reads probe's metadata as `DEP_PROBE_KEY` and fails
//     unless it is `42`.
//   - `kernel` sets its linker script with `rustc-link-arg`, as a
//     kernel migrated from Cargo does.
//   - `tool` is a host root, so `consumer` and `probe` build for the
//     host and for the kernel's target, and each script runs twice.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("build-script-cfg", "0.1.0");
    q.cargo_workspace(".");
    // Profiles build for the kernel's target, the one non-host target.
    q.add(Package::new("kernel").target(&Target::builtin("x86_64-unknown-none")));
    q.add(Package::new("tool"));
    Ok(())
}
