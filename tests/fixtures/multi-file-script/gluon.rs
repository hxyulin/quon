use gluon_config::{Config, Result};

#[path = "bootloader/gluon.rs"]
mod bootloader;
#[path = "kernel/gluon.rs"]
mod kernel;
#[path = "shared/gluon.rs"]
mod shared;

fn main() -> Result<()> {
    let mut cfg = Config::new();
    cfg.project("multi-file-script", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");
    cfg.profile("dev").target(target).opt_level(0).debug_info(true);

    shared::register(&mut cfg)?;
    bootloader::register(&mut cfg)?;
    kernel::register(&mut cfg)?;

    cfg.emit()
}
