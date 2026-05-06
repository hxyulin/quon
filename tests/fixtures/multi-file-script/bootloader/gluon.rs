//! Bootloader subsystem registration. Exercises the `register()` convention.
//!
//! The target handle is owned by the root `gluon.rs` (which also wires
//! it into the profile) and threaded through here — the Config API
//! treats `target("...")` as a declaration, not a lookup, so calling it
//! a second time would emit a "defined more than once" diagnostic.

use gluon_config::{Config, Handle, Result, TargetDef};

pub fn register(cfg: &mut Config, target: Handle<TargetDef>) -> Result<()> {
    cfg.group("bootloader").target(target).edition("2021");
    Ok(())
}
