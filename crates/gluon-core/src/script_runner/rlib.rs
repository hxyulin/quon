//! The gluon-config library that `gluon.rs` links against, built once
//! per set of gluon-config sources and rustc, and shared by every
//! project.
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
//! ## Where the sources come from
//!
//! gluon-config is not published. `build.rs` embeds its sources, those
//! of gluon-model, a workspace manifest for the two and gluon's own
//! `Cargo.lock` into the binary ([`CONFIG_SOURCES`]). An installed
//! gluon therefore needs no checkout and no crates.io release, and a
//! gluon built from a checkout uses exactly the sources it was built
//! from. Only third-party dependencies (serde, serde_json) come from
//! crates.io, at the versions in the embedded lock. Building gluon put
//! those in cargo's cache, so the build runs `--offline` first.
//!
//! ## Where the library lives
//!
//! Under [`crate::home::gluon_home`] (`$GLUON_HOME`, default
//! `~/.gluon`), in `<home>/config-lib/<key>/`. The key hashes the
//! embedded sources and [`RustcInfo::version_hash`]. An entry holds:
//!
//! - `src/`: the unpacked sources, which `gluon configure` points
//!   rust-analyzer at;
//! - `target/`: cargo's target dir;
//! - `manifest.json`: the resolved paths. Later runs read it and skip
//!   cargo entirely.
//!
//! cargo runs with `RUSTC` set to the project's rustc, so the compiler
//! in the key is the one that built the library.

use crate::cache::CacheLock;
use crate::compile::RustcInfo;
use crate::error::{Error, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/config_sources.rs"));

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

/// Written into an entry's `src/` after the first successful build.
/// Until then the unpacked tree may be partial and its `Cargo.lock` is
/// gluon's full lock, so the next attempt unpacks again.
const BUILT_MARKER: &str = ".built";

/// Build, or find already built, the gluon-config library for
/// `rustc_info`. Several gluon processes may call this at once: the
/// first builds the entry under its lock, the rest wait and then read
/// its manifest.
pub(crate) fn ensure_gluon_config_rlib(rustc_info: &RustcInfo) -> Result<ConfigLib> {
    let home = crate::home::gluon_home()?;
    let dir = home
        .join("config-lib")
        .join(entry_key(CONFIG_SOURCES, &rustc_info.version_hash()));
    let manifest_path = dir.join("manifest.json");

    // Freshness rule: the entry's directory name covers every input of
    // the build: the embedded sources, manifests and Cargo.lock, and
    // the rustc identity. A gluon with different gluon-config sources
    // uses a different entry. The manifest is trusted while every path
    // it records still exists; a missing one (a wiped target dir)
    // rebuilds. Not detected: a file under the entry replaced by hand.
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
    let paths = build_entry(&dir, &rustc_info.rustc_path)?;
    write_cached_manifest(&manifest_path, &paths)?;
    Ok(ConfigLib { dir, paths })
}

/// `sha256` over every embedded file's path and contents, then the
/// rustc identity, in hex. Lengths separate the fields so no two
/// different inputs hash the same bytes.
fn entry_key(sources: &[(&str, &[u8])], rustc_id: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (path, bytes) in sources {
        h.update((path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    h.update(rustc_id);
    format!("{:x}", h.finalize())
}

/// Unpack the sources if needed and run cargo. The caller holds the
/// entry lock.
fn build_entry(dir: &Path, rustc: &Path) -> Result<RlibPaths> {
    let src = dir.join("src");
    let marker = src.join(BUILT_MARKER);
    // After the first build, cargo has trimmed the lock to the two
    // crates' dependencies. `--locked` then keeps those versions. The
    // first build cannot pass it: trimming the lock is an update.
    let locked = marker.is_file();
    if !locked {
        unpack(&src, CONFIG_SOURCES)?;
    }
    let paths = run_cargo_build(dir, rustc, locked)?;
    if !paths.gluon_config_rlib.is_file() {
        return Err(Error::Config(format!(
            "cargo build -p gluon-config in {} reported {} but the file does not exist",
            src.display(),
            paths.gluon_config_rlib.display()
        )));
    }
    if !locked {
        write_file(&marker, b"")?;
    }
    Ok(paths)
}

/// Write `sources` under `src`, replacing whatever an earlier,
/// unfinished attempt left there.
fn unpack(src: &Path, sources: &[(&str, &[u8])]) -> Result<()> {
    if src.exists() {
        std::fs::remove_dir_all(src).map_err(|e| Error::Io {
            path: src.to_path_buf(),
            source: e,
        })?;
    }
    for (rel, bytes) in sources {
        let path = src.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        write_file(&path, bytes)?;
    }
    Ok(())
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

/// `cargo build` for gluon-config in the entry `dir` (sources in
/// `src/`, output in `target/`), compiled by `rustc`.
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
    .arg(dir.join("src").join("Cargo.toml"))
    .arg("--target-dir")
    .arg(dir.join("target"));
    if locked {
        cmd.arg("--locked");
    }
    if offline {
        cmd.arg("--offline");
    }
    cmd.env("RUSTC", rustc).current_dir(dir.join("src"));
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
    fn embedded_sources_are_a_workspace_of_the_two_crates() {
        let paths: Vec<&str> = CONFIG_SOURCES.iter().map(|(p, _)| *p).collect();
        assert!(paths.is_sorted(), "{paths:?}");
        for want in [
            "Cargo.lock",
            "Cargo.toml",
            "crates/gluon-config/Cargo.toml",
            "crates/gluon-config/src/lib.rs",
            "crates/gluon-model/Cargo.toml",
            "crates/gluon-model/src/lib.rs",
        ] {
            assert!(paths.contains(&want), "{want} missing: {paths:?}");
        }
        assert!(
            paths.iter().all(|p| p.starts_with("crates/gluon-config/")
                || p.starts_with("crates/gluon-model/")
                || !p.contains('/')),
            "{paths:?}"
        );

        let root = CONFIG_SOURCES
            .iter()
            .find(|(p, _)| *p == "Cargo.toml")
            .map(|(_, b)| std::str::from_utf8(b).unwrap())
            .unwrap();
        let doc: toml::Table = toml::from_str(root).expect("valid TOML");
        let ws = doc["workspace"].as_table().unwrap();
        let members: Vec<&str> = ws["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert_eq!(members, ["crates/gluon-config", "crates/gluon-model"]);
        // The members inherit these from the workspace.
        assert!(ws["package"].get("version").is_some());
        assert!(ws.get("lints").is_some());
        let deps = ws["dependencies"].as_table().unwrap();
        assert!(deps.contains_key("gluon-model"));
        assert!(!deps.contains_key("gluon-core"), "{deps:?}");
    }

    #[test]
    fn entry_key_covers_every_source_and_the_rustc() {
        type Sources<'a> = &'a [(&'a str, &'a [u8])];
        let base: Sources = &[("a.rs", b"one"), ("b.rs", b"two")];
        let key = entry_key(base, b"rustc 1");
        assert_eq!(key, entry_key(base, b"rustc 1"));
        let cases: [(&str, Sources, &[u8]); 4] = [
            ("rustc", base, b"rustc 2"),
            (
                "contents",
                &[("a.rs", b"one"), ("b.rs", b"TWO")],
                b"rustc 1",
            ),
            ("path", &[("a.rs", b"one"), ("c.rs", b"two")], b"rustc 1"),
            // Bytes moved across the path/contents boundary.
            (
                "boundary",
                &[("a.rso", b"ne"), ("b.rs", b"two")],
                b"rustc 1",
            ),
        ];
        for (what, sources, rustc) in cases {
            assert_ne!(key, entry_key(sources, rustc), "{what}");
        }
    }

    #[test]
    fn unpack_replaces_an_unfinished_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("stale")).unwrap();
        std::fs::write(src.join("stale/old.rs"), b"left over").unwrap();
        unpack(
            &src,
            &[
                ("Cargo.toml", b"[workspace]\n"),
                ("crates/a/src/lib.rs", b""),
            ],
        )
        .unwrap();
        assert!(!src.join("stale").exists());
        assert_eq!(
            std::fs::read(src.join("Cargo.toml")).unwrap(),
            b"[workspace]\n"
        );
        assert!(src.join("crates/a/src/lib.rs").is_file());
    }
}
