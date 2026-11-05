//! Where registry packages' sources live.
//!
//! The workspace loader does not know how registry sources were
//! obtained. It takes a [`RegistrySources`] map from `(name, version)` to
//! the source directory, and reads `<dir>/Cargo.toml`. A vendor
//! directory is the only backend today; Cargo's registry cache is a
//! later backend that fills the same map.

use super::lock::Lockfile;
use crate::error::Result;
use semver::Version;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct RegistrySources {
    dirs: BTreeMap<(String, Version), PathBuf>,
    /// Appended to "source missing" diagnostics: what the user runs to
    /// fill the source location.
    hint: String,
}

impl RegistrySources {
    pub fn new(dirs: BTreeMap<(String, Version), PathBuf>, hint: impl Into<String>) -> Self {
        Self {
            dirs,
            hint: hint.into(),
        }
    }

    /// Sources vendored by `gluon vendor` into `vendor_dir`:
    /// `<vendor_dir>/<name>-<version>/` for every registry package in
    /// `<workspace_root>/Cargo.lock` (`gluon vendor` passes
    /// `--versioned-dirs`). A missing lock yields an empty map; the
    /// loader reports the missing lock itself.
    pub fn vendor_dir(workspace_root: &Path, vendor_dir: &Path) -> Result<Self> {
        let lock_path = workspace_root.join("Cargo.lock");
        let mut dirs = BTreeMap::new();
        if lock_path.exists() {
            let lock = Lockfile::load(&lock_path)?;
            for p in lock.packages.iter().filter(|p| p.is_registry()) {
                let dir = vendor_dir.join(format!("{}-{}", p.name, p.version));
                dirs.insert((p.name.clone(), p.version.clone()), dir);
            }
        }
        Ok(Self::new(dirs, "run `gluon vendor`"))
    }

    /// The source directory of `name@version`, or the reason there is
    /// none.
    pub fn dir(&self, name: &str, version: &Version) -> std::result::Result<&Path, String> {
        let dir = self
            .dirs
            .get(&(name.to_string(), version.clone()))
            .ok_or_else(|| format!("no source location for `{name} {version}`; {}", self.hint))?;
        if !dir.join("Cargo.toml").is_file() {
            return Err(format!(
                "source of `{name} {version}` is missing at {}; {}",
                dir.display(),
                self.hint
            ));
        }
        Ok(dir)
    }
}
