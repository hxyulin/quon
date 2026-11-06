//! Bootloader subsystem registration, through the `register()`
//! convention: each subsystem file adds its own crates to the `Quon`
//! the root `quon.rs` passes in.

use quon_config::prelude::*;

pub fn register(q: &mut Quon) -> Result<()> {
    q.add(Crate::lib("bootloader", "bootloader").edition(Edition::E2024));
    Ok(())
}
