//! Kernel subsystem registration. The integration test edits the V1
//! body of `register()` between two runs (renaming the group); the
//! cache-invalidation assertion lives in
//! `crates/quon-core/tests/script_runner_multi_file.rs`.
//!
//! Like `bootloader/quon.rs`, this looks the target up by name —
//! `cfg.target(...)` is idempotent for identical re-declarations, so
//! every subsystem can declare its own dependency without coordination.

use quon_config::{Config, Result};

pub fn register(cfg: &mut Config) -> Result<()> {
    let target = cfg.target("x86_64-unknown-none");
    cfg.group("kernel-v1").target(target).edition("2024");
    Ok(())
}
