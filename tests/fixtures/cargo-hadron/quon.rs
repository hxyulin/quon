// Hadron-shaped workspace: a kernel on `x86_64-unknown-none` whose
// parser crate derives zerocopy traits and a path proc macro's trait.
// quon.rs names only the root package; everything else comes from the
// Cargo workspace.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("cargo-hadron", "0.1.0");
    q.cargo_workspace(".");
    q.default_target(&Target::builtin("x86_64-unknown-none"));
    let kernel = q.add(Package::new("kernel").linker_script("crates/kernel/kernel/kernel.ld"));
    q.run(&kernel);
    Ok(())
}
