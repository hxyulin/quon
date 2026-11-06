// Host-test fixture for per-package features: `board-a` and `board-b`
// turn on chip features of `chip` that exclude each other, so their
// tests only build when each resolves on its own. Each board's bin is
// firmware with `test = false`, which does not compile for the host.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("host-test-features", "0.1.0");
    q.cargo_workspace(".");
    q.host_tests()
        .package_with_features("board-a", ["host"])
        .package_with_features("board-b", ["host"]);
    Ok(())
}
