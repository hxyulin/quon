// Quon build for Quon itself: a host tool, so the profiles build for
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

use quon_config::prelude::*;

/// Workspace packages whose tests `quon test` runs.
const PACKAGES: &[&str] = &[
    "quon-model",
    "quon-config",
    "quon-core",
    "quon-runner",
    "quon-runner-qemu",
    "quon",
];

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("quon", "0.1.1");
    q.cargo_workspace(".");
    let quon = q.add(Package::new("quon"));
    q.run(&quon);

    // The tests build quon.rs scripts, which link a quon-config library
    // that Quon builds into $QUON_HOME. Keep them out of ~/.quon, as
    // .cargo/config.toml does under cargo.
    q.host_tests()
        .packages(PACKAGES)
        .env_path("QUON_HOME", "build/quon-home");
    Ok(())
}
