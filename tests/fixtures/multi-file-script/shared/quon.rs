//! Shared subsystem registration. Demonstrates that the multi-file
//! pattern scales beyond two subsystems — the integration test asserts
//! `shared-utils` is present on every run, so a regression that broke
//! the third subsystem's `register()` call would surface immediately.

use quon_config::{Config, Result};

pub fn register(cfg: &mut Config) -> Result<()> {
    let target = cfg.target("x86_64-unknown-none");
    cfg.group("shared-utils").target(target).edition("2024");
    Ok(())
}
