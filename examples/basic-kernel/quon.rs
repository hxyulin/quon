// examples/basic-kernel/quon.rs
//
// Minimal bare-metal kernel project: one target and one package from
// the Cargo workspace in this directory, booted under QEMU. Run with:
//
//     quon build
//     quon run
//
// The kernel's edition, source path and dependencies come from
// crates/kernel/Cargo.toml. quon.rs holds only what Cargo has no
// concept of: the target, the linker script and QEMU.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("my-kernel", "0.1.0");

    // The directory holding the workspace Cargo.toml, relative to
    // this file.
    q.cargo_workspace(".");

    // Every crate builds for this target unless it names its own.
    // `Target::spec(name, path)` points at a custom JSON spec instead.
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    // Builds the workspace package: its lib and bins, plus everything
    // they depend on.
    let kernel = q.add(Package::new("kernel").linker_script("crates/kernel/kernel.ld"));

    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(128)
            .cores(1)
            .serial(Serial::Stdio),
    );

    // `quon run` boots the kernel under QEMU. The built-in `dev` and
    // `release` profiles build for its target.
    q.run(&kernel).runner(&qemu);
    Ok(())
}
