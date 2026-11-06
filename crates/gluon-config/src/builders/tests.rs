//! Builders returned by [`crate::Config::host_tests`] and
//! [`crate::Config::boot_test`].

use crate::config::Config;
use gluon_model::Handle;
use gluon_model::RawBootTestDef;

/// Chainable builder returned by [`Config::host_tests`].
pub struct HostTestsBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
}

impl HostTestsBuilder<'_> {
    /// Build and run the unit tests of workspace package `name` on the
    /// host, along with its `harness = true` tests in `tests/`. The
    /// package must compile for the host triple. Listing a package
    /// twice has no further effect.
    pub fn package(self, name: &str) -> Self {
        self.cfg.raw.host_tests.insert(name.into());
        self
    }
}

/// Chainable builder returned by [`Config::boot_test`]. Its methods do
/// nothing for a repeated name; the repeat is reported as a
/// diagnostic.
pub struct BootTestBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    /// `None` for a repeated name.
    pub(crate) handle: Option<Handle<RawBootTestDef>>,
}

impl BootTestBuilder<'_> {
    /// The profile to build and boot. Required.
    pub fn profile(self, name: &str) -> Self {
        if let Some(h) = self.handle
            && let Some(t) = self.cfg.raw.boot_tests.get_mut(h)
        {
            t.profile = Some(name.into());
        }
        self
    }
}
