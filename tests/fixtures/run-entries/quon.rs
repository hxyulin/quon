// Run-entry fixture: two boards enable chip features of one HAL that
// exclude each other. Each entry resolves features alone, so both
// build; one resolution over both packages would fail to compile
// `chip`. Everything builds for the host, so no sysroot is needed.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("run-entries", "0.1.0");
    q.cargo_workspace(".");
    let a = q.add(Package::new("board-a"));
    let b = q.add(Package::new("board-b"));
    q.run(&a).name("a");
    q.run(&b).name("b");
    q.host_tests().package("chip");
    Ok(())
}
