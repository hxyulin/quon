//! `gluon.lock` — on-disk pin of the vendored dependency set.
//!
//! `gluon.lock` is a thin peer of cargo's `Cargo.lock`. The authoritative
//! version resolution lives in cargo's lockfile, inside the scratch
//! workspace under `<build>/vendor-workspaces/<target>/Cargo.lock` (one
//! per target — `host/` for the host pipeline). `gluon.lock`
//! instead records two things cargo cannot:
//!
//! 1. A **fingerprint** over the declared `BuildModel::external_deps`
//!    set, so `gluon build` can tell in O(model-size) time whether the
//!    user has edited `gluon.rs` since the last `gluon vendor` run.
//!    See [`super::fingerprint`] for the hash function.
//! 2. The **list of packages** that were actually vendored, with
//!    per-package checksums. This lets `gluon vendor --check` detect
//!    hand-edits of `vendor/` without re-running the full vendor flow.
//!
//! # Corruption policy
//!
//! Missing lock files are `Ok(None)` — that's the "fresh project, never
//! vendored" case, not an error. Corrupt or wrong-version lock files
//! produce an [`Error::Diagnostics`]: unlike the cache manifest, a
//! stale-but-parseable lock would silently desync the model from the
//! actual `vendor/` contents, which is the exact bug this file exists
//! to prevent. Better to surface the corruption and force a
//! `gluon vendor` run than to fall back to "empty lock".
//!
//! # Atomic writes
//!
//! [`VendorLock::save_atomic`] uses the same `<path>.tmp.<pid>.<nonce>`
//!     + fsync + rename pattern as
//!     [`crate::cache::manifest::CacheManifest::save_atomic`], for the same
//!     reasons.
//!
//! # Why vendor freshness is not in the unified cache
//!
//! Stage 5 collapsed the build-time freshness systems (rustc cache,
//! sysroot stamp, image stamp, ESP stamp) onto a single
//! `cache::Cache`. Vendor's `gluon.lock`-fingerprint check was
//! deliberately *not* migrated. The reason is the time horizon:
//! `gluon.lock` is committed to source control and travels with the
//! repository; the build cache is per-machine and `.gitignore`d.
//!
//! Migrating vendor into the cache would mean: after `git clone`,
//! machine B has a valid `gluon.lock` + `vendor/` tree but a cold
//! cache, so the cache would report "stale" and trigger a re-vendor
//! on every fresh checkout. The lockfile-fingerprint check correctly
//! says "nothing to do" in that case.
//!
//! The vendor freshness question — *"is the on-disk vendor tree
//! consistent with what `gluon.rs` declares?"* — is fundamentally
//! cross-machine, not build-time per-machine. It belongs here,
//! not in `cache/`.

use crate::error::{Diagnostic, Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// What rustc crate kind cargo emitted for a vendored package.
///
/// Today only two kinds are interesting to gluon's compile path:
///
/// - [`Lib`](Self::Lib) — `.rlib` artifacts that cross-compile against
///   the kernel sysroot.
/// - [`ProcMacro`](Self::ProcMacro) — host-only dylibs, even when the
///   *consumer* is a kernel crate.
///
/// Other kinds (`bin`, `cdylib`, `staticlib`, ...) are filtered out
/// upstream in `vendor::cross::metadata` and never make it into a
/// `VendorLockPackage`. If we ever need to track them, add a variant
/// here rather than smuggling them through `Lib`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VendorPackageKind {
    #[default]
    Lib,
    ProcMacro,
}

/// One vendored package as pinned in `gluon.lock`.
///
/// Mirrors the shape upstream hadron used so hand-inspection and
/// gluon.lock diffs are obvious at a glance.
///
/// # Schema-v1 compatibility
///
/// `target` and `kind` are v2 additions. Both carry `#[serde(default)]`
/// so v1 packages (which had neither field) deserialise as
/// `target = "host"` / `kind = Lib`. That matches the v1 invariant
/// — the v1 pipeline only ever vendored host-targeted libs — so the
/// migration is lossless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorLockPackage {
    /// Crate name as it appears in `Cargo.toml` / `gluon.rs`.
    pub name: String,
    /// Resolved version. For git deps this is the version declared in
    /// the vendored `Cargo.toml`; for path deps it is the path crate's
    /// declared version.
    pub version: String,
    /// Build target the package was vendored *for*. Either `"host"` or
    /// the name of a `target(...)` declared in `gluon.rs`. The same
    /// crate at the same version may appear twice with different
    /// `target` values — once host, once cross — and they are pinned,
    /// stamped, and looked up independently.
    ///
    /// Defaults to `"host"` on v1 lockfiles (see the type-level
    /// schema-v1 note).
    #[serde(default = "default_target")]
    pub target: String,
    /// Crate kind cargo emitted. See [`VendorPackageKind`].
    ///
    /// Defaults to [`VendorPackageKind::Lib`] on v1 lockfiles.
    #[serde(default)]
    pub kind: VendorPackageKind,
    /// Where the package came from:
    ///
    /// - `"crates-io"` for [`DepSource::CratesIo`]
    /// - `"git+<url>#<rev-or-branch-or-tag>"` for [`DepSource::Git`]
    /// - `"path+<relpath>"` for [`DepSource::Path`]
    ///
    /// Kept as a single string field for easy round-tripping and
    /// because the source variant is fully encoded in the prefix.
    ///
    /// [`DepSource::CratesIo`]: gluon_model::DepSource::CratesIo
    /// [`DepSource::Git`]: gluon_model::DepSource::Git
    /// [`DepSource::Path`]: gluon_model::DepSource::Path
    pub source: String,
    /// SHA-256 of the vendored directory's contents (see
    /// [`super::checksum`]). `None` for path deps because their
    /// contents are not gluon-managed and will naturally drift with
    /// user edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Absolute path to the artifact (`.rlib` for libs, `.dylib`/`.so`/
    /// `.dll` for proc-macros) that cargo produced when building the
    /// scratch vendor workspace. Captured from `cargo build
    /// --message-format=json`. `None` for fresh lockfiles written
    /// before the cargo-build step ran (e.g. lockfiles from older
    /// gluon versions, or path-only models that skipped the build).
    ///
    /// `gluon build` consults this path via `command_builder` to wire
    /// `--extern <alias>=<path>` for consumer crates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<PathBuf>,
}

/// Default value for [`VendorLockPackage::target`] when deserialising
/// a v1 lockfile (see the schema-v1 note on the struct).
fn default_target() -> String {
    "host".to_string()
}

/// Top-level `gluon.lock` document.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorLock {
    /// Schema version. Bump on incompatible layout changes.
    pub version: u32,
    /// Hex `sha256:...` over the sorted `external_deps` snapshot —
    /// see [`super::fingerprint`]. A mismatch between this and the
    /// live model means the user edited deps without re-vendoring.
    pub fingerprint: String,
    /// Vendored packages, serialised as a TOML array of tables.
    #[serde(rename = "package", default)]
    pub packages: Vec<VendorLockPackage>,
}

impl VendorLock {
    /// Current schema version. Bump + add a migration path if
    /// `VendorLock` ever grows incompatible fields.
    ///
    /// # Version history
    ///
    /// - **v1** (initial): host-only pipeline. `VendorLockPackage`
    ///   had `name`, `version`, `source`, `checksum`, `artifact_path`.
    /// - **v2** (cross-target vendor builds): adds `target` and `kind`
    ///   per package so the same crate can be pinned twice — once for
    ///   the host pipeline, once for a cross target — and the
    ///   compile-side lookup can disambiguate between them. v1
    ///   lockfiles still load (see [`Self::load`]).
    pub const CURRENT_VERSION: u32 = 2;

    /// Lockfile schema versions this build of gluon understands. v1 is
    /// kept readable so a `git pull` that brings in a cross-target
    /// dep doesn't force every collaborator into a manual re-vendor;
    /// the next `gluon vendor` rewrites at [`Self::CURRENT_VERSION`].
    const SUPPORTED_VERSIONS: &'static [u32] = &[1, 2];

    /// Construct an empty lock with a placeholder fingerprint.
    ///
    /// Used by callers that want to build up a lock programmatically
    /// before writing it (e.g. `vendor_sync`). Not meant as a fallback
    /// for load failures — see [`Self::load`].
    pub fn empty(fingerprint: impl Into<String>) -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            fingerprint: fingerprint.into(),
            packages: Vec::new(),
        }
    }

    /// Load a lock from disk.
    ///
    /// Returns:
    /// - `Ok(None)` if the file does not exist (the "never vendored"
    ///   case — callers should decide whether that's an error based on
    ///   whether the model actually has external deps).
    /// - `Ok(Some(lock))` on a successful, current-version parse.
    /// - `Err(Diagnostics)` on any of: unreadable file, parse failure,
    ///   wrong schema version. Unlike `CacheManifest::load`, we treat
    ///   these as hard errors because silently using an empty lock
    ///   would mask dependency drift — the exact bug this file is
    ///   here to catch.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(e) => {
                return Err(Error::Diagnostics(vec![Diagnostic::error(format!(
                    "failed to read gluon.lock at {}: {e}",
                    path.display()
                ))]));
            }
        };
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            Error::Diagnostics(vec![Diagnostic::error(format!(
                "gluon.lock at {} is not valid UTF-8: {e}",
                path.display()
            ))])
        })?;
        let parsed: VendorLock = toml::from_str(text).map_err(|e| {
            Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "failed to parse gluon.lock at {}: {e}",
                    path.display()
                ))
                .with_note("delete gluon.lock and re-run `gluon vendor` to regenerate"),
            ])
        })?;
        if !Self::SUPPORTED_VERSIONS.contains(&parsed.version) {
            return Err(Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "gluon.lock at {} has schema version {} (expected one of {:?})",
                    path.display(),
                    parsed.version,
                    Self::SUPPORTED_VERSIONS,
                ))
                .with_note("delete gluon.lock and re-run `gluon vendor` to regenerate"),
            ]));
        }
        Ok(Some(parsed))
    }

    /// Write the lock to `path` atomically.
    ///
    /// Serialises to TOML, writes to `<path>.tmp.<pid>.<nonce>`,
    /// fsyncs, then renames into place. Creates the parent directory
    /// on demand. Always stamps the current schema version regardless
    /// of whatever the caller left in `self.version` — same rationale
    /// as [`crate::cache::manifest::CacheManifest::save_atomic`].
    ///
    /// A header comment is prepended to discourage hand-editing.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        // Shallow view with the current version stamped in. Can't mutate
        // `&self`, and we don't want callers to see this field flip
        // behind their back.
        let to_write = VendorLock {
            version: Self::CURRENT_VERSION,
            fingerprint: self.fingerprint.clone(),
            packages: self.packages.clone(),
        };

        let body = toml::to_string_pretty(&to_write).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(e),
        })?;

        let file_name = path
            .file_name()
            .ok_or_else(|| Error::Config(format!("gluon.lock path has no filename: {path:?}")))?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp_path = path.with_file_name(format!(
            "{}.tmp.{}.{}",
            file_name.to_string_lossy(),
            std::process::id(),
            nonce,
        ));

        // Wrap the write/fsync/rename body in an IIFE so we have a
        // single cleanup point — see the equivalent in
        // `CacheManifest::save_atomic`.
        let tmp_for_body = tmp_path.clone();
        let result: Result<()> = (|| {
            {
                let file = File::create(&tmp_for_body).map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: e,
                })?;
                let mut writer = BufWriter::new(file);
                writer
                    .write_all(HEADER_COMMENT.as_bytes())
                    .map_err(|e| Error::Io {
                        path: tmp_for_body.clone(),
                        source: e,
                    })?;
                writer.write_all(body.as_bytes()).map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: e,
                })?;
                let file = writer.into_inner().map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: std::io::Error::other(e.to_string()),
                })?;
                file.sync_all().map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: e,
                })?;
            }
            fs::rename(&tmp_for_body, path).map_err(|e| Error::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }

        result
    }
}

/// Prefix written at the top of every `gluon.lock` file. Purely
/// advisory — we ignore it on read, but users editing the file see it
/// immediately.
const HEADER_COMMENT: &str = "\
# This file is auto-generated by `gluon vendor`. Do not edit by hand.
# It pins the vendored dependency set so `gluon build` can detect
# staleness without re-running cargo vendor.

";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lock() -> VendorLock {
        // Mixed-target sample: bitflags appears twice (host + cross),
        // syn is host-only proc-macro, local-helper is a host-only path
        // dep. Exercises every (target, kind) shape the lockfile is
        // expected to carry.
        VendorLock {
            version: VendorLock::CURRENT_VERSION,
            fingerprint: "sha256:deadbeef".into(),
            packages: vec![
                VendorLockPackage {
                    name: "bitflags".into(),
                    version: "2.11.0".into(),
                    target: "host".into(),
                    kind: VendorPackageKind::Lib,
                    source: "crates-io".into(),
                    checksum: Some("sha256:aaaa".into()),
                    artifact_path: Some(PathBuf::from(
                        "/build/target/release/deps/libbitflags-abcd.rlib",
                    )),
                },
                VendorLockPackage {
                    name: "bitflags".into(),
                    version: "2.11.0".into(),
                    target: "kernel".into(),
                    kind: VendorPackageKind::Lib,
                    source: "crates-io".into(),
                    checksum: Some("sha256:aaaa".into()),
                    artifact_path: Some(PathBuf::from(
                        "/build/cross/kernel/deps/libbitflags-wxyz.rlib",
                    )),
                },
                VendorLockPackage {
                    name: "syn".into(),
                    version: "2.0.117".into(),
                    target: "host".into(),
                    kind: VendorPackageKind::ProcMacro,
                    source: "crates-io".into(),
                    checksum: Some("sha256:cccc".into()),
                    artifact_path: Some(PathBuf::from(
                        "/build/target/release/deps/libsyn-mnop.dylib",
                    )),
                },
                VendorLockPackage {
                    name: "tokio".into(),
                    version: "1.40.0".into(),
                    target: "host".into(),
                    kind: VendorPackageKind::Lib,
                    source: "git+https://github.com/tokio-rs/tokio#abc1234".into(),
                    checksum: Some("sha256:bbbb".into()),
                    artifact_path: Some(PathBuf::from(
                        "/build/target/release/deps/libtokio-efgh.rlib",
                    )),
                },
                VendorLockPackage {
                    name: "local-helper".into(),
                    version: "0.1.0".into(),
                    target: "host".into(),
                    kind: VendorPackageKind::Lib,
                    source: "path+../helper".into(),
                    checksum: None,
                    artifact_path: Some(PathBuf::from(
                        "/build/target/release/deps/liblocal_helper-ijkl.rlib",
                    )),
                },
            ],
        }
    }

    #[test]
    fn round_trip_multi_source() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("gluon.lock");
        let lock = sample_lock();
        lock.save_atomic(&p).expect("save");
        let loaded = VendorLock::load(&p).expect("load").expect("present");
        assert_eq!(loaded, lock);
    }

    // Old lockfiles (gluon < cargo-as-builder) lack the `artifact_path`
    // field. Loading them must succeed with `artifact_path = None` so
    // upgrades don't force a re-vendor; the auto_register pass can
    // detect the missing path and either run the cargo-build step or
    // surface a "rerun gluon vendor" diagnostic, depending on context.
    //
    // Also exercises the v1 → v2 schema migration: pre-cross-target
    // lockfiles have no `target` or `kind` fields per package. The
    // `#[serde(default)]` attributes on those fields must produce
    // `target = "host"`, `kind = Lib` — matching the v1 invariant
    // that everything was a host-targeted lib.
    #[test]
    fn loads_v1_lockfile_with_target_kind_defaults() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("gluon.lock");
        std::fs::write(
            &p,
            br#"version = 1
fingerprint = "sha256:abc"

[[package]]
name = "bitflags"
version = "2.11.0"
source = "crates-io"
checksum = "sha256:aaaa"
"#,
        )
        .expect("seed");
        let loaded = VendorLock::load(&p).expect("load").expect("present");
        assert_eq!(loaded.version, 1, "load must preserve the on-disk version");
        assert_eq!(loaded.packages.len(), 1);
        assert!(
            loaded.packages[0].artifact_path.is_none(),
            "absent field must deserialise to None"
        );
        assert_eq!(
            loaded.packages[0].target, "host",
            "v1 packages migrate to target=host"
        );
        assert_eq!(
            loaded.packages[0].kind,
            VendorPackageKind::Lib,
            "v1 packages migrate to kind=lib"
        );
    }

    // The next save_atomic after loading a v1 must rewrite the file as
    // v2 — that's how the migration completes. We assert this against
    // the same v1 fixture used above.
    #[test]
    fn save_after_loading_v1_rewrites_as_v2() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("gluon.lock");
        std::fs::write(
            &p,
            br#"version = 1
fingerprint = "sha256:abc"

[[package]]
name = "bitflags"
version = "2.11.0"
source = "crates-io"
checksum = "sha256:aaaa"
"#,
        )
        .expect("seed");
        let loaded = VendorLock::load(&p).expect("load").expect("present");
        loaded.save_atomic(&p).expect("save");
        let body = std::fs::read_to_string(&p).expect("read");
        assert!(
            body.contains("version = 2"),
            "v1 lockfile must round-trip as v2; got: {body}"
        );
        let reloaded = VendorLock::load(&p).expect("reload").expect("present");
        assert_eq!(reloaded.version, VendorLock::CURRENT_VERSION);
        assert_eq!(reloaded.packages[0].target, "host");
        assert_eq!(reloaded.packages[0].kind, VendorPackageKind::Lib);
    }

    #[test]
    fn load_missing_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("nope.lock");
        assert!(VendorLock::load(&p).expect("load").is_none());
    }

    #[test]
    fn load_corrupt_is_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("bad.lock");
        std::fs::write(&p, b"not toml at all \x00\x01").expect("seed");
        let err = VendorLock::load(&p).expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains("parse") || msg.contains("UTF-8"),
            "expected parse/UTF-8 error, got: {msg}"
        );
    }

    #[test]
    fn load_wrong_version_is_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("v99.lock");
        std::fs::write(
            &p,
            br#"version = 99
fingerprint = "sha256:xx"
"#,
        )
        .expect("seed");
        let err = VendorLock::load(&p).expect_err("must error");
        assert!(err.to_string().contains("schema version 99"));
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("nested/dir/gluon.lock");
        sample_lock().save_atomic(&p).expect("save");
        assert!(p.exists());
    }

    #[test]
    fn save_always_stamps_current_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("gluon.lock");
        let lock = VendorLock {
            version: 42, // bogus
            fingerprint: "sha256:cafe".into(),
            packages: Vec::new(),
        };
        lock.save_atomic(&p).expect("save");
        let loaded = VendorLock::load(&p).expect("load").expect("present");
        assert_eq!(loaded.version, VendorLock::CURRENT_VERSION);
    }

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("gluon.lock");
        sample_lock().save_atomic(&p).expect("save");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("gluon.lock.tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "unexpected leftover temp files: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn header_comment_is_written() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("gluon.lock");
        sample_lock().save_atomic(&p).expect("save");
        let body = std::fs::read_to_string(&p).expect("read");
        assert!(body.starts_with("# This file is auto-generated"));
    }

    // Same crate vendored twice — once host, once kernel — must round
    // trip cleanly. The two entries share name+version+source but
    // differ in `target`, which is the whole point of v2.
    #[test]
    fn round_trip_disambiguates_same_crate_across_targets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("gluon.lock");
        let lock = sample_lock();
        lock.save_atomic(&p).expect("save");
        let loaded = VendorLock::load(&p).expect("load").expect("present");

        let bitflags: Vec<_> = loaded
            .packages
            .iter()
            .filter(|pkg| pkg.name == "bitflags")
            .collect();
        assert_eq!(bitflags.len(), 2, "both bitflags entries must round-trip");
        let targets: Vec<&str> = bitflags.iter().map(|p| p.target.as_str()).collect();
        assert!(targets.contains(&"host"));
        assert!(targets.contains(&"kernel"));

        let syn = loaded
            .packages
            .iter()
            .find(|pkg| pkg.name == "syn")
            .expect("syn must round-trip");
        assert_eq!(syn.kind, VendorPackageKind::ProcMacro);
        assert_eq!(syn.target, "host", "proc-macros are always host-built");
    }

    // The on-disk `kind` field is the kebab-case form `"proc-macro"`,
    // not the Rust identifier. Hand-edited lockfiles and external
    // tooling all see kebab-case, so pin it explicitly.
    #[test]
    fn proc_macro_kind_serialises_as_kebab_case() {
        let lock = VendorLock {
            version: VendorLock::CURRENT_VERSION,
            fingerprint: "sha256:x".into(),
            packages: vec![VendorLockPackage {
                name: "syn".into(),
                version: "2.0.117".into(),
                target: "host".into(),
                kind: VendorPackageKind::ProcMacro,
                source: "crates-io".into(),
                checksum: Some("sha256:y".into()),
                artifact_path: None,
            }],
        };
        let toml = toml::to_string_pretty(&lock).expect("serialise");
        assert!(
            toml.contains(r#"kind = "proc-macro""#),
            "kind must serialise as kebab-case: {toml}"
        );
    }

    #[test]
    fn path_deps_serialise_without_checksum_key() {
        let lock = VendorLock {
            version: VendorLock::CURRENT_VERSION,
            fingerprint: "sha256:x".into(),
            packages: vec![VendorLockPackage {
                name: "local".into(),
                version: "0.1.0".into(),
                target: "host".into(),
                kind: VendorPackageKind::Lib,
                source: "path+../local".into(),
                checksum: None,
                artifact_path: None,
            }],
        };
        let toml = toml::to_string_pretty(&lock).expect("serialise");
        assert!(
            !toml.contains("checksum"),
            "path dep emitted a checksum: {toml}"
        );
        assert!(
            !toml.contains("artifact_path"),
            "absent artifact_path leaked into output: {toml}"
        );
    }
}
