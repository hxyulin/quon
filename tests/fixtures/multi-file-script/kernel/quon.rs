//! Kernel subsystem registration. The integration test edits the V1
//! body of `register()` between two runs, renaming the crate and so
//! its group; the cache-invalidation assertion lives in
//! `crates/quon-core/tests/script_runner_multi_file.rs`.

use quon_config::prelude::*;

pub fn register(q: &mut Quon) -> Result<()> {
    q.add(Crate::lib("kernel-v1", "kernel").edition(Edition::E2024));
    Ok(())
}
