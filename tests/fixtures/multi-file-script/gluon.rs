use gluon_config::{Config, Result};

#[path = "bootloader/gluon.rs"]
mod bootloader;
#[path = "kernel/gluon.rs"]
mod kernel;

fn main() -> Result<()> {
    let mut cfg = Config::new();
    cfg.project("multi-file-script", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");
    cfg.profile("dev").target(target).opt_level(0).debug_info(true);

    // Subsystems take the target handle by value — the Config API treats
    // `cfg.target(...)` as a declaration, so threading the handle through
    // is the natural way to share a single target across modules.
    bootloader::register(&mut cfg, target)?;
    kernel::register(&mut cfg, target)?;

    cfg.emit()
}
