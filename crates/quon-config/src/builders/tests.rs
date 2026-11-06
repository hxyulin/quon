//! Builders returned by [`crate::Config::host_tests`] and
//! [`crate::Config::boot_test`].

use crate::config::Config;
use quon_model::RawBootTestDef;
use quon_model::{Handle, HostTestEnv};

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

    /// Set `name` to `value` for every host test run. The variable is
    /// set when the tests run, not when they compile, so `env!` does not
    /// see it.
    pub fn env(self, name: &str, value: &str) -> Self {
        self.cfg
            .raw
            .host_test_env
            .insert(name.into(), HostTestEnv::Value(value.into()));
        self
    }

    /// Set `name` to `path` for every host test run, made absolute
    /// against the project root, since each test runs from its own
    /// package directory.
    pub fn env_path(self, name: &str, path: impl Into<std::path::PathBuf>) -> Self {
        self.cfg
            .raw
            .host_test_env
            .insert(name.into(), HostTestEnv::Path(path.into()));
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

#[cfg(test)]
mod unit_tests {
    use crate::Config;
    use quon_model::HostTestEnv;

    #[test]
    fn host_test_env_records_values_and_paths() {
        let mut cfg = Config::new();
        cfg.host_tests()
            .package("tool")
            .env("RUST_LOG", "debug")
            .env_path("TOOL_HOME", "build/home");
        let env = &cfg.raw.host_test_env;
        assert_eq!(env["RUST_LOG"], HostTestEnv::Value("debug".into()));
        assert_eq!(env["TOOL_HOME"], HostTestEnv::Path("build/home".into()));
        assert!(cfg.raw.host_tests.contains("tool"));
    }
}
