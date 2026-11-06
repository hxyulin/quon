// Quon build for Quon itself: a host tool, so the profile builds for
// the host and `quon run` runs the binary directly.
//
//     quon build              build the quon binary
//     quon run -- <args>      build and run it
//     quon test               run every package's unit tests, the tests
//                              in tests/ and the doctests
//
// The end-to-end tests marked #[ignore] still run only under
// `cargo nextest run --workspace --run-ignored all`, since `quon test`
// passes nothing but the filter to libtest.

use quon_config::{Config, Result};

/// Workspace packages whose tests `quon test` runs.
const PACKAGES: &[&str] = &[
    "quon-model",
    "quon-config",
    "quon-core",
    "quon-runner",
    "quon-runner-qemu",
    "quon",
];

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("quon", "0.1.1");
    cfg.cargo_workspace(".");
    cfg.group("tools").target_host().package("quon");
    cfg.profile("dev")
        .target_host()
        .opt_level(0)
        .debug_info(true)
        .run_binary("quon");

    // The tests build quon.rs scripts, which link a quon-config library
    // that Quon builds into $QUON_HOME. Keep them out of ~/.quon, as
    // .cargo/config.toml does under cargo.
    let mut tests = cfg.host_tests().env_path("QUON_HOME", "build/quon-home");
    for name in PACKAGES {
        tests = tests.package(name);
    }
    drop(tests);
    Ok(())
}
