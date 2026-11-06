// Vendored crates with build scripts, built offline.
//
// The crates under `vendor/` stand in for crates.io packages. The test
// writes the cross vendor workspace for `x86_64-unknown-none` and points
// it at that directory, so `cargo metadata` resolves them without a
// network, the way it resolves a `quon vendor` tree.
//
// - `pm2` and `qt` are shaped like `proc-macro2` and `quote`: libraries
//   of a proc macro, with build scripts that probe `$RUSTC`.
// - `hello-derive` is a proc macro with a build-dependency.
// - `probe-sys` declares `links = "probe"` and passes metadata to
//   `consumer`'s build script.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("vendor-cross-build-script", "0.1.0");

    let kernel = Target::builtin("x86_64-unknown-none");
    q.default_target(&kernel);

    let hello_derive = q.add(Dependency::crates_io("hello-derive", "0.1").target(&kernel));
    let consumer = q.add(Dependency::crates_io("consumer", "0.1").target(&kernel));

    q.add(
        Crate::lib("kernel", "crates/kernel")
            .edition(Edition::E2021)
            .dep(&hello_derive)
            .dep(&consumer),
    );

    Ok(())
}
