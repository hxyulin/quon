//! Shared subsystem registration. A third subsystem shows the
//! multi-file pattern scales beyond two: the integration test asserts
//! `shared-utils` is present on every run, so a regression that broke
//! this `register()` call would surface immediately.

use quon_config::prelude::*;

pub fn register(q: &mut Quon) -> Result<()> {
    q.add(Crate::lib("shared-utils", "shared").edition(Edition::E2024));
    Ok(())
}
