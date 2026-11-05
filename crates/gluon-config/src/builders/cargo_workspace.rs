//! Chainable builder returned by [`crate::Config::cargo_workspace`].

use crate::config::Config;

/// Chainable builder returned by [`Config::cargo_workspace`]. Its
/// methods do nothing when the workspace was already declared; the
/// second declaration is reported as a diagnostic.
pub struct CargoWorkspaceBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    /// False for a repeated `cargo_workspace` call.
    pub(crate) active: bool,
}

impl CargoWorkspaceBuilder<'_> {
    /// Read registry sources from `dir` (relative to the project root),
    /// filled by `gluon vendor`, instead of Cargo's registry cache. A
    /// build then runs no Cargo command and fails if `Cargo.lock`
    /// changed since the last `gluon vendor`.
    pub fn vendored(self, dir: &str) -> Self {
        if self.active
            && let Some(ws) = &mut self.cfg.raw.cargo_workspace
        {
            ws.vendored = Some(dir.into());
        }
        self
    }
}
