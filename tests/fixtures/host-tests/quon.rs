// Host-test fixture: `calc`'s unit tests and its `tests/api.rs` build
// for the host with `--test` and run there. `helper` is a
// dev-dependency, so only the tests link it. Nothing builds for the
// target, so the fixture needs no sysroot.

use quon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("host-tests", "0.1.0");
    let target = cfg.target("x86_64-unknown-none");
    cfg.cargo_workspace(".");
    cfg.profile("dev").target(target).opt_level(0).debug_info(true);
    cfg.host_tests().package("calc");
    Ok(())
}
