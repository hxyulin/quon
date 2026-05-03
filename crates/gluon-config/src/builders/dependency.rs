//! Chainable builder for an `ExternalDepDef` declared via
//! [`crate::Config::dependency`].

use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{DepSource, ExternalDepDef, GitRef, Handle};

/// Chainable builder returned by [`Config::dependency`].
pub struct DependencyBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<ExternalDepDef>>,
}

impl<'cfg> DependencyBuilder<'cfg> {
    fn modify<F: FnOnce(&mut ExternalDepDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(d) = self.cfg.raw.external_deps.get_mut(*handle) {
                f(d);
            }
        }
    }

    /// crates.io with exact version (e.g. `"2.11"`). Replaces any
    /// previously-set source (last-call wins, matching the engine).
    pub fn version(mut self, version: &str) -> Self {
        let v = version.to_string();
        self.modify(|d| d.source = DepSource::CratesIo { version: v });
        self
    }

    /// Local path on disk (relative to the project root).
    pub fn path(mut self, path: &str) -> Self {
        let p = path.to_string();
        self.modify(|d| d.source = DepSource::Path { path: p });
        self
    }

    /// Git source (defaults to the repo's default branch). Refine
    /// with `.rev`, `.tag`, or `.branch` to pin.
    pub fn git(mut self, url: &str) -> Self {
        let u = url.to_string();
        self.modify(|d| {
            d.source = DepSource::Git {
                url: u,
                reference: GitRef::Default,
            };
        });
        self
    }

    /// Pin a Git source to a specific commit. Pushes a diagnostic if
    /// `.git(...)` has not been called first.
    pub fn rev(self, rev: &str) -> Self {
        self.set_git_ref(GitRef::Rev(rev.into()))
    }
    pub fn tag(self, tag: &str) -> Self {
        self.set_git_ref(GitRef::Tag(tag.into()))
    }
    pub fn branch(self, branch: &str) -> Self {
        self.set_git_ref(GitRef::Branch(branch.into()))
    }

    fn set_git_ref(self, new_ref: GitRef) -> Self {
        if self.state.is_duplicate() {
            return self;
        }
        let name = self.state.name().to_string();
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(d) = self.cfg.raw.external_deps.get_mut(*handle) {
                if let DepSource::Git { reference, .. } = &mut d.source {
                    *reference = new_ref;
                    return self;
                }
            }
        }
        // Fall through: not a Git source. Mirror the engine's wording.
        self.cfg.diagnostics.push(ConfigError::new(format!(
            "dependency '{name}': .rev/.tag/.branch requires a .git(...) call first"
        )));
        self
    }

    /// Replace the feature list.
    pub fn features(mut self, features: &[&str]) -> Self {
        let v: Vec<String> = features.iter().map(|s| (*s).to_string()).collect();
        self.modify(|d| d.features = v);
        self
    }

    /// Suppress default features. Mirrors the engine's
    /// `.no_default_features()` (no argument) — calling it sets the
    /// flag to false and there is no way to re-enable it on the same
    /// dep.
    pub fn no_default_features(mut self) -> Self {
        self.modify(|d| d.default_features = false);
        self
    }

    pub fn cfg_flags(mut self, flags: &[&str]) -> Self {
        let v: Vec<String> = flags.iter().map(|s| (*s).to_string()).collect();
        self.modify(|d| d.cfg_flags = v);
        self
    }

    pub fn rustc_flags(mut self, flags: &[&str]) -> Self {
        let v: Vec<String> = flags.iter().map(|s| (*s).to_string()).collect();
        self.modify(|d| d.rustc_flags = v);
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use gluon_model::{DepSource, GitRef};

    #[test]
    fn dependency_path_records_path_source() {
        let mut cfg = Config::new();
        cfg.dependency("helper").path("../helper");
        let h = cfg.raw.external_deps.lookup("helper").unwrap();
        let d = cfg.raw.external_deps.get(h).unwrap();
        match &d.source {
            DepSource::Path { path } => assert_eq!(path, "../helper"),
            other => panic!("expected Path, got {other:?}"),
        }
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn dependency_version_records_crates_io() {
        let mut cfg = Config::new();
        cfg.dependency("bitflags").version("2.11");
        let h = cfg.raw.external_deps.lookup("bitflags").unwrap();
        let d = cfg.raw.external_deps.get(h).unwrap();
        match &d.source {
            DepSource::CratesIo { version } => assert_eq!(version, "2.11"),
            other => panic!("expected CratesIo, got {other:?}"),
        }
    }

    #[test]
    fn dependency_git_with_rev() {
        let mut cfg = Config::new();
        cfg.dependency("foo").git("https://example.com/foo.git").rev("abcdef");
        let h = cfg.raw.external_deps.lookup("foo").unwrap();
        let d = cfg.raw.external_deps.get(h).unwrap();
        match &d.source {
            DepSource::Git { url, reference: GitRef::Rev(r) } => {
                assert_eq!(url, "https://example.com/foo.git");
                assert_eq!(r, "abcdef");
            }
            other => panic!("expected Git+Rev, got {other:?}"),
        }
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn dependency_rev_without_git_diagnostic() {
        let mut cfg = Config::new();
        cfg.dependency("foo").rev("abcdef");
        assert!(cfg
            .diagnostics
            .iter()
            .any(|d| d.message.contains(".rev/.tag/.branch requires a .git(...) call first")));
    }

    #[test]
    fn duplicate_dependency_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.dependency("a").version("1.0");
        cfg.dependency("a").version("2.0");
        let h = cfg.raw.external_deps.lookup("a").unwrap();
        let d = cfg.raw.external_deps.get(h).unwrap();
        match &d.source {
            DepSource::CratesIo { version } => assert_eq!(version, "1.0"),
            other => panic!("expected CratesIo from first definition, got {other:?}"),
        }
        assert_eq!(cfg.diagnostics.len(), 1);
    }
}
