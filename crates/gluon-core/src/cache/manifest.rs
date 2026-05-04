//! On-disk cache manifest types and atomic persistence.
//!
//! See `docs/src/architecture/cache.md` for the manifest's role, the
//! corruption-handling policy (always cold-cache, never error), and
//! the atomic write-temp + fsync + rename protocol.

use crate::error::{Diagnostic, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

/// Per-source-file fingerprint. The fast path is mtime + size; the
/// content hash is only consulted when the fast path disagrees, but we
/// populate it eagerly on [`crate::cache::Cache::mark_built`] so the
/// fallback actually has something to compare against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    /// Last-modification time in nanoseconds since `UNIX_EPOCH`.
    pub mtime_ns: u128,
    /// File size in bytes. Included alongside `mtime_ns` because some
    /// filesystems round mtime to the second — two edits within the same
    /// second that change size will still invalidate the cache.
    pub size: u64,
    /// Content hash used when the mtime+size check fails. Populated on the
    /// first build; `Option` rather than a mandatory field so older
    /// manifests that predate eager hashing (and any future lazy-fill
    /// upgrade path) can still deserialise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<[u8; 32]>,
}

/// One cached build. Keyed in the manifest by a caller-chosen string
/// (typically the crate id in chunk A4's sysroot builder).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustcEntry {
    /// SHA-256 of the rustc invocation that produced `output_path` — see
    /// [`crate::cache::hash::hash_argv`]. Mismatch ⇒ stale.
    pub argv_hash: [u8; 32],
    /// Source files consumed by the build, with per-file fingerprints.
    /// `BTreeMap` for deterministic iteration / serialisation order.
    pub source_files: BTreeMap<PathBuf, FileFingerprint>,
    /// Mtimes (nanoseconds since `UNIX_EPOCH`) of the direct parent
    /// directories of all source files. A change in directory mtime
    /// indicates that files were added or removed — catching new-file
    /// additions that per-file tracking alone would miss.
    ///
    /// Backwards-compatible: older manifests deserialise with an empty
    /// map via `#[serde(default)]`; newer manifests read by older gluon
    /// versions silently ignore the populated field (the old code had
    /// `let _ = &entry.parent_dir_mtimes`). Both directions degrade to
    /// a more conservative freshness check, never to incorrect caching.
    #[serde(default)]
    pub parent_dir_mtimes: BTreeMap<PathBuf, u128>,
    /// Path to the artefact this entry declares as built. `is_fresh`
    /// requires this file to exist for the entry to be considered fresh.
    pub output_path: PathBuf,
    /// Wall-clock time of the build in nanoseconds since `UNIX_EPOCH`.
    /// Informational only — not used in freshness checks.
    pub built_at: u128,
}

/// Top-level manifest persisted as JSON. Keys are strings so the file
/// is stable across [`BTreeMap`] internal representation changes.
///
/// The manifest holds two parallel maps:
/// - `rustc`: per-crate compile cache entries (argv hash + source set).
/// - `composite`: non-rustc artifacts whose freshness is determined by
///   input fingerprints + an opaque `aux_hash` (sysroot, image, ESP).
///
/// Splitting them keeps each entry shape minimal — composite entries
/// have no meaningful `argv_hash`, rustc entries have no `aux_hash` —
/// and lets [`crate::cache::Cache`] expose two type-safe APIs without
/// runtime tag checking on every read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheManifest {
    pub version: u32,
    /// Rustc-shaped entries (per-crate compile cache).
    ///
    /// `#[serde(default)]` is load-bearing for the v1→v2 migration UX:
    /// a v1 manifest has only `entries: {...}` and no `rustc` key. Without
    /// the attribute, serde would reject the document with a parse error
    /// and the user would see "cache manifest is corrupted" instead of the
    /// cleaner "cache schema upgraded; rebuilt clean" message. With it,
    /// serde parses v1 successfully (silently dropping `entries`,
    /// defaulting `rustc`/`composite` to empty), then the version check
    /// fires and routes to the upgrade branch.
    #[serde(default)]
    pub rustc: BTreeMap<String, RustcEntry>,
    /// Composite-shaped entries (sysroot, image, ESP). Same `#[serde(default)]`
    /// rationale as `rustc`.
    #[serde(default)]
    pub composite: BTreeMap<String, crate::cache::composite::CompositeEntry>,
}

impl Default for CacheManifest {
    /// Hand-rolled rather than derived so `default()` stamps
    /// `CURRENT_VERSION` instead of `0`. A manifest with `version: 0`
    /// would be treated as a stale older-schema document by `load`,
    /// which makes `Default` a footgun for callers who construct
    /// in-memory manifests directly. Equivalent to [`Self::fresh`].
    fn default() -> Self {
        Self::fresh()
    }
}

impl CacheManifest {
    /// The only version gluon currently understands. Bump (and add a
    /// migration path) if the on-disk layout ever changes incompatibly.
    ///
    /// History:
    /// - v1: single `entries: BTreeMap<String, CacheEntry>` map.
    /// - v2: split into `rustc` + `composite` maps; `CacheEntry`
    ///   renamed to `RustcEntry`. v1 manifests are *discarded* on
    ///   load (cold-cache fallback) — the rustc-side schema didn't
    ///   change in shape, but writing a converter wasn't worth the
    ///   complexity for a build cache that re-populates on next run.
    pub const CURRENT_VERSION: u32 = 2;

    /// Load a manifest from disk.
    ///
    /// Returns `(manifest, warnings)` — the manifest is always usable
    /// (cold-cache fallback on any corruption), and warnings are
    /// [`Diagnostic`]s the caller can surface or suppress. On any of:
    /// file missing, file unreadable, JSON parse failure, or version
    /// mismatch, the returned manifest is a fresh default.
    pub fn load(path: &Path) -> (Self, Vec<Diagnostic>) {
        let mut warnings = Vec::new();

        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Self::fresh(), warnings);
            }
            Err(_) => {
                warnings.push(
                    Diagnostic::warning(format!(
                        "cache manifest at {} is unreadable; treating as cold cache",
                        path.display()
                    ))
                    .with_note("Next build will rebuild from scratch. Delete the file to silence this warning."),
                );
                return (Self::fresh(), warnings);
            }
        };
        match serde_json::from_slice::<CacheManifest>(&bytes) {
            Ok(m) if m.version == Self::CURRENT_VERSION => (m, warnings),
            Ok(m) => {
                // Schema version mismatch — either an older gluon wrote
                // a v < CURRENT manifest (we never carry stale entries
                // forward across schema bumps) or a newer gluon wrote a
                // v > CURRENT manifest (we can't make sense of fields
                // that didn't exist when this binary was built). Both
                // collapse to "cold cache, rebuild from scratch"; the
                // wording deliberately says "upgraded" because the
                // overwhelmingly common path is a forward upgrade after
                // a gluon version bump.
                warnings.push(Diagnostic::warning(format!(
                    "cache schema upgraded; rebuilt clean (manifest at {} was version {}, expected {})",
                    path.display(),
                    m.version,
                    Self::CURRENT_VERSION
                )));
                (Self::fresh(), warnings)
            }
            Err(_) => {
                warnings.push(
                    Diagnostic::warning(format!(
                        "cache manifest at {} is corrupted; treating as cold cache",
                        path.display()
                    ))
                    .with_note("Next build will rebuild from scratch. Delete the file to silence this warning."),
                );
                (Self::fresh(), warnings)
            }
        }
    }

    /// Write the manifest to `path` atomically.
    ///
    /// Writes to `<path>.tmp.<pid>.<nonce>` with pretty-printed JSON,
    /// fsyncs the file, then renames into place. Creates the parent
    /// directory if it doesn't already exist. Always stamps
    /// `version = CURRENT_VERSION` so a stale in-memory value can't be
    /// persisted. On any failure path the temp file is removed before
    /// the error is returned, so failed builds don't accumulate orphans
    /// in the cache directory.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        // Stamp the current version regardless of whatever the caller
        // left in `self.version`. Construct a shallow view rather than
        // mutating `self`: the public API is `&self` and we don't want
        // hidden side-effects.
        let to_write = CacheManifest {
            version: Self::CURRENT_VERSION,
            rustc: self.rustc.clone(),
            composite: self.composite.clone(),
        };

        // Build `<path>.tmp.<pid>.<nonce>`. We avoid `with_extension`
        // because it mangles extensionless paths (turning `cache` into
        // `cache.tmp.123.456` is fine, but `with_extension` on a path
        // whose final component is `foo` would replace, not append).
        // `with_file_name` + `format!` is unambiguous.
        let file_name = path.file_name().ok_or_else(|| {
            Error::Config(format!("cache manifest path has no filename: {path:?}"))
        })?;
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

        // Wrap the write/fsync/rename body in an immediately-invoked
        // closure so we have a single point at which we can clean up the
        // temp file before propagating any error. Without this, a
        // failure inside `to_writer_pretty`, `into_inner`, `sync_all`,
        // or `rename` would leave `<path>.tmp.<pid>.<nonce>` on disk and
        // the build directory would accumulate orphans across failed
        // runs.
        let tmp_for_body = tmp_path.clone();
        let result: Result<()> = (|| {
            // Scoped so the BufWriter (and underlying File) is dropped
            // before the rename, which would otherwise leave a dangling
            // handle on Windows.
            {
                let file = File::create(&tmp_for_body).map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: e,
                })?;
                let mut writer = BufWriter::new(file);
                serde_json::to_writer_pretty(&mut writer, &to_write).map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: std::io::Error::other(e),
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
            // Best-effort cleanup. If the temp file was never created
            // (e.g. File::create failed) this is a no-op; if the rename
            // succeeded the temp file no longer exists and this is also
            // a no-op. We deliberately swallow the remove error: the
            // caller already has a more useful error to propagate.
            let _ = fs::remove_file(&tmp_path);
        }

        result
    }

    fn fresh() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            rustc: BTreeMap::new(),
            composite: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> RustcEntry {
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("src/a.rs"),
            FileFingerprint {
                mtime_ns: 1_000,
                size: 42,
                sha256: Some([0x11; 32]),
            },
        );
        RustcEntry {
            argv_hash: [0xAB; 32],
            source_files: files,
            parent_dir_mtimes: BTreeMap::new(),
            output_path: PathBuf::from("build/foo.rlib"),
            built_at: 999_999,
        }
    }

    #[test]
    fn round_trip_single_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache-manifest.json");

        let mut m = CacheManifest::fresh();
        m.rustc.insert("foo".into(), sample_entry());
        m.save_atomic(&p).expect("save");

        let loaded = CacheManifest::load(&p).0;
        assert_eq!(loaded.version, CacheManifest::CURRENT_VERSION);
        assert_eq!(loaded.rustc.get("foo"), Some(&sample_entry()));
    }

    #[test]
    fn missing_file_returns_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("nope.json");
        let loaded = CacheManifest::load(&p).0;
        assert_eq!(loaded.version, CacheManifest::CURRENT_VERSION);
        assert!(loaded.rustc.is_empty());
    }

    #[test]
    fn corrupted_json_returns_default_with_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("bad.json");
        std::fs::write(&p, b"not json at all").expect("seed");
        let (loaded, warnings) = CacheManifest::load(&p);
        assert!(loaded.rustc.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("corrupted"));
    }

    #[test]
    fn wrong_version_returns_default_with_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("v99.json");
        std::fs::write(&p, br#"{"version": 99, "rustc": {}, "composite": {}}"#).expect("seed");
        let (loaded, warnings) = CacheManifest::load(&p);
        assert!(loaded.rustc.is_empty());
        assert_eq!(loaded.version, CacheManifest::CURRENT_VERSION);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].message.contains("cache schema upgraded"),
            "warning text mismatch: {}",
            warnings[0].message
        );
    }

    #[test]
    fn save_always_stamps_current_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache.json");

        // Construct with a bogus version; save should overwrite it.
        let m = CacheManifest {
            version: 42,
            rustc: BTreeMap::new(),
            composite: BTreeMap::new(),
        };
        m.save_atomic(&p).expect("save");

        let bytes = std::fs::read(&p).expect("read");
        let parsed: CacheManifest = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed.version, CacheManifest::CURRENT_VERSION);
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("nested/dir/cache.json");
        let m = CacheManifest::fresh();
        m.save_atomic(&p).expect("save");
        assert!(p.exists());
    }

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache-manifest.json");
        let m = CacheManifest::fresh();
        m.save_atomic(&p).expect("save");

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("cache-manifest.json.tmp")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "unexpected leftover temp files: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_atomic_cleans_up_tmp_file_on_failure() {
        // Force `fs::rename` (or an earlier write step) to fail by making
        // the parent directory read+execute-only, then assert no
        // `.tmp.<pid>.<nonce>` orphan is left behind. We restore the
        // permissions at the end so the tempdir can clean itself up.
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ro");
        std::fs::create_dir(&dir).expect("mkdir");

        // Create a placeholder so the parent directory exists from
        // save_atomic's point of view (it'll skip create_dir_all because
        // the parent already exists).
        let target = dir.join("cache-manifest.json");

        // Read+execute only — no writes allowed in this directory.
        let original_perms = std::fs::metadata(&dir).expect("meta").permissions();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod ro");

        let m = CacheManifest::fresh();
        let result = m.save_atomic(&target);

        // Restore permissions before any assertion that might panic, so
        // the tempdir cleanup at end-of-scope still succeeds.
        std::fs::set_permissions(&dir, original_perms).expect("restore perms");

        assert!(
            result.is_err(),
            "expected save_atomic to fail in a read-only directory"
        );

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("cache-manifest.json.tmp")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "save_atomic leaked temp files on failure: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn entries_iterate_in_sorted_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache.json");

        let mut m = CacheManifest::fresh();
        for k in ["z", "a", "m"] {
            m.rustc.insert(k.into(), sample_entry());
        }
        m.save_atomic(&p).expect("save");

        let loaded = CacheManifest::load(&p).0;
        let keys: Vec<_> = loaded.rustc.keys().cloned().collect();
        assert_eq!(keys, vec!["a".to_string(), "m".into(), "z".into()]);
    }

    #[test]
    fn manifest_has_separate_rustc_and_composite_maps() {
        let m = CacheManifest::default();
        assert_eq!(m.version, CacheManifest::CURRENT_VERSION);
        assert!(m.rustc.is_empty());
        assert!(m.composite.is_empty());
    }

    #[test]
    fn manifest_round_trip_with_both_maps_populated() {
        use crate::cache::composite::CompositeEntry;

        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache.json");

        let mut m = CacheManifest::default();
        m.rustc.insert("crate-foo".into(), sample_entry());
        m.composite.insert(
            "image:boot".into(),
            CompositeEntry {
                inputs: BTreeMap::new(),
                aux_hash: Some([0xAA; 32]),
                output_path: PathBuf::from("/build/boot.img"),
                built_at: 0,
            },
        );
        m.save_atomic(&p).expect("save");

        let loaded = CacheManifest::load(&p).0;
        assert_eq!(loaded.rustc.len(), 1);
        assert_eq!(loaded.composite.len(), 1);
        assert!(loaded.rustc.contains_key("crate-foo"));
        assert!(loaded.composite.contains_key("image:boot"));
    }

    #[test]
    fn v1_manifest_is_discarded_with_upgrade_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache.json");
        // A literal v1 payload as written by the previous CURRENT_VERSION.
        std::fs::write(
            &p,
            br#"{"version": 1, "entries": {"foo": {
                "argv_hash": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
                "source_files": {},
                "parent_dir_mtimes": {},
                "output_path": "/x.rlib",
                "built_at": 0
            }}}"#,
        )
        .expect("seed v1");
        let (loaded, warnings) = CacheManifest::load(&p);
        assert_eq!(loaded.version, CacheManifest::CURRENT_VERSION);
        assert!(loaded.rustc.is_empty(), "v1 entries must be discarded");
        assert!(loaded.composite.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].message.contains("cache schema upgraded"),
            "warning text mismatch: {}",
            warnings[0].message
        );
    }

    #[test]
    fn future_version_is_discarded_with_upgrade_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("future.json");
        std::fs::write(&p, br#"{"version": 99, "rustc": {}, "composite": {}}"#).expect("seed");
        let (loaded, warnings) = CacheManifest::load(&p);
        assert!(loaded.rustc.is_empty());
        assert!(loaded.composite.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].message.contains("cache schema upgraded"),
            "warning text mismatch: {}",
            warnings[0].message
        );
    }
}
