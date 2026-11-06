use quon_config::prelude::*;

#[path = "bootloader/quon.rs"]
mod bootloader;
#[path = "kernel/quon.rs"]
mod kernel;
#[path = "shared/quon.rs"]
mod shared;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("multi-file-script", "0.1.0");
    // Set once here, so the subsystems need not agree on a target.
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    shared::register(q)?;
    bootloader::register(q)?;
    kernel::register(q)?;

    Ok(())
}
