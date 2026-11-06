//! Where registry packages' sources live.
//!
//! The workspace loader does not know how registry sources were
//! obtained. It takes a [`RegistrySources`] map from `(name, version)` to
//! the source directory, and reads `<dir>/Cargo.toml`. Two backends fill
//! the map:
//!
//! - **Cargo's registry cache** (the default),
//!   [`RegistrySources::registry_cache`]. Quon does not compute paths
//!   under `$CARGO_HOME/registry/src`; it runs `cargo fetch --locked`
//!   and reads each package's `manifest_path` from `cargo metadata
//!   --locked --offline`. The map is cached in the build directory,
//!   keyed on the SHA-256 of `Cargo.lock` and the `CARGO_HOME` value,
//!   so a build whose lock and sources are unchanged runs no Cargo
//!   command. A cached directory that no longer exists (Cargo deletes
//!   unpacked sources it has not used for a while) reruns both steps.
//! - **A vendor directory** named in `quon.rs`,
//!   [`RegistrySources::vendor_dir`]: `<dir>/<name>-<version>/`, filled
//!   by `quon vendor` ([`vendor`]). The build runs no Cargo command and
//!   fails when `Cargo.lock` changed since the last `quon vendor`
//!   ([`check_vendor_stamp`]).
//!
//! Git packages come from the same `cargo metadata` output: Cargo's
//! checkout of the locked commit, under `$CARGO_HOME/git/checkouts`,
//! keyed by name, version and the `git+<url>?<reference>#<commit>`
//! source `Cargo.lock` records. A vendor directory has none; see
//! [`RegistrySources::is_vendored`].

use super::lock::Lockfile;
use crate::cache::hash::sha256_bytes;
use crate::error::{Diagnostic, Error, Result};
use crate::vendor::cargo_cmd::resolve_cargo;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// File in a vendor directory holding the SHA-256 of the `Cargo.lock`
/// that `quon vendor` vendored.
pub const VENDOR_STAMP: &str = ".quon-vendor-stamp";

/// Hint for sources missing from Cargo's registry cache.
const REGISTRY_HINT: &str = "run `cargo fetch`, or vendor the sources with `quon vendor`";
/// Hint for sources missing from a vendor directory.
const VENDOR_HINT: &str = "run `quon vendor`";

#[derive(Debug, Clone, Default)]
pub struct RegistrySources {
    dirs: BTreeMap<(String, Version), PathBuf>,
    /// Git checkouts by name, version and lock source. Canonical, so a
    /// path dependency's canonical directory finds its source.
    git: BTreeMap<(String, Version, String), PathBuf>,
    /// Appended to "source missing" diagnostics: what the user runs to
    /// fill the source location.
    hint: String,
    pub(crate) vendored: bool,
}

impl RegistrySources {
    pub fn new(dirs: BTreeMap<(String, Version), PathBuf>, hint: impl Into<String>) -> Self {
        Self {
            dirs,
            git: BTreeMap::new(),
            hint: hint.into(),
            vendored: false,
        }
    }

    /// Add git checkouts, by name, version and lock source.
    #[must_use]
    pub fn with_git(mut self, git: BTreeMap<(String, Version, String), PathBuf>) -> Self {
        self.git = git
            .into_iter()
            .map(|(k, dir)| (k, std::fs::canonicalize(&dir).unwrap_or(dir)))
            .collect();
        self
    }

    /// Whether the sources come from a vendor directory, which holds no
    /// git packages.
    pub fn is_vendored(&self) -> bool {
        self.vendored
    }

    /// The checkout of git package `name@version` from `source`, or the
    /// reason there is none.
    pub fn git_dir(
        &self,
        name: &str,
        version: &Version,
        source: &str,
    ) -> std::result::Result<&Path, String> {
        let dir = self
            .git
            .get(&(name.to_string(), version.clone(), source.to_string()))
            .ok_or_else(|| {
                format!("no checkout of `{name} {version}` from {source}; run `cargo fetch`")
            })?;
        if !dir.join("Cargo.toml").is_file() {
            return Err(format!(
                "checkout of `{name} {version}` is missing at {}; run `cargo fetch`",
                dir.display()
            ));
        }
        Ok(dir)
    }

    /// The lock source of the git package whose canonical directory is
    /// `dir`: a path dependency inside a git checkout belongs to that
    /// checkout's source, as in Cargo.
    pub fn git_source_of(&self, dir: &Path) -> Option<&str> {
        self.git
            .iter()
            .find(|(_, d)| d.as_path() == dir)
            .map(|((_, _, source), _)| source.as_str())
    }

    /// What the user runs when `Cargo.lock` is missing or does not
    /// match the manifests. Cargo writes the lock; in vendor mode the
    /// vendor directory must then be refilled.
    pub fn lock_hint(&self) -> &'static str {
        if self.vendored {
            "run `cargo update`, then `quon vendor`"
        } else {
            "run `cargo update`"
        }
    }

    /// Sources vendored by `quon vendor` into `vendor_dir`:
    /// `<vendor_dir>/<name>-<version>/` for every registry package in
    /// `<workspace_root>/Cargo.lock` (`quon vendor` passes
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
        Ok(Self {
            vendored: true,
            ..Self::new(dirs, VENDOR_HINT)
        })
    }

    /// Sources in Cargo's registry cache, as Cargo reports them. See the
    /// module docs. `cache_file` holds the map between builds; `None`
    /// asks Cargo every time.
    ///
    /// Runs no Cargo command when `Cargo.lock` is missing (the loader
    /// reports it) or holds no registry package.
    pub fn registry_cache(workspace_root: &Path, cache_file: Option<&Path>) -> Result<Self> {
        let lock_path = workspace_root.join("Cargo.lock");
        let Ok(bytes) = std::fs::read(&lock_path) else {
            return Ok(Self::new(BTreeMap::new(), REGISTRY_HINT));
        };
        let lock = Lockfile::parse(&String::from_utf8_lossy(&bytes), &lock_path)?;
        if !lock.packages.iter().any(|p| p.is_registry() || p.is_git()) {
            return Ok(Self::new(BTreeMap::new(), REGISTRY_HINT));
        }
        let key = SourceCacheKey {
            format: SOURCE_CACHE_FORMAT,
            lock_sha256: hex(&sha256_bytes(&bytes)),
            cargo_home: cargo_home()
                .map(|h| h.display().to_string())
                .unwrap_or_default(),
        };
        if let Some(found) = cache_file.and_then(|f| read_source_cache(f, &key)) {
            return Ok(Self::new(found.registry, REGISTRY_HINT).with_git(found.git));
        }

        run_cargo(workspace_root, &["fetch", "--locked"])?;
        let out = run_cargo(
            workspace_root,
            &["metadata", "--format-version", "1", "--locked", "--offline"],
        )?;
        let found = parse_metadata_sources(&out.stdout)?;
        if let Some(f) = cache_file {
            // A failed write only means the next build asks Cargo again.
            let _ = write_source_cache(f, &key, &found);
        }
        Ok(Self::new(found.registry, REGISTRY_HINT).with_git(found.git))
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

/// `$CARGO_HOME`, defaulting to `~/.cargo` as in Cargo.
pub fn cargo_home() -> Option<PathBuf> {
    match std::env::var_os("CARGO_HOME") {
        Some(h) => Some(PathBuf::from(h)),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".cargo")),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of `<workspace_root>/Cargo.lock` in hex, or `None` when there
/// is no lock.
fn lock_sha256(workspace_root: &Path) -> Result<Option<String>> {
    let path = workspace_root.join("Cargo.lock");
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(hex(&sha256_bytes(&bytes)))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io { path, source: e }),
    }
}

/// Fail unless `vendor_dir` was filled by `quon vendor` from the
/// current `Cargo.lock`. A workspace without a lock passes; the loader
/// reports the missing lock.
pub fn check_vendor_stamp(workspace_root: &Path, vendor_dir: &Path) -> Result<()> {
    let Some(want) = lock_sha256(workspace_root)? else {
        return Ok(());
    };
    let stamp = vendor_dir.join(VENDOR_STAMP);
    let found = std::fs::read_to_string(&stamp).ok();
    match found.as_deref().map(str::trim) {
        Some(have) if have == want => Ok(()),
        Some(_) => Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "Cargo.lock changed since the last `quon vendor` into {}",
                vendor_dir.display()
            ))
            .with_note("run `quon vendor`"),
        ])),
        None => Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "{} was not filled by `quon vendor` (no {VENDOR_STAMP})",
                vendor_dir.display()
            ))
            .with_note("run `quon vendor`"),
        ])),
    }
}

/// Record that `vendor_dir` holds the sources for the current
/// `Cargo.lock`.
pub fn write_vendor_stamp(workspace_root: &Path, vendor_dir: &Path) -> Result<()> {
    let hash = lock_sha256(workspace_root)?.ok_or_else(|| {
        Error::Config(format!(
            "no Cargo.lock in {}; run `cargo generate-lockfile`",
            workspace_root.display()
        ))
    })?;
    let stamp = vendor_dir.join(VENDOR_STAMP);
    std::fs::write(&stamp, format!("{hash}\n")).map_err(|e| Error::Io {
        path: stamp,
        source: e,
    })
}

/// `quon vendor` for a Cargo workspace: run `cargo vendor
/// --versioned-dirs --locked` into `vendor_dir` and write the vendor
/// stamp. Returns the number of registry packages vendored.
///
/// Cargo's progress goes to stderr. The `.cargo/config.toml` snippet
/// `cargo vendor` prints is dropped: Quon reads the directory itself,
/// configured with `q.cargo_workspace(..).vendored(..)`.
pub fn vendor(workspace_root: &Path, vendor_dir: &Path, offline: bool) -> Result<usize> {
    // The stamp goes first, so a failed run never leaves a directory
    // that claims to match the lock.
    match std::fs::remove_file(vendor_dir.join(VENDOR_STAMP)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::Io {
                path: vendor_dir.join(VENDOR_STAMP),
                source: e,
            });
        }
    }
    let target = vendor_dir.to_string_lossy().into_owned();
    let mut args = vec!["vendor", "--versioned-dirs", "--locked"];
    if offline {
        args.push("--offline");
    }
    args.push(&target);
    let mut cmd = cargo_command(workspace_root, &args);
    cmd.stderr(Stdio::inherit());
    let out = spawn(cmd, &args)?;
    check_status(&out, &args, workspace_root)?;
    write_vendor_stamp(workspace_root, vendor_dir)?;
    let lock = Lockfile::load(&workspace_root.join("Cargo.lock"))?;
    Ok(lock.packages.iter().filter(|p| p.is_registry()).count())
}

/// `cargo <args> --manifest-path <ws>/Cargo.toml`, run in the workspace
/// root so Cargo reads the workspace's `.cargo/config.toml`.
fn cargo_command(workspace_root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(resolve_cargo());
    cmd.args(args)
        .arg("--manifest-path")
        .arg(workspace_root.join("Cargo.toml"))
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn spawn(mut cmd: Command, args: &[&str]) -> Result<Output> {
    cmd.output().map_err(|e| {
        Error::Diagnostics(vec![
            Diagnostic::error(format!("failed to spawn `cargo {}`: {e}", args.join(" ")))
                .with_note(format!(
                    "tried to invoke {} (set $CARGO to override)",
                    resolve_cargo().display()
                )),
        ])
    })
}

fn check_status(out: &Output, args: &[&str], workspace_root: &Path) -> Result<()> {
    if out.status.success() {
        return Ok(());
    }
    let mut d = Diagnostic::error(format!(
        "`cargo {}` failed in {}",
        args.join(" "),
        workspace_root.display()
    ));
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        d = d.with_note(format!("cargo's stderr:\n{}", stderr.trim_end()));
    }
    Err(Error::Diagnostics(vec![d.with_note(
        "to build without Cargo or the network, vendor the sources with `quon vendor` and \
         add `.vendored(\"vendor\")` to `q.cargo_workspace(...)`",
    )]))
}

/// Run a Cargo command that must succeed, capturing its output.
fn run_cargo(workspace_root: &Path, args: &[&str]) -> Result<Output> {
    let out = spawn(cargo_command(workspace_root, args), args)?;
    check_status(&out, args, workspace_root)?;
    Ok(out)
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    name: String,
    version: String,
    source: Option<String>,
    manifest_path: PathBuf,
}

/// Source directories from `cargo metadata`: registry packages by name
/// and version, git checkouts by name, version and source.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SourceDirs {
    pub registry: BTreeMap<(String, Version), PathBuf>,
    pub git: BTreeMap<(String, Version, String), PathBuf>,
}

/// The registry and git packages' source directories from `cargo
/// metadata --format-version 1` output. Path packages are skipped.
pub(crate) fn parse_metadata_sources(json: &[u8]) -> Result<SourceDirs> {
    let err = |m: String| Error::Config(format!("cannot read `cargo metadata` output: {m}"));
    let meta: Metadata = serde_json::from_slice(json).map_err(|e| err(e.to_string()))?;
    let mut found = SourceDirs::default();
    for p in meta.packages {
        let Some(source) = p.source else { continue };
        let registry = source.starts_with("registry+") || source.starts_with("sparse+");
        let git = source.starts_with("git+");
        if !registry && !git {
            continue;
        }
        let version = Version::parse(&p.version)
            .map_err(|e| err(format!("package `{}`: bad version: {e}", p.name)))?;
        let dir = p
            .manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        if git {
            found.git.insert((p.name, version, source), dir);
        } else {
            found.registry.insert((p.name, version), dir);
        }
    }
    Ok(found)
}

/// Bumped when the cache file's meaning changes, so an older file is
/// ignored instead of misread: format 2 added git checkouts, which a
/// format 1 file for the same lock left out.
const SOURCE_CACHE_FORMAT: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SourceCacheKey {
    #[serde(default)]
    format: u32,
    lock_sha256: String,
    cargo_home: String,
}

#[derive(Serialize, Deserialize)]
struct SourceCache {
    #[serde(flatten)]
    key: SourceCacheKey,
    packages: Vec<CachedSource>,
}

#[derive(Serialize, Deserialize)]
struct CachedSource {
    name: String,
    version: String,
    dir: PathBuf,
    /// The lock source of a git checkout; `None` for a registry package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

/// The cached map, if its key is `key` and every directory still holds
/// a manifest.
fn read_source_cache(file: &Path, key: &SourceCacheKey) -> Option<SourceDirs> {
    let cache: SourceCache = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    if &cache.key != key {
        return None;
    }
    let mut found = SourceDirs::default();
    for p in cache.packages {
        if !p.dir.join("Cargo.toml").is_file() {
            return None;
        }
        let version = Version::parse(&p.version).ok()?;
        match p.source {
            Some(source) => found.git.insert((p.name, version, source), p.dir),
            None => found.registry.insert((p.name, version), p.dir),
        };
    }
    Some(found)
}

fn write_source_cache(
    file: &Path,
    key: &SourceCacheKey,
    found: &SourceDirs,
) -> std::io::Result<()> {
    let registry = found
        .registry
        .iter()
        .map(|((name, version), dir)| CachedSource {
            name: name.clone(),
            version: version.to_string(),
            dir: dir.clone(),
            source: None,
        });
    let git = found
        .git
        .iter()
        .map(|((name, version, source), dir)| CachedSource {
            name: name.clone(),
            version: version.to_string(),
            dir: dir.clone(),
            source: Some(source.clone()),
        });
    let cache = SourceCache {
        key: key.clone(),
        packages: registry.chain(git).collect(),
    };
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(&cache).map_err(std::io::Error::other)?;
    std::fs::write(file, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = "version = 4\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\n\
                        dependencies = [\"reg\"]\n[[package]]\nname = \"reg\"\n\
                        version = \"1.0.0\"\n\
                        source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
                        checksum = \"00\"\n";

    #[test]
    fn metadata_keeps_registry_packages_by_version_and_git_checkouts_by_source() {
        let json = br#"{"packages": [
            {"name": "a", "version": "0.1.0", "source": null,
             "manifest_path": "/ws/a/Cargo.toml"},
            {"name": "reg", "version": "1.0.0",
             "source": "registry+https://github.com/rust-lang/crates.io-index",
             "manifest_path": "/home/.cargo/registry/src/idx/reg-1.0.0/Cargo.toml"},
            {"name": "sp", "version": "2.0.0-rc.1", "source": "sparse+https://example.com/",
             "manifest_path": "/c/sp-2.0.0-rc.1/Cargo.toml"},
            {"name": "g", "version": "0.1.0", "source": "git+https://example.com/g#abc",
             "manifest_path": "/c/g/Cargo.toml"}
        ]}"#;
        let found = parse_metadata_sources(json).unwrap();
        let got: Vec<String> = found
            .registry
            .iter()
            .map(|((n, v), d)| format!("{n} {v} {}", d.display()))
            .collect();
        assert_eq!(
            got,
            [
                "reg 1.0.0 /home/.cargo/registry/src/idx/reg-1.0.0",
                "sp 2.0.0-rc.1 /c/sp-2.0.0-rc.1",
            ]
        );
        let git: Vec<String> = found
            .git
            .iter()
            .map(|((n, v, s), d)| format!("{n} {v} {s} {}", d.display()))
            .collect();
        assert_eq!(git, ["g 0.1.0 git+https://example.com/g#abc /c/g"]);
    }

    #[test]
    fn source_cache_round_trips_and_checks_key_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("reg-1.0.0");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("Cargo.toml"), "").unwrap();
        let file = tmp.path().join("build/cargo-sources.json");
        let key = SourceCacheKey {
            format: SOURCE_CACHE_FORMAT,
            lock_sha256: "aa".into(),
            cargo_home: "/h".into(),
        };
        let dirs = SourceDirs {
            registry: BTreeMap::from([(("reg".to_string(), Version::new(1, 0, 0)), pkg.clone())]),
            git: BTreeMap::from([(
                (
                    "g".to_string(),
                    Version::new(0, 1, 0),
                    "git+https://example.com/g#abc".to_string(),
                ),
                pkg.clone(),
            )]),
        };
        write_source_cache(&file, &key, &dirs).unwrap();
        assert_eq!(read_source_cache(&file, &key), Some(dirs));

        // A file from before git checkouts were cached lacks them, so it
        // must not be used.
        let older = SourceCacheKey {
            format: 0,
            ..key.clone()
        };
        assert_eq!(read_source_cache(&file, &older), None);

        let other_home = SourceCacheKey {
            cargo_home: "/other".into(),
            ..key.clone()
        };
        assert_eq!(read_source_cache(&file, &other_home), None);

        // Cargo removed the unpacked sources: the cache is not used.
        std::fs::remove_file(pkg.join("Cargo.toml")).unwrap();
        assert_eq!(read_source_cache(&file, &key), None);
    }

    /// Without registry packages in the lock, no Cargo command runs:
    /// `$CARGO` is not consulted, so a broken one cannot fail the load.
    #[test]
    fn registry_cache_without_registry_packages_runs_no_cargo() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.lock"),
            "version = 4\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let s = RegistrySources::registry_cache(tmp.path(), None).unwrap();
        assert!(s.dirs.is_empty());
        // No lock at all: nothing to fetch either.
        let empty = tempfile::tempdir().unwrap();
        assert!(
            RegistrySources::registry_cache(empty.path(), None)
                .unwrap()
                .dirs
                .is_empty()
        );
    }

    #[test]
    fn registry_cache_uses_a_matching_cache_without_cargo() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), LOCK).unwrap();
        let pkg = tmp.path().join("src/reg-1.0.0");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("Cargo.toml"), "").unwrap();
        let key = SourceCacheKey {
            format: SOURCE_CACHE_FORMAT,
            lock_sha256: lock_sha256(tmp.path()).unwrap().unwrap(),
            cargo_home: cargo_home().unwrap().display().to_string(),
        };
        let file = tmp.path().join("cargo-sources.json");
        let dirs = SourceDirs {
            registry: BTreeMap::from([(("reg".to_string(), Version::new(1, 0, 0)), pkg.clone())]),
            git: BTreeMap::new(),
        };
        write_source_cache(&file, &key, &dirs).unwrap();
        // The workspace has no Cargo.toml, so any Cargo command would
        // fail: a hit proves none ran.
        let s = RegistrySources::registry_cache(tmp.path(), Some(&file)).unwrap();
        assert_eq!(s.dir("reg", &Version::new(1, 0, 0)).unwrap(), pkg);
    }

    #[test]
    fn vendor_stamp_tracks_the_lock() {
        let ws = tempfile::tempdir().unwrap();
        let vendor = ws.path().join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        // No lock: nothing to check.
        check_vendor_stamp(ws.path(), &vendor).unwrap();

        std::fs::write(ws.path().join("Cargo.lock"), LOCK).unwrap();
        let err = check_vendor_stamp(ws.path(), &vendor)
            .unwrap_err()
            .to_string();
        assert!(err.contains("was not filled by `quon vendor`"), "{err}");

        write_vendor_stamp(ws.path(), &vendor).unwrap();
        check_vendor_stamp(ws.path(), &vendor).unwrap();

        std::fs::write(ws.path().join("Cargo.lock"), format!("{LOCK}\n")).unwrap();
        let err = check_vendor_stamp(ws.path(), &vendor)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Cargo.lock changed since the last `quon vendor`"),
            "{err}"
        );
    }
}
