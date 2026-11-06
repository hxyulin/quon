// Gluon build for Gluon itself: a host tool, so the profile builds for
// the host and `gluon run` runs the binary directly.
//
//     gluon build              build the gluon binary
//     gluon run -- <args>      build and run it
//     gluon test               run every package's unit tests, the tests
//                              in tests/ and the doctests
//
// The end-to-end tests marked #[ignore] still run only under
// `cargo nextest run --workspace --run-ignored all`, since `gluon test`
// passes nothing but the filter to libtest.

use gluon_config::{Config, Result};

/// Workspace packages whose tests `gluon test` runs.
const PACKAGES: &[&str] = &[
    "gluon-model",
    "gluon-config",
    "gluon-core",
    "gluon-runner",
    "gluon-runner-qemu",
    "gluon-cli",
];

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("gluon", "0.1.1");
    cfg.cargo_workspace(".");
    cfg.group("tools").target_host().package("gluon-cli");
    cfg.profile("dev")
        .target_host()
        .opt_level(0)
        .debug_info(true)
        .run_binary("gluon");

    // The tests build gluon.rs scripts, which link a gluon-config library
    // that Gluon builds into $GLUON_HOME. Keep them out of ~/.gluon, as
    // .cargo/config.toml does under cargo.
    let mut tests = cfg.host_tests().env_path("GLUON_HOME", "build/gluon-home");
    for name in PACKAGES {
        tests = tests.package(name);
    }
    drop(tests);
    Ok(())
}
