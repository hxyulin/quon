// Registry dependency fixture: the kernel depends on the registry
// package `fakeflags`. Its sources come from Cargo's registry cache by
// default; the tests point Cargo at `registry/`, a directory source, so
// no network is needed. Tests that switch to vendor mode append
// `.vendored("vendor")` to the `cargo_workspace` line.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("cargo-registry", "0.1.0");
    q.cargo_workspace(".");
    q.default_target(&Target::builtin("x86_64-unknown-none"));
    let kernel = q.add(Package::new("kernel").linker_script("crates/kernel/kernel.ld"));
    q.run(&kernel);
    Ok(())
}
