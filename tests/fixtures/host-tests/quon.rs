// Host-test fixture: `calc`'s unit tests and its `tests/api.rs` build
// for the host with `--test` and run there. `helper` is a
// dev-dependency, so only the tests link it. Nothing builds for a
// cross target, so the fixture needs no sysroot.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("host-tests", "0.1.0");
    q.cargo_workspace(".");
    q.host_tests().package("calc");
    Ok(())
}
