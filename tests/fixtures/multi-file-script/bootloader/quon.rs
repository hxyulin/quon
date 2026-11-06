//! Bootloader subsystem registration. Exercises the canonical
//! `register()` convention: the subsystem looks the target up by name
//! through `cfg.target(...)`, which is idempotent (see
//! `quon-config/src/config.rs::Config::target`).

use quon_config::{Config, Result};

pub fn register(cfg: &mut Config) -> Result<()> {
    let target = cfg.target("x86_64-unknown-none");
    cfg.group("bootloader").target(target).edition("2024");
    Ok(())
}
