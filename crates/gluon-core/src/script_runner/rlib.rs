//! On-demand build (and cache) of `libgluon_config.rlib`.
//!
//! ## Why this layer exists
//!
//! A user's `gluon.rs` says `use gluon_config;`. To compile that file
//! with rustc directly (we do not want to drag a `Cargo.toml` into every
//! kernel project), we must hand rustc:
//!
//! - the absolute path to a `libgluon_config-<hash>.rlib`, and
//! - a `-L dependency=<dir>` pointing at every transitive rlib gluon-config
//!   needs to link against (`libgluon_model.rlib`, `libserde.rlib`, etc.).
//!
//! The simplest reliable way to obtain both is to delegate the build to
//! cargo (`cargo build --release -p gluon-config --message-format=json`),
//! parse the JSON output stream for the `compiler-artifact` message
//! whose target is `gluon-config`, and pluck `filenames[0]` — the rlib —
//! out of it. The deps directory is the rlib's parent.
//!
//! ## Caching
//!
//! Cargo already incrementally caches the compile, but we still need a
//! lightweight side-cache because the JSON we parse to *find* the rlib
//! is only emitted on a cache miss; on a cargo cache hit the JSON
//! stream is empty. Rather than scan `target/release/deps` heuristically
//! for the latest matching rlib, we persist the resolved paths under
//! `<build_dir>/script-runner/rlib/<hash>/manifest.json` so subsequent
//! invocations skip cargo entirely and read the manifest directly.
//!
//! Cache key is `sha256(rustc_version || sources_of(gluon-config) ||
//! sources_of(gluon-model))`. Touching either crate's sources or the
//! compiler invalidates the entry.
//!
//! ## Workspace discovery
//!
//! The cargo invocation needs a `Cargo.toml`. `crates/gluon-core/build.rs`
//! bakes the workspace root into `GLUON_CONFIG_WORKSPACE_DIR` at compile
//! time, and runtime can override via `GLUON_CONFIG_SOURCE_DIR`. After
//! `cargo install` the baked path won't exist on the user's machine —
//! Stage 1.7 will replace this with an install-time pre-built rlib;
//! Stage 1's contract is "always run from a checkout".

use crate::compile::RustcInfo;
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Paths consumed by `cache::ensure_script_binary`'s rustc invocation.
#[derive(Debug, Clone)]
pub struct RlibPaths {
    /// Absolute path to `libgluon_config-<hash>.rlib`.
    pub gluon_config_rlib: PathBuf,
    /// Absolute path to the deps directory containing all transitive rlibs.
    /// Passed to rustc as `-L dependency=<this>`.
    pub deps_dir: PathBuf,
}

/// Build (or find-cached) the `gluon-config` rlib + deps directory.
///
/// On cache miss, shells out to `cargo build --release -p gluon-config
/// --message-format=json` from the gluon workspace root, parses the JSON
/// stream to extract the rlib path, and persists a manifest under
/// `<build_dir>/script-runner/rlib/<hash>/manifest.json`.
pub fn ensure_gluon_config_rlib(build_dir: &Path, rustc_info: &RustcInfo) -> Result<RlibPaths> {
    let workspace = workspace_dir()?;
    let cache_root = build_dir.join("script-runner").join("rlib");

    let key = compute_cache_key(&workspace, &rustc_info.version)?;
    let entry_dir = cache_root.join(&key);
    let manifest_path = entry_dir.join("manifest.json");

    // Cache hit: the manifest exists *and* the paths it records still
    // resolve to real files. The latter check guards against a wiped
    // `target/` directory invalidating a stale manifest — without it,
    // we'd return paths that the rustc invocation in `cache.rs` would
    // then fail on with a confusing `No such file or directory`.
    if let Some(paths) = read_cached_manifest(&manifest_path)
        && paths.gluon_config_rlib.is_file()
        && paths.deps_dir.is_dir()
    {
        return Ok(paths);
    }

    // Cache miss: build via cargo and persist the resolved paths.
    let paths = run_cargo_build(&workspace)?;

    std::fs::create_dir_all(&entry_dir).map_err(|e| Error::Io {
        path: entry_dir.clone(),
        source: e,
    })?;
    write_cached_manifest(&manifest_path, &paths)?;
    Ok(paths)
}

/// Resolve the gluon workspace directory.
///
/// Order:
/// 1. `GLUON_CONFIG_SOURCE_DIR` env var (runtime override — used by
///    install-time deployments and test harnesses that need to point
///    at a specific checkout).
/// 2. `GLUON_CONFIG_WORKSPACE_DIR` baked in at compile time by
///    `build.rs`.
///
/// Both must point at a directory whose `crates/gluon-config/Cargo.toml`
/// exists; otherwise we fail loudly so the user knows to set the env var.
fn workspace_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("GLUON_CONFIG_SOURCE_DIR") {
        let p = PathBuf::from(p);
        if !p.join("crates/gluon-config/Cargo.toml").is_file() {
            return Err(Error::Config(format!(
                "GLUON_CONFIG_SOURCE_DIR={} does not contain crates/gluon-config/Cargo.toml",
                p.display()
            )));
        }
        return Ok(p);
    }
    let baked = env!("GLUON_CONFIG_WORKSPACE_DIR");
    let p = PathBuf::from(baked);
    if !p.join("crates/gluon-config/Cargo.toml").is_file() {
        return Err(Error::Config(format!(
            "gluon-config workspace not found at {} (baked at compile time). \
             Set GLUON_CONFIG_SOURCE_DIR to the gluon source checkout.",
            p.display()
        )));
    }
    Ok(p)
}

/// Hash all `.rs` files and `Cargo.toml`s under `crates/gluon-config` and
/// `crates/gluon-model`, plus the rustc version. Determinism: the file
/// list is sorted by path before hashing.
fn compute_cache_key(workspace_dir: &Path, rustc_version: &str) -> Result<String> {
    use crate::cache::sha256_file;
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(rustc_version.as_bytes());

    for sub in ["crates/gluon-config", "crates/gluon-model"] {
        let dir = workspace_dir.join(sub);
        let mut entries: Vec<PathBuf> = walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                let name = e.path().file_name().and_then(|s| s.to_str());
                let ext = e.path().extension().and_then(|s| s.to_str());
                ext == Some("rs") || name == Some("Cargo.toml")
            })
            .map(|e| e.into_path())
            .collect();
        entries.sort();
        for path in entries {
            // Hash the relative path so a checkout's absolute prefix
            // doesn't change the cache key.
            let rel = path.strip_prefix(workspace_dir).unwrap_or(&path);
            hasher.update(rel.to_string_lossy().as_bytes());
            let h = sha256_file(&path).map_err(|e| match e {
                Error::Io { source, .. } => Error::Io {
                    path: path.clone(),
                    source,
                },
                other => other,
            })?;
            hasher.update(h);
        }
    }
    // Lock file captures transitive dep versions. If serde bumps from 1.0.196
    // → 1.0.197 with no source changes here, our cache MUST invalidate so the
    // new rlib is rebuilt against the new transitive deps. Without this hash,
    // the cache hit path would return paths into target/ that may no longer
    // exist.
    let lock_path = workspace_dir.join("Cargo.lock");
    if lock_path.is_file() {
        let h = sha256_file(&lock_path)?;
        hasher.update(h);
    }
    // (If Cargo.lock is absent — e.g., an unusual library-only workspace —
    // skip the hash; cargo will regenerate the lock and the next call will
    // pick it up.)

    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedManifest {
    gluon_config_rlib: PathBuf,
    deps_dir: PathBuf,
}

impl From<RlibPaths> for CachedManifest {
    fn from(p: RlibPaths) -> Self {
        Self {
            gluon_config_rlib: p.gluon_config_rlib,
            deps_dir: p.deps_dir,
        }
    }
}

impl From<CachedManifest> for RlibPaths {
    fn from(m: CachedManifest) -> Self {
        Self {
            gluon_config_rlib: m.gluon_config_rlib,
            deps_dir: m.deps_dir,
        }
    }
}

fn read_cached_manifest(path: &Path) -> Option<RlibPaths> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<CachedManifest>(&bytes)
        .ok()
        .map(Into::into)
}

fn write_cached_manifest(path: &Path, paths: &RlibPaths) -> Result<()> {
    let manifest: CachedManifest = paths.clone().into();
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| Error::Config(format!("serialize rlib manifest: {e}")))?;
    std::fs::write(path, bytes).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

fn run_cargo_build(workspace_dir: &Path) -> Result<RlibPaths> {
    use std::process::Command;
    let output = Command::new("cargo")
        .args([
            "build",
            "--release",
            "-p",
            "gluon-config",
            "--message-format=json",
        ])
        .current_dir(workspace_dir)
        .output()
        .map_err(|e| Error::Io {
            path: workspace_dir.to_path_buf(),
            source: e,
        })?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "cargo build -p gluon-config failed (status {}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    parse_cargo_output(&output.stdout)
}

/// Walk the JSON message stream and locate (a) the `gluon-config` rlib
/// path and (b) the directory that contains the *transitive* deps
/// (`libgluon_model.rlib`, `libserde.rlib`, …) needed to link against
/// it.
///
/// ## Why these two paths come from different artefacts
///
/// Cargo emits two filenames for the package being explicitly built
/// (`-p gluon-config` here):
///
/// - `<target>/release/libgluon_config.rlib`            (no hash; "public" artefact)
/// - `<target>/release/deps/libgluon_config-<svh>.rmeta`(rmeta only)
///
/// The first one is sufficient as an `--extern` target. **But its
/// parent directory is `<target>/release/`, NOT `<target>/release/deps/`**
/// — so we cannot derive `deps_dir` from it. The transitive rlibs the
/// user's `gluon.rs` needs (`libgluon_model-<svh>.rlib`, etc.) live in
/// `<target>/release/deps/`.
///
/// The deps dir is reliably derivable from any *transitive* artefact's
/// filename — those always land in `<target>/release/deps/`. We pick
/// `gluon-model` because it's a guaranteed dep of `gluon-config` and
/// always emitted as part of the build. If a future restructure removes
/// gluon-model as a direct dep, this parser must be revisited.
fn parse_cargo_output(stdout: &[u8]) -> Result<RlibPaths> {
    let mut gluon_config_rlib: Option<PathBuf> = None;
    let mut deps_dir: Option<PathBuf> = None;
    let stdout_str = std::str::from_utf8(stdout)
        .map_err(|e| Error::Config(format!("cargo stdout was not utf-8: {e}")))?;

    for line in stdout_str.lines() {
        // Each line is a self-contained JSON object. Skip lines that
        // don't parse — cargo occasionally interleaves a non-JSON line
        // (e.g. a build-script println) and we don't want a single
        // stray line to abort the whole search.
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["reason"].as_str() != Some("compiler-artifact") {
            continue;
        }
        let name = v["target"]["name"].as_str().unwrap_or("");
        // Must be a library artifact, not a build-script or test binary.
        let kinds = v["target"]["kind"].as_array().cloned().unwrap_or_default();
        let is_lib = kinds
            .iter()
            .any(|k| matches!(k.as_str(), Some("lib") | Some("rlib")));
        if !is_lib {
            continue;
        }
        let filenames = v["filenames"].as_array().cloned().unwrap_or_default();
        let first_rlib = filenames
            .iter()
            .filter_map(|f| f.as_str())
            .find(|s| s.ends_with(".rlib"))
            .map(PathBuf::from);

        // Cargo emits both `gluon-config` (the package name) and
        // `gluon_config` (the crate name) interchangeably across versions.
        if name == "gluon-config" || name == "gluon_config" {
            if let Some(p) = first_rlib.clone() {
                gluon_config_rlib = Some(p);
            }
        }
        // gluon-model is a transitive dep — its rlib reliably lives in
        // `<target>/release/deps/`, which is the deps_dir we need.
        if (name == "gluon-model" || name == "gluon_model")
            && let Some(p) = first_rlib
            && let Some(parent) = p.parent()
        {
            deps_dir = Some(parent.to_path_buf());
        }
    }

    let gluon_config_rlib = gluon_config_rlib.ok_or_else(|| {
        Error::Config(
            "cargo build did not produce a gluon-config rlib artifact \
             (no compiler-artifact message found)"
                .into(),
        )
    })?;

    // Fallback: if cargo didn't emit a gluon-model artifact (already
    // fully cached → no recompile messages), derive the deps_dir from
    // the gluon-config rlib's neighbour `deps/` directory. This is a
    // documented stable cargo layout: `<target>/release/deps/` always
    // exists alongside `<target>/release/libgluon_config.rlib`.
    let deps_dir = match deps_dir {
        Some(d) => d,
        None => {
            let parent = gluon_config_rlib
                .parent()
                .ok_or_else(|| Error::Config("rlib path has no parent directory".into()))?;
            // If the rlib is already inside `deps/`, parent IS the deps dir.
            // Otherwise (the public-artefact case at `<target>/release/`),
            // append `deps`.
            if parent.file_name().and_then(|n| n.to_str()) == Some("deps") {
                parent.to_path_buf()
            } else {
                parent.join("deps")
            }
        }
    };

    Ok(RlibPaths {
        gluon_config_rlib,
        deps_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cargo_output_picks_gluon_config_rlib() {
        let stream = r#"
{"reason":"compiler-artifact","target":{"name":"serde","kind":["lib"]},"filenames":["/tmp/release/deps/libserde.rlib"]}
{"reason":"compiler-artifact","target":{"name":"gluon-model","kind":["lib"]},"filenames":["/tmp/release/deps/libgluon_model-x.rlib","/tmp/release/deps/libgluon_model-x.rmeta"]}
{"reason":"compiler-artifact","target":{"name":"gluon-config","kind":["lib"]},"filenames":["/tmp/release/deps/libgluon_config-abc.rlib","/tmp/release/deps/libgluon_config-abc.rmeta"]}
{"reason":"build-finished","success":true}
"#;
        let paths = parse_cargo_output(stream.as_bytes()).expect("parse ok");
        assert_eq!(
            paths.gluon_config_rlib,
            PathBuf::from("/tmp/release/deps/libgluon_config-abc.rlib")
        );
        // deps_dir comes from gluon-model's parent (a transitive dep is
        // always emitted into `<target>/release/deps/`).
        assert_eq!(paths.deps_dir, PathBuf::from("/tmp/release/deps"));
    }

    #[test]
    fn parse_cargo_output_handles_public_artifact_layout() {
        // Real-world cargo output: gluon-config (the explicitly-built
        // package) is emitted at `<target>/release/libgluon_config.rlib`
        // (no hash, NOT inside deps/), while gluon-model lives in
        // `<target>/release/deps/`. The deps_dir must point at the
        // latter, not the former.
        let stream = r#"
{"reason":"compiler-artifact","target":{"name":"gluon-model","kind":["lib"]},"filenames":["/tmp/release/deps/libgluon_model-8ae.rlib"]}
{"reason":"compiler-artifact","target":{"name":"gluon-config","kind":["lib"]},"filenames":["/tmp/release/libgluon_config.rlib"]}
{"reason":"build-finished","success":true}
"#;
        let paths = parse_cargo_output(stream.as_bytes()).expect("parse ok");
        assert_eq!(
            paths.gluon_config_rlib,
            PathBuf::from("/tmp/release/libgluon_config.rlib")
        );
        assert_eq!(
            paths.deps_dir,
            PathBuf::from("/tmp/release/deps"),
            "deps_dir must come from the transitive dep, not the public artefact's parent"
        );
    }

    #[test]
    fn parse_cargo_output_falls_back_to_sibling_deps_dir() {
        // If only the gluon-config artifact is emitted (e.g. the rest
        // were fully cached), derive deps_dir as `<parent>/deps`.
        let stream = r#"
{"reason":"compiler-artifact","target":{"name":"gluon-config","kind":["lib"]},"filenames":["/tmp/release/libgluon_config.rlib"]}
"#;
        let paths = parse_cargo_output(stream.as_bytes()).expect("parse ok");
        assert_eq!(paths.deps_dir, PathBuf::from("/tmp/release/deps"));
    }

    #[test]
    fn parse_cargo_output_accepts_underscore_name() {
        // Older/newer cargo may emit `gluon_config` (crate name) instead of
        // `gluon-config` (package name). Both must work.
        let stream = r#"{"reason":"compiler-artifact","target":{"name":"gluon_config","kind":["rlib"]},"filenames":["/x/libgluon_config-1.rlib"]}"#;
        let paths = parse_cargo_output(stream.as_bytes()).expect("parse ok");
        assert_eq!(
            paths.gluon_config_rlib,
            PathBuf::from("/x/libgluon_config-1.rlib")
        );
    }

    #[test]
    fn parse_cargo_output_errors_when_no_artifact() {
        let stream = r#"{"reason":"build-finished","success":true}"#;
        assert!(parse_cargo_output(stream.as_bytes()).is_err());
    }

    #[test]
    fn parse_cargo_output_skips_non_lib_targets() {
        // A `compiler-artifact` for a build-script binary must not match.
        let stream = r#"{"reason":"compiler-artifact","target":{"name":"gluon-config","kind":["custom-build"]},"filenames":["/x/build-script-build"]}"#;
        assert!(parse_cargo_output(stream.as_bytes()).is_err());
    }

    #[test]
    fn parse_cargo_output_tolerates_non_json_lines() {
        let stream = r#"this is not json
{"reason":"compiler-artifact","target":{"name":"gluon-config","kind":["lib"]},"filenames":["/x/libgluon_config-1.rlib"]}
also not json
"#;
        let paths = parse_cargo_output(stream.as_bytes()).expect("parse ok");
        assert_eq!(
            paths.gluon_config_rlib,
            PathBuf::from("/x/libgluon_config-1.rlib")
        );
    }

    #[test]
    fn cache_key_is_deterministic_across_calls() {
        // Smoke test: hashing the same workspace twice must produce the
        // same key. Real correctness comes from the integration test;
        // this just guards against accidental nondeterminism (HashMap
        // iteration order, mtime peeking, etc.).
        let workspace = workspace_dir().expect("workspace_dir");
        let v = "rustc 1.85.0";
        let k1 = compute_cache_key(&workspace, v).expect("k1");
        let k2 = compute_cache_key(&workspace, v).expect("k2");
        assert_eq!(k1, k2);
    }

    #[test]
    fn cache_key_changes_with_rustc_version() {
        let workspace = workspace_dir().expect("workspace_dir");
        let k1 = compute_cache_key(&workspace, "rustc 1.85.0").expect("k1");
        let k2 = compute_cache_key(&workspace, "rustc 1.86.0").expect("k2");
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_changes_when_cargo_lock_changes() {
        let tmp = tempfile::tempdir().unwrap();
        // Stub a minimal workspace layout with the two crates that
        // compute_cache_key walks.
        std::fs::create_dir_all(tmp.path().join("crates/gluon-config")).unwrap();
        std::fs::create_dir_all(tmp.path().join("crates/gluon-model")).unwrap();
        std::fs::write(
            tmp.path().join("crates/gluon-config/Cargo.toml"),
            b"[package]\nname=\"gluon-config\"\nversion=\"0\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("crates/gluon-model/Cargo.toml"),
            b"[package]\nname=\"gluon-model\"\nversion=\"0\"\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), b"# version 1\n").unwrap();
        let k1 = compute_cache_key(tmp.path(), "rustc 1.0").unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), b"# version 2\n").unwrap();
        let k2 = compute_cache_key(tmp.path(), "rustc 1.0").unwrap();
        assert_ne!(k1, k2, "cache key must invalidate when Cargo.lock changes");
    }
}
