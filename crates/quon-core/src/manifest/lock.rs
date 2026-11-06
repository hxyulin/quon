//! `Cargo.lock` reader (format versions 3 and 4).
//!
//! Quon does no version resolution. `Cargo.lock`, written by Cargo,
//! records which version each registry dependency uses; Quon only
//! looks those versions up and checks them against the manifest's
//! requirement, so a stale lock is an error instead of a wrong build.

use crate::error::{Error, Result};
use semver::{Version, VersionReq};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct RawLock {
    version: Option<u32>,
    #[serde(default)]
    package: Vec<RawLockPackage>,
}

#[derive(Debug, Deserialize)]
struct RawLockPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

/// One `[[package]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockPackage {
    pub name: String,
    pub version: Version,
    /// `None` for path packages, else the source URL
    /// (`registry+https://...`, `sparse+https://...`, `git+...`).
    pub source: Option<String>,
    /// SHA-256 of the `.crate` file, for registry packages.
    pub checksum: Option<String>,
    pub dependencies: Vec<LockDepRef>,
}

impl LockPackage {
    pub fn is_registry(&self) -> bool {
        self.source
            .as_deref()
            .is_some_and(|s| s.starts_with("registry+") || s.starts_with("sparse+"))
    }

    pub fn is_git(&self) -> bool {
        self.source
            .as_deref()
            .is_some_and(|s| s.starts_with("git+"))
    }
}

/// The repository URL of a `git+<url>?<reference>#<commit>` source or
/// of a manifest's `git = "<url>"`, without the `.git` suffix or a
/// trailing slash, so the two compare equal.
pub fn git_repository(source: &str) -> &str {
    let url = source.strip_prefix("git+").unwrap_or(source);
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let url = url.trim_end_matches('/');
    url.strip_suffix(".git").unwrap_or(url)
}

/// The reference part of a `git+<url>?<reference>#<commit>` source,
/// such as `rev=abc` or `branch=main`; empty for the default branch.
pub fn git_reference(source: &str) -> &str {
    let before_commit = source.split('#').next().unwrap_or(source);
    before_commit.split_once('?').map_or("", |(_, r)| r)
}

/// An entry of a package's `dependencies` list: `"name"`,
/// `"name version"` or `"name version (source)"`. Cargo writes the
/// version only when the lock holds several versions of `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockDepRef {
    pub name: String,
    pub version: Option<Version>,
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Lockfile {
    pub path: PathBuf,
    pub packages: Vec<LockPackage>,
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let err = |msg: String| Error::Config(format!("{}: {msg}", path.display()));
        let raw: RawLock = toml::from_str(text).map_err(|e| err(e.to_string()))?;
        match raw.version {
            Some(3 | 4) => {}
            other => {
                return Err(err(format!(
                    "unsupported Cargo.lock version {other:?}; Quon reads versions 3 and 4 \
                     (regenerate it with a recent cargo)"
                )));
            }
        }
        let mut packages = Vec::with_capacity(raw.package.len());
        for p in raw.package {
            let version = Version::parse(&p.version)
                .map_err(|e| err(format!("package `{}`: bad version: {e}", p.name)))?;
            let dependencies = p
                .dependencies
                .iter()
                .map(|d| parse_dep_ref(d).map_err(|m| err(format!("package `{}`: {m}", p.name))))
                .collect::<Result<Vec<_>>>()?;
            packages.push(LockPackage {
                name: p.name,
                version,
                source: p.source,
                checksum: p.checksum,
                dependencies,
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            packages,
        })
    }

    /// The lock entry for a path package (no source) named `name`.
    pub fn path_package(&self, name: &str, version: &Version) -> Option<&LockPackage> {
        self.packages
            .iter()
            .find(|p| p.source.is_none() && p.name == name && &p.version == version)
    }

    /// The lock entry for the git package `name@version` from `source`.
    pub fn git_package(&self, name: &str, version: &Version, source: &str) -> Option<&LockPackage> {
        self.packages.iter().find(|p| {
            p.name == name && &p.version == version && p.source.as_deref() == Some(source)
        })
    }

    /// Find the git package that `parent`'s dependency on `name` from
    /// the repository `url` locks to. `Err` carries a reason for the
    /// diagnostic.
    pub fn locked_git_dep(
        &self,
        parent: &LockPackage,
        name: &str,
        url: &str,
    ) -> std::result::Result<&LockPackage, String> {
        parent
            .dependencies
            .iter()
            .filter(|d| d.name == name)
            .flat_map(|d| {
                self.packages.iter().filter(move |p| {
                    p.is_git()
                        && p.name == name
                        && d.version.as_ref().is_none_or(|v| v == &p.version)
                        && d.source
                            .as_ref()
                            .is_none_or(|s| Some(s) == p.source.as_ref())
                })
            })
            .find(|p| git_repository(p.source.as_deref().unwrap_or("")) == git_repository(url))
            .ok_or_else(|| {
                format!(
                    "Cargo.lock has no git entry for `{name}` from {url} under `{} {}`",
                    parent.name, parent.version
                )
            })
    }

    /// The lock entry for registry package `name@version`.
    pub fn registry_package(&self, name: &str, version: &Version) -> Option<&LockPackage> {
        self.packages
            .iter()
            .find(|p| p.is_registry() && p.name == name && &p.version == version)
    }

    /// Find the registry package that `parent`'s dependency on `name`
    /// with requirement `req` locks to.
    ///
    /// Several entries in `parent.dependencies` can name the same
    /// package (a renamed second version); the one whose version
    /// satisfies `req` wins. `Err` carries a reason for the diagnostic.
    pub fn locked_registry_dep(
        &self,
        parent: &LockPackage,
        name: &str,
        req: &VersionReq,
    ) -> std::result::Result<&LockPackage, String> {
        let mut candidates = Vec::new();
        for d in parent.dependencies.iter().filter(|d| d.name == name) {
            for p in self.packages.iter().filter(|p| {
                p.is_registry()
                    && p.name == name
                    && d.version.as_ref().is_none_or(|v| v == &p.version)
                    && d.source
                        .as_ref()
                        .is_none_or(|s| Some(s) == p.source.as_ref())
            }) {
                candidates.push(p);
            }
        }
        if candidates.is_empty() {
            return Err(format!(
                "Cargo.lock has no registry entry for `{name}` under `{} {}`",
                parent.name, parent.version
            ));
        }
        candidates
            .iter()
            .find(|p| req.matches(&p.version))
            .copied()
            .ok_or_else(|| {
                let found: Vec<String> = candidates.iter().map(|p| p.version.to_string()).collect();
                format!(
                    "Cargo.lock locks `{name}` to {} but the manifest requires `{req}`",
                    found.join(", ")
                )
            })
    }
}

fn parse_dep_ref(s: &str) -> std::result::Result<LockDepRef, String> {
    let mut parts = s.splitn(3, ' ');
    let name = parts.next().unwrap_or_default().to_string();
    let version = parts
        .next()
        .map(|v| Version::parse(v).map_err(|e| format!("bad dependency entry `{s}`: {e}")))
        .transpose()?;
    let source = parts.next().map(|src| {
        src.trim_start_matches('(')
            .trim_end_matches(')')
            .to_string()
    });
    Ok(LockDepRef {
        name,
        version,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "fake 1.0.5",
 "fake 2.1.0",
 "util",
]

[[package]]
name = "fake"
version = "1.0.5"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "fake"
version = "2.1.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "util"
version = "0.3.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

    fn lock() -> Lockfile {
        Lockfile::parse(LOCK, Path::new("Cargo.lock")).unwrap()
    }

    #[test]
    fn picks_version_matching_requirement() {
        let l = lock();
        let app = l
            .path_package("app", &Version::parse("0.1.0").unwrap())
            .unwrap();
        let one = l
            .locked_registry_dep(app, "fake", &VersionReq::parse("1").unwrap())
            .unwrap();
        assert_eq!(one.version.to_string(), "1.0.5");
        let two = l
            .locked_registry_dep(app, "fake", &VersionReq::parse("2.1").unwrap())
            .unwrap();
        assert_eq!(two.version.to_string(), "2.1.0");
        let util = l
            .locked_registry_dep(app, "util", &VersionReq::parse("0.3").unwrap())
            .unwrap();
        assert_eq!(util.version.to_string(), "0.3.0");
    }

    #[test]
    fn stale_lock_reports_mismatch() {
        let l = lock();
        let app = l
            .path_package("app", &Version::parse("0.1.0").unwrap())
            .unwrap();
        let err = l
            .locked_registry_dep(app, "util", &VersionReq::parse("0.4").unwrap())
            .unwrap_err();
        assert!(err.contains("requires `^0.4`"), "{err}");
        let err = l
            .locked_registry_dep(app, "missing", &VersionReq::STAR)
            .unwrap_err();
        assert!(err.contains("no registry entry"), "{err}");
    }

    #[test]
    fn rejects_old_format() {
        let err = Lockfile::parse("version = 1\n", Path::new("Cargo.lock")).unwrap_err();
        assert!(err.to_string().contains("unsupported Cargo.lock version"));
        // v1/v2 lockfiles have no top-level version key.
        assert!(Lockfile::parse("", Path::new("Cargo.lock")).is_err());
    }
}
