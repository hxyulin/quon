//! `quon test` suites.

use crate::profile::ProfileRef;
use crate::quon::{Declare, Quon};
use quon_model::HostTestEnv;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// A boot test: `quon test` builds `profile`, runs it with the run
/// entry's runner, and passes when the runner reports success. The
/// kernel decides what to run, typically from a config option that the
/// profile's preset sets.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// let ktest = q.add(opt::bool("KTEST"));
/// let on = q.add(Preset::new("ktest").set(&ktest, true));
/// let dev = q.profile("dev");
/// let test = q.add(Profile::new("test").inherits(&dev).preset(&on));
/// q.add(BootTest::new("ktest", &test));
/// ```
#[derive(Debug, Clone)]
#[must_use = "a boot test does nothing until it is passed to `q.add`"]
pub struct BootTest {
    pub(crate) name: String,
    pub(crate) profile: String,
}

impl BootTest {
    pub fn new(name: impl Into<String>, profile: &ProfileRef) -> Self {
        Self {
            name: name.into(),
            profile: profile.name.clone(),
        }
    }
}

impl Declare for BootTest {
    type Handle = ();
    fn declare(self, q: &mut Quon) {
        q.add_boot_test(self);
    }
}

/// The workspace packages whose tests `quon test` runs on the host,
/// from [`Quon::host_tests`].
#[derive(Debug, Default)]
pub struct HostTests {
    pub(crate) packages: BTreeSet<String>,
    pub(crate) features: BTreeMap<String, BTreeSet<String>>,
    pub(crate) env: BTreeMap<String, HostTestEnv>,
}

impl HostTests {
    /// Build and run package `name`'s unit tests, its doctests, and
    /// its `harness = true` tests in `tests/`, against the host's
    /// standard library. The package must build for the host. A target
    /// with `test = false` in `Cargo.toml` is left out, as `cargo test`
    /// leaves it out.
    ///
    /// The packages added here resolve Cargo features together, as
    /// `cargo test -p a -p b` does.
    pub fn package(&mut self, name: impl Into<String>) -> &mut Self {
        self.packages.insert(name.into());
        self
    }

    /// Like [`package`](Self::package), with `features` of package
    /// `name` enabled. The package resolves features on its own, as
    /// `cargo test -p <name> --features <features>` does, so two such
    /// packages can enable features of a shared dependency that exclude
    /// each other, such as two chip features of one HAL. A dependency
    /// that two resolutions give the same features builds once.
    ///
    /// ```
    /// # use quon_config::prelude::*;
    /// # let mut q = Quon::new();
    /// q.host_tests()
    ///     .package("protocol")
    ///     .package_with_features("board-a", ["host"])
    ///     .package_with_features("board-b", ["host"]);
    /// ```
    pub fn package_with_features<S: AsRef<str>>(
        &mut self,
        name: impl Into<String>,
        features: impl IntoIterator<Item = S>,
    ) -> &mut Self {
        let name = name.into();
        self.features
            .entry(name.clone())
            .or_default()
            .extend(features.into_iter().map(|f| f.as_ref().to_owned()));
        self.packages.insert(name);
        self
    }

    pub fn packages<S: AsRef<str>>(&mut self, names: impl IntoIterator<Item = S>) -> &mut Self {
        self.packages
            .extend(names.into_iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// Set `var` to `value` when the tests run. It is not set when they
    /// compile, so `env!` does not see it.
    pub fn env(&mut self, var: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.env
            .insert(var.into(), HostTestEnv::Value(value.into()));
        self
    }

    /// Set `var` to `path`, made absolute against the project root,
    /// since each test runs in its own package directory.
    pub fn env_path(&mut self, var: impl Into<String>, path: impl Into<PathBuf>) -> &mut Self {
        self.env.insert(var.into(), HostTestEnv::Path(path.into()));
        self
    }
}
