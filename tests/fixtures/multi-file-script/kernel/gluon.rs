//! Kernel subsystem registration. The integration test edits the V1
//! body of `register()` between two runs (renaming the group); the
//! cache-invalidation assertion lives in
//! `crates/gluon-core/tests/script_runner_multi_file.rs`.
//!
//! The target handle is owned by the root `gluon.rs` and passed in —
//! see the comment on `bootloader/gluon.rs::register` for the rationale.

use gluon_config::{Config, Handle, Result, TargetDef};

pub fn register(cfg: &mut Config, target: Handle<TargetDef>) -> Result<()> {
    cfg.group("kernel-v1").target(target).edition("2021");
    Ok(())
}
