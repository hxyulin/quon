//! The gluon-config library that `gluon.rs` links against, built once
//! per gluon version and rustc and shared by every project.
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
//! ## Where the library lives
//!
//! Under [`crate::home::gluon_home`] (`$GLUON_HOME`, default
//! `~/.gluon`), in one of two kinds of entry:
//!
//! - **Installed gluon:** `<home>/<gluon version>/config-lib/<rustc>/`.
//!   The generated `Cargo.toml` depends on `gluon-config =
//!   "=<version>"`. `cargo install gluon-cli` already downloaded that
//!   crate's sources, because gluon-core depends on it, so the build
//!   runs `--offline` first and goes online only if that fails.
//! - **Checkout:** `<home>/dev/config-lib/<key>/`, used when the
//!   checkout `build.rs` baked in still exists or
//!   `GLUON_CONFIG_SOURCE_DIR` is set. The `Cargo.toml` uses a path
//!   dependency on the checkout, and the key adds the hash of the
//!   gluon-config and gluon-model sources and the checkout's
//!   `Cargo.lock`. An edit therefore gets a new entry, and a
//!   development build never lands in a version-named one.
//!
//! `<rustc>` is [`RustcInfo::version_hash`] in hex. An entry holds the
//! generated `Cargo.toml` and `lib.rs`, its `Cargo.lock`, the cargo
//! target dir, and `manifest.json` with the resolved paths. Later runs
//! read the manifest and skip cargo entirely. cargo runs with `RUSTC`
//! set to the project's rustc, so the compiler in the key is the one
//! that built the library.

use crate::cache::CacheLock;
use crate::compile::RustcInfo;
use crate::error::{Error, Result};
use crate::script_runner::file_hashes::FileHashes;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Paths consumed by `cache::ensure_script_binary`'s rustc invocation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// gluon-config's `src/lib.rs`, as cargo reported it. `gluon
    /// configure` points rust-analyzer at it.
    pub gluon_config_src: Option<PathBuf>,
}

/// A built gluon-config library entry.
#[derive(Debug, Clone)]
pub struct ConfigLib {
    /// The entry directory under `$GLUON_HOME`.
    pub dir: PathBuf,
    pub paths: RlibPaths,
}

/// Where gluon-config's sources come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// crates.io, at this gluon's version.
    Registry,
    /// A gluon checkout (the workspace root).
    Checkout(PathBuf),
}

/// The gluon-config version an installed gluon depends on. The
/// workspace gives every gluon crate one version
/// (`workspace.package.version`), so gluon-core's is gluon-config's.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A file that only a gluon checkout has, relative to its root.
const CHECKOUT_MARKER: &str = "crates/gluon-config/Cargo.toml";

/// Build, or find already built, the gluon-config library for
/// `rustc_info`. Several gluon processes may call this at once: the
/// first builds the entry under its lock, the rest wait and then read
/// its manifest.
pub(crate) fn ensure_gluon_config_rlib(
    rustc_info: &RustcInfo,
    hashes: &mut FileHashes,
) -> Result<ConfigLib> {
    let home = crate::home::gluon_home()?;
    let source = source()?;
    let dir = entry_dir(&home, &source, rustc_info, hashes)?;
    let manifest_path = dir.join("manifest.json");

    // Freshness rule: the entry's directory name covers every input of
    // the build: the gluon version (for a checkout, the hash of the
    // gluon-config and gluon-model sources and Cargo.lock) and the
    // rustc identity. The entry's Cargo.lock is written by its first
    // build and passed `--locked` on every later one, so dependency
    // versions cannot drift. The manifest is trusted while every path
    // it records still exists; a missing one (a wiped target dir)
    // rebuilds. Not detected: a library file under `target/` replaced
    // by hand.
    if let Some(paths) = read_cached_manifest(&manifest_path) {
        return Ok(ConfigLib { dir, paths });
    }

    let _lock = CacheLock::acquire_file(
        &dir.join(".lock"),
        &format!("another gluon to build {}", dir.display()),
    )?;
    // Another process may have built the entry while we waited.
    if let Some(paths) = read_cached_manifest(&manifest_path) {
        return Ok(ConfigLib { dir, paths });
    }

    eprintln!(
        "gluon: building the gluon-config library for {} in {}",
        rustc_info.version,
        dir.display()
    );
    let paths = build_entry(&dir, &source, &rustc_info.rustc_path)?;
    write_cached_manifest(&manifest_path, &paths)?;
    Ok(ConfigLib { dir, paths })
}

/// A checkout when `GLUON_CONFIG_SOURCE_DIR` is set or the checkout
/// `build.rs` baked in still exists, else the registry. An installed
/// gluon bakes its own unpacked crate directory, which has no
/// [`CHECKOUT_MARKER`].
fn source() -> Result<Source> {
    if let Some(p) = std::env::var_os("GLUON_CONFIG_SOURCE_DIR") {
        let p = PathBuf::from(p);
        if !p.join(CHECKOUT_MARKER).is_file() {
            return Err(Error::Config(format!(
                "GLUON_CONFIG_SOURCE_DIR={} does not contain {CHECKOUT_MARKER}",
                p.display()
            )));
        }
        return Ok(Source::Checkout(p));
    }
    let baked = PathBuf::from(env!("GLUON_CONFIG_WORKSPACE_DIR"));
    if baked.join(CHECKOUT_MARKER).is_file() {
        Ok(Source::Checkout(baked))
    } else {
        Ok(Source::Registry)
    }
}

fn entry_dir(
    home: &Path,
    source: &Source,
    rustc_info: &RustcInfo,
    hashes: &mut FileHashes,
) -> Result<PathBuf> {
    let rustc = hex(&rustc_info.version_hash());
    Ok(match source {
        Source::Registry => home.join(VERSION).join("config-lib").join(rustc),
        Source::Checkout(workspace) => home
            .join("dev")
            .join("config-lib")
            .join(compute_cache_key(workspace, &rustc, hashes)?),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hash all `.rs` files and `Cargo.toml`s under `crates/gluon-config` and
/// `crates/gluon-model`, the checkout's `Cargo.lock`, and the rustc
/// identity. Determinism: the file list is sorted by path before
/// hashing. File hashes come from `hashes`, which skips files whose
/// mtime and size are unchanged.
fn compute_cache_key(
    workspace_dir: &Path,
    rustc_id: &str,
    hashes: &mut FileHashes,
) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(rustc_id.as_bytes());

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
            hasher.update(hashes.hash(&path)?);
        }
    }
    // The entry's first build copies this lock, so it decides the
    // transitive dependency versions and must be part of the key.
    let lock_path = workspace_dir.join("Cargo.lock");
    if lock_path.is_file() {
        hasher.update(hashes.hash(&lock_path)?);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Write the entry's package and run cargo in it. The caller holds
/// the entry lock.
fn build_entry(dir: &Path, source: &Source, rustc: &Path) -> Result<RlibPaths> {
    let lock = dir.join("Cargo.lock");
    // An existing lock was written by an earlier build of this entry;
    // `--locked` keeps that build's dependency versions.
    let locked = lock.is_file();
    write_file(&dir.join("Cargo.toml"), cargo_toml(source).as_bytes())?;
    write_file(
        &dir.join("lib.rs"),
        b"// Generated by gluon. Empty: only the gluon-config dependency is built.\n",
    )?;
    if !locked && let Source::Checkout(workspace) = source {
        // Start from the checkout's resolution so the library is built
        // with the dependency versions gluon itself was tested with.
        // cargo drops the entries this package does not use.
        let from = workspace.join("Cargo.lock");
        if from.is_file() {
            std::fs::copy(&from, &lock).map_err(|e| Error::Io {
                path: from,
                source: e,
            })?;
        }
    }

    let paths = run_cargo_build(dir, rustc, locked).map_err(|e| match (e, source) {
        (Error::Config(msg), Source::Registry) => Error::Config(format!(
            "{msg}\nnote: this gluon found no gluon checkout, so it builds gluon-config \
             {VERSION} from crates.io. Set GLUON_CONFIG_SOURCE_DIR to a gluon checkout \
             to build from its sources instead."
        )),
        (e, _) => e,
    })?;
    if !paths.gluon_config_rlib.is_file() {
        return Err(Error::Config(format!(
            "cargo build -p gluon-config in {} reported {} but the file does not exist",
            dir.display(),
            paths.gluon_config_rlib.display()
        )));
    }
    Ok(paths)
}

/// The entry's `Cargo.toml`. The empty `[workspace]` keeps cargo from
/// treating the entry as part of a workspace above it, for example
/// when `$GLUON_HOME` is inside a gluon checkout's `target/`.
fn cargo_toml(source: &Source) -> String {
    let dep = match source {
        Source::Registry => toml_string(&format!("={VERSION}")),
        Source::Checkout(workspace) => format!(
            "{{ path = {} }}",
            toml_string(&workspace.join("crates/gluon-config").to_string_lossy())
        ),
    };
    format!(
        "# Generated by gluon. Builds the gluon-config library that gluon.rs\n\
         # scripts link against.\n\
         [package]\n\
         name = \"gluon-config-lib\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         path = \"lib.rs\"\n\
         \n\
         [dependencies]\n\
         gluon-config = {dep}\n\
         \n\
         [workspace]\n"
    )
}

fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

/// The manifest at `path`, if it parses and every path it records
/// exists. Without the existence check, a wiped target dir would hand
/// the script's rustc invocation paths that fail with a confusing
/// `No such file or directory`.
fn read_cached_manifest(path: &Path) -> Option<RlibPaths> {
    let bytes = std::fs::read(path).ok()?;
    let paths: RlibPaths = serde_json::from_slice(&bytes).ok()?;
    let complete = paths.gluon_config_rlib.is_file()
        && paths
            .gluon_config_rmeta
            .as_ref()
            .is_none_or(|m| m.is_file())
        && paths.dep_dirs.iter().all(|d| d.is_dir());
    complete.then_some(paths)
}

/// Written to a temp file and renamed, because readers check the
/// manifest without taking the entry lock.
fn write_cached_manifest(path: &Path, paths: &RlibPaths) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(paths)
        .map_err(|e| Error::Config(format!("serialize rlib manifest: {e}")))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    write_file(&tmp, &bytes)?;
    std::fs::rename(&tmp, path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

/// `cargo build` for gluon-config in the entry `dir`, compiled by
/// `rustc`.
///
/// rustc refuses (E0514) to link a crate built by a different
/// compiler, and the script binary is compiled by the project's rustc,
/// so `RUSTC` makes cargo use that compiler too. The cargo beside that
/// rustc is preferred over the one on `$PATH`: under rustup, `cargo`
/// on `$PATH` picks a toolchain for the entry directory, which may not
/// exist or may be older than the project's. `--target-dir` keeps a
/// `CARGO_TARGET_DIR` from the environment from moving the build out
/// of the entry.
fn cargo_build_command(
    dir: &Path,
    rustc: &Path,
    locked: bool,
    offline: bool,
) -> std::process::Command {
    let sibling = rustc.with_file_name(if cfg!(windows) { "cargo.exe" } else { "cargo" });
    let cargo = if sibling.is_file() {
        sibling
    } else {
        PathBuf::from("cargo")
    };
    let mut cmd = std::process::Command::new(cargo);
    cmd.args([
        "build",
        "--release",
        "-p",
        "gluon-config",
        "--message-format=json",
    ])
    .arg("--manifest-path")
    .arg(dir.join("Cargo.toml"))
    .arg("--target-dir")
    .arg(dir.join("target"));
    if locked {
        cmd.arg("--locked");
    }
    if offline {
        cmd.arg("--offline");
    }
    cmd.env("RUSTC", rustc).current_dir(dir);
    cmd
}

/// Build offline first: the sources are usually in cargo's cache
/// already, and that keeps `gluon build` working without a network.
/// Only when that fails is the build retried online.
fn run_cargo_build(dir: &Path, rustc: &Path, locked: bool) -> Result<RlibPaths> {
    let run = |offline: bool| {
        cargo_build_command(dir, rustc, locked, offline)
            .output()
            .map_err(|e| Error::Io {
                path: PathBuf::from("cargo"),
                source: e,
            })
    };
    let mut output = run(true)?;
    if !output.status.success() {
        output = run(false)?;
    }
    if !output.status.success() {
        return Err(Error::Config(format!(
            "cargo build -p gluon-config failed in {} (status {}):\n{}",
            dir.display(),
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
    let mut gluon_config_src: Option<PathBuf> = None;
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
            gluon_config_src = v["target"]["src_path"].as_str().map(PathBuf::from);
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
        gluon_config_src,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty memo per call keeps these tests about the key
    /// itself; the memo's fast path has its own tests.
    fn memo() -> FileHashes {
        FileHashes::load(PathBuf::from("/nonexistent/gluon-test-memo.json"))
    }

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
{"reason":"compiler-artifact","target":{"name":"gluon-config","kind":["lib"],"src_path":"/src/gluon-config/src/lib.rs"},"filenames":["/t/release/libgluon_config.rlib","/t/release/deps/libgluon_config-c.rmeta"]}
{"reason":"build-finished","success":true}
"#;
        let paths = parse_cargo_output(stream.as_bytes()).expect("parse ok");
        assert_eq!(
            paths.gluon_config_src,
            Some(PathBuf::from("/src/gluon-config/src/lib.rs"))
        );
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
    fn parse_cargo_output_errors_when_no_lib_artifact() {
        let cases = [
            (
                "no compiler-artifact",
                r#"{"reason":"build-finished","success":true}"#,
            ),
            (
                // A build-script binary must not match.
                "only a custom-build artifact",
                r#"{"reason":"compiler-artifact","target":{"name":"gluon-config","kind":["custom-build"]},"filenames":["/x/build-script-build"]}"#,
            ),
        ];
        for (name, stream) in cases {
            assert!(parse_cargo_output(stream.as_bytes()).is_err(), "{name}");
        }
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
    fn cargo_build_uses_the_project_rustc_and_the_entry() {
        // The library is keyed on the project's rustc, and the script
        // is compiled by it, so cargo must build gluon-config with it
        // too, into the entry's own target dir.
        let rustc = Path::new("/toolchains/project/bin/rustc");
        let cmd = cargo_build_command(Path::new("/home/entry"), rustc, true, true);
        let rustc_env = cmd
            .get_envs()
            .find(|(k, _)| *k == "RUSTC")
            .and_then(|(_, v)| v);
        assert_eq!(rustc_env, Some(rustc.as_os_str()));
        let args: Vec<_> = cmd.get_args().collect();
        for want in ["--locked", "--offline", "/home/entry/target"] {
            assert!(
                args.contains(&std::ffi::OsStr::new(want)),
                "{want}: {args:?}"
            );
        }
    }

    #[test]
    fn cargo_toml_depends_on_the_source() {
        let cases = [
            (Source::Registry, format!("={VERSION}"), None),
            (
                Source::Checkout(PathBuf::from("/src/gluon")),
                String::new(),
                Some("/src/gluon/crates/gluon-config"),
            ),
        ];
        for (source, version, path) in cases {
            let doc: toml::Value = toml::from_str(&cargo_toml(&source)).expect("valid TOML");
            let dep = &doc["dependencies"]["gluon-config"];
            match path {
                None => assert_eq!(dep.as_str(), Some(version.as_str())),
                Some(p) => assert_eq!(dep["path"].as_str(), Some(p)),
            }
            assert!(doc.get("workspace").is_some(), "{source:?}");
        }
    }

    fn checkout() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn cache_key_is_deterministic_across_calls() {
        // Smoke test: hashing the same workspace twice must produce the
        // same key. This guards against accidental nondeterminism
        // (HashMap iteration order, mtime peeking, etc.).
        let workspace = checkout();
        let k1 = compute_cache_key(&workspace, "rustc-a", &mut memo()).expect("k1");
        let k2 = compute_cache_key(&workspace, "rustc-a", &mut memo()).expect("k2");
        assert_eq!(k1, k2);
    }

    #[test]
    fn cache_key_changes_with_rustc() {
        let workspace = checkout();
        let k1 = compute_cache_key(&workspace, "rustc-a", &mut memo()).expect("k1");
        let k2 = compute_cache_key(&workspace, "rustc-b", &mut memo()).expect("k2");
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
        let k1 = compute_cache_key(tmp.path(), "rustc 1.0", &mut memo()).unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), b"# version 2\n").unwrap();
        let k2 = compute_cache_key(tmp.path(), "rustc 1.0", &mut memo()).unwrap();
        assert_ne!(k1, k2, "cache key must invalidate when Cargo.lock changes");
    }
}
