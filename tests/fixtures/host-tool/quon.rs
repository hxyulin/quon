// Host-tool fixture: the project names no target, so its profiles
// build for the host and `quon run` runs the binary directly.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("host-tool", "0.1.0");
    q.cargo_workspace(".");
    let hello = q.add(Package::new("hello"));
    q.run(&hello);
    Ok(())
}
