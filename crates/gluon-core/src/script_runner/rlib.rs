//! On-demand build (and cache) of `libgluon_config.rlib`.
//!
//! ## Why this layer exists
//!
//! A user's `gluon.rs` says `use gluon_config;`. To compile that file
//! with rustc directly (we do not want to drag a `Cargo.toml` into every
//! kernel project), we must hand rustc:
//!
//! - the absolute path to a `libgluon_config-<hash>.rlib` and, where
//!   cargo writes one, its sibling `.rmeta` (both go to `--extern`), and
//! - one `-L dependency=<dir>` for every directory holding a transitive
//!   library gluon-config links against (`libgluon_model`, `libserde`,
//!   etc.).
//!
//! The simplest reliable way to obtain both is to delegate the build to
//! cargo (`cargo build --release -p gluon-config --message-format=json`)
//! and read the `compiler-artifact` messages. Cargo emits one per unit,
//! including units that were already fresh, so the stream always lists
//! every library in the graph. See [`parse_cargo_output`] for how the
//! paths are picked and why both cargo build-dir layouts work.
//!
//! ## Caching
//!
//! Cargo already incrementally caches the compile, but running it costs
//! a cargo startup and a lock on the target dir. We persist the resolved
//! paths under `<build_dir>/script-runner/rlib/<hash>/manifest.json` so
//! subsequent invocations skip cargo entirely and read the manifest
//! directly. A manifest is only trusted while every path it records
//! still exists.
//!
//! Cache key is `sha256(rustc_version || sources_of(gluon-config) ||
//! sources_of(gluon-model))`. Touching either crate's sources or the
//! compiler invalidates the entry. cargo is run with `RUSTC` set to the
//! project's rustc, so the compiler in the key is the one that built
//! the rlib.
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
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Paths consumed by `cache::ensure_script_binary`'s rustc invocation.
#[derive(Debug, Clone)]
pub struct RlibPaths {
    /// Absolute path to `libgluon_config-<hash>.rlib`.
    pub gluon_config_rlib: PathBuf,
    /// Absolute path to the `.rmeta` beside the rlib, if cargo reported
    /// one. Nightly cargo (1.101+) writes rlibs with only a metadata
    /// stub, so rustc needs this file too; cargo itself passes both as
    /// `--extern` when linking a binary.
    pub gluon_config_rmeta: Option<PathBuf>,
    /// Directories holding the transitive libraries, sorted and
    /// deduplicated. Each is passed to rustc as `-L dependency=<dir>`.
    pub dep_dirs: Vec<PathBuf>,
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
        && paths
            .gluon_config_rmeta
            .as_ref()
            .is_none_or(|m| m.is_file())
        && paths.dep_dirs.iter().all(|d| d.is_dir())
    {
        return Ok(paths);
    }

    // Cache miss: build via cargo and persist the resolved paths.
    let paths = run_cargo_build(&workspace, &rustc_info.rustc_path)?;
    if !paths.gluon_config_rlib.is_file() {
        return Err(Error::Config(format!(
            "cargo build -p gluon-config reported {} but the file does not exist",
            paths.gluon_config_rlib.display()
        )));
    }

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
///
/// Exposed as `pub` so `configure` (Stage 1 shim) can compute the
/// gluon-config `lib.rs` path without triggering a cargo build.
pub fn workspace_dir() -> Result<PathBuf> {
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

/// On-disk shape of `manifest.json`. A manifest written by an older
/// gluon (single `deps_dir` field) fails to deserialize, which
/// [`read_cached_manifest`] treats as a cache miss.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedManifest {
    gluon_config_rlib: PathBuf,
    gluon_config_rmeta: Option<PathBuf>,
    dep_dirs: Vec<PathBuf>,
}

impl From<RlibPaths> for CachedManifest {
    fn from(p: RlibPaths) -> Self {
        Self {
            gluon_config_rlib: p.gluon_config_rlib,
            gluon_config_rmeta: p.gluon_config_rmeta,
            dep_dirs: p.dep_dirs,
        }
    }
}

impl From<CachedManifest> for RlibPaths {
    fn from(m: CachedManifest) -> Self {
        Self {
            gluon_config_rlib: m.gluon_config_rlib,
            gluon_config_rmeta: m.gluon_config_rmeta,
            dep_dirs: m.dep_dirs,
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

/// `cargo build` for gluon-config, compiled by `rustc`.
///
/// cargo runs from the gluon workspace, where rustup would pick the
/// workspace's own toolchain. The script binary is compiled by the
/// project's rustc, and rustc refuses (E0514) to link a crate built by a
/// different compiler. Setting `RUSTC` makes cargo use the project's
/// compiler, which is also the one the cache key names.
fn cargo_build_command(workspace_dir: &Path, rustc: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("cargo");
    cmd.args([
        "build",
        "--release",
        "-p",
        "gluon-config",
        "--message-format=json",
    ])
    .env("RUSTC", rustc)
    .current_dir(workspace_dir);
    cmd
}

fn run_cargo_build(workspace_dir: &Path, rustc: &Path) -> Result<RlibPaths> {
    let output = cargo_build_command(workspace_dir, rustc)
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
/// to pass as `--extern` and (b) every directory that holds a library
/// unit, for `-L dependency=`.
///
/// ## Cargo build-dir layouts
///
/// Stable cargo puts every unit in `<target>/release/deps/` and
/// uplifts the explicitly built package to `<target>/release/`:
///
/// - `<target>/release/libgluon_config.rlib`        (uplifted, no hash)
/// - `<target>/release/deps/libgluon_config-<h>.rmeta`
///
/// Nightly cargo (1.101+) gives each unit its own directory and no
/// longer embeds full metadata in rlibs:
///
/// - `<target>/release/build/gluon-config/<h>/out/libgluon_config-<h>.rlib`
/// - `<target>/release/build/gluon-config/<h>/out/libgluon_config-<h>.rmeta`
///
/// There the rlib holds only a metadata stub, and rustc reads the real
/// metadata from the sibling `.rmeta`. An uplifted rlib has no sibling,
/// so rustc rejects it with "only metadata stub found".
///
/// ## Selection rules
///
/// - The gluon-config rlib is the one next to its `.rmeta`: the
///   `.rmeta` path from `filenames` with the extension swapped. Cargo
///   writes that rlib in both layouts (stable hardlinks the uplifted
///   copy from it). Both files are returned, and the caller passes
///   both to `--extern` as cargo does. Only when cargo reports no
///   `.rmeta` do we fall back to the first `.rlib` in `filenames`.
/// - Dependency directories are the parents of every `.rmeta` and
///   proc-macro dylib of every library unit. `.rmeta` files never sit
///   in the uplift directory, so the stub-only uplifted rlib is never
///   put on a search path. On stable this yields the single `deps/`
///   directory; on nightly one directory per unit.
fn parse_cargo_output(stdout: &[u8]) -> Result<RlibPaths> {
    let mut gluon_config: Option<(PathBuf, Option<PathBuf>)> = None;
    let mut dep_dirs: BTreeSet<PathBuf> = BTreeSet::new();
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
        let is_proc_macro = kinds.iter().any(|k| k.as_str() == Some("proc-macro"));
        if !is_lib && !is_proc_macro {
            continue;
        }
        let filenames: Vec<PathBuf> = v["filenames"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let has_ext = |p: &Path, exts: &[&str]| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
        };

        for f in &filenames {
            let search_ext: &[&str] = if is_proc_macro {
                &["dylib", "so", "dll"]
            } else {
                &["rmeta"]
            };
            if has_ext(f, search_ext)
                && let Some(parent) = f.parent()
            {
                dep_dirs.insert(parent.to_path_buf());
            }
        }

        // Cargo emits both `gluon-config` (the package name) and
        // `gluon_config` (the crate name) interchangeably across versions.
        if is_lib && (name == "gluon-config" || name == "gluon_config") {
            let rmeta = filenames.iter().find(|f| has_ext(f, &["rmeta"]));
            if let Some(m) = rmeta {
                gluon_config = Some((m.with_extension("rlib"), Some(m.clone())));
            } else if let Some(r) = filenames.iter().find(|f| has_ext(f, &["rlib"])) {
                gluon_config = Some((r.clone(), None));
            }
        }
    }

    let (gluon_config_rlib, gluon_config_rmeta) = gluon_config.ok_or_else(|| {
        Error::Config(
            "cargo build did not produce a gluon-config rlib artifact \
             (no compiler-artifact message found)"
                .into(),
        )
    })?;

    Ok(RlibPaths {
        gluon_config_rlib,
        gluon_config_rmeta,
        dep_dirs: dep_dirs.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cargo_output_stable_deps_layout() {
        // Stable cargo: every unit in `deps/`, the root package uplifted
        // to `release/`. The uplifted rlib is ignored in favour of the
        // hashed one next to the `.rmeta`, and `release/` is not a
        // search path.
        let stream = r#"
{"reason":"compiler-artifact","target":{"name":"serde","kind":["lib"]},"filenames":["/t/release/deps/libserde-s.rlib","/t/release/deps/libserde-s.rmeta"]}
{"reason":"compiler-artifact","target":{"name":"serde_derive","kind":["proc-macro"]},"filenames":["/t/release/deps/libserde_derive-d.dylib"]}
{"reason":"compiler-artifact","target":{"name":"gluon-model","kind":["lib"]},"filenames":["/t/release/deps/libgluon_model-m.rlib","/t/release/deps/libgluon_model-m.rmeta"]}
{"reason":"compiler-artifact","target":{"name":"gluon-config","kind":["lib"]},"filenames":["/t/release/libgluon_config.rlib","/t/release/deps/libgluon_config-c.rmeta"]}
{"reason":"build-finished","success":true}
"#;
        let paths = parse_cargo_output(stream.as_bytes()).expect("parse ok");
        assert_eq!(
            paths.gluon_config_rlib,
            PathBuf::from("/t/release/deps/libgluon_config-c.rlib")
        );
        assert_eq!(
            paths.gluon_config_rmeta,
            Some(PathBuf::from("/t/release/deps/libgluon_config-c.rmeta"))
        );
        assert_eq!(paths.dep_dirs, vec![PathBuf::from("/t/release/deps")]);
    }

    #[test]
    fn parse_cargo_output_per_unit_layout() {
        // Nightly cargo 1.101+: each unit in its own directory, rlib
        // beside its `.rmeta`. Every unit directory is a search path.
        let stream = r#"
{"reason":"compiler-artifact","target":{"name":"build-script-build","kind":["custom-build"]},"filenames":["/t/release/build/serde/b/out/build_script_build"]}
{"reason":"compiler-artifact","target":{"name":"serde","kind":["lib"]},"filenames":["/t/release/build/serde/s/out/libserde-s.rlib","/t/release/build/serde/s/out/libserde-s.rmeta"],"fresh":true}
{"reason":"compiler-artifact","target":{"name":"serde_derive","kind":["proc-macro"]},"filenames":["/t/release/build/serde_derive/d/out/libserde_derive-d.dylib"],"fresh":true}
{"reason":"compiler-artifact","target":{"name":"gluon_model","kind":["lib"]},"filenames":["/t/release/build/gluon-model/m/out/libgluon_model-m.rlib","/t/release/build/gluon-model/m/out/libgluon_model-m.rmeta"],"fresh":true}
{"reason":"compiler-artifact","target":{"name":"gluon_config","kind":["lib"]},"filenames":["/t/release/build/gluon-config/c/out/libgluon_config-c.rlib","/t/release/build/gluon-config/c/out/libgluon_config-c.rmeta"],"fresh":true}
{"reason":"build-finished","success":true}
"#;
        let paths = parse_cargo_output(stream.as_bytes()).expect("parse ok");
        assert_eq!(
            paths.gluon_config_rlib,
            PathBuf::from("/t/release/build/gluon-config/c/out/libgluon_config-c.rlib")
        );
        assert_eq!(
            paths.gluon_config_rmeta,
            Some(PathBuf::from(
                "/t/release/build/gluon-config/c/out/libgluon_config-c.rmeta"
            ))
        );
        assert_eq!(
            paths.dep_dirs,
            vec![
                PathBuf::from("/t/release/build/gluon-config/c/out"),
                PathBuf::from("/t/release/build/gluon-model/m/out"),
                PathBuf::from("/t/release/build/serde/s/out"),
                PathBuf::from("/t/release/build/serde_derive/d/out"),
            ]
        );
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
    fn cargo_build_uses_the_project_rustc() {
        // The rlib cache is keyed on the project's rustc, and the script
        // is compiled by it, so cargo must build gluon-config with it too.
        let rustc = Path::new("/toolchains/project/bin/rustc");
        let cmd = cargo_build_command(Path::new("/gluon"), rustc);
        let rustc_env = cmd
            .get_envs()
            .find(|(k, _)| *k == "RUSTC")
            .and_then(|(_, v)| v);
        assert_eq!(rustc_env, Some(rustc.as_os_str()));
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
