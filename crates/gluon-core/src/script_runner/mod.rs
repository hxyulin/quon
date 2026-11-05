//! Compile and run a user-authored `gluon.rs` build script, then return a
//! fully-interned [`BuildModel`].
//!
//! ## Pipeline
//!
//! ```text
//!   gluon.rs source
//!         │
//!         ▼
//!   ┌─────────────────────┐
//!   │ rlib::ensure_..()   │  build (or cache-hit) libgluon_config.rlib
//!   └─────────┬───────────┘
//!             │
//!             ▼
//!   ┌─────────────────────┐
//!   │ cache::ensure_..()  │  rustc the user's gluon.rs into a binary
//!   └─────────┬───────────┘
//!             │
//!             ▼
//!   ┌─────────────────────┐
//!   │ run_script_binary   │  spawn the binary, capture stdout
//!   └─────────┬───────────┘
//!             │
//!             ▼  RawBuildModel JSON
//!   ┌─────────────────────┐
//!   │ finalize()          │  merge .kconfig includes, intern handles
//!   └─────────┬───────────┘
//!             ▼
//!         BuildModel
//! ```
//!
//! ## Why two cache layers
//!
//! The rlib build is *expensive* (~5 s cold) but rarely changes — it
//! invalidates only on rustc upgrade or `gluon-config`/`gluon-model`
//! source edits. The script binary is *cheap* (~1 s) but invalidates on
//! every `gluon.rs` edit. Splitting them lets a normal "edit gluon.rs,
//! re-run" loop avoid the rlib cost.
//!
//! ## Stage 1 limitations
//!
//! - **No advisory lock around the on-demand cargo build or script
//!   compilation.** Two concurrent `gluon build` invocations may both
//!   trigger `cargo build -p gluon-config` against the same `target/`
//!   directory; cargo has its own `.cargo-lock`, so the cargo invocations
//!   themselves serialise correctly, but they will both write to the
//!   same script-runner cache directory. In the worst case this produces
//!   a redundant rebuild rather than corruption (`std::fs::write` is
//!   atomic on POSIX). Stage 1.7 follow-up may add explicit locking via
//!   the existing `crate::cache::lock::CacheLock` infrastructure.
//! - **`Cargo.lock` is hashed into the cache key but cargo's transitive
//!   library files are not.** If a transitive rlib in a dep dir is replaced with
//!   a different SVH that happens to compile cleanly against the
//!   user's `gluon.rs`, the cache may serve a stale binary. In practice
//!   cargo's incremental rebuild keeps the deps in sync — the only path
//!   where this matters is hand-deletion of files under `target/`.
//!
//! ## Error surface
//!
//! Every failure mode below maps to a single [`Error`] variant. The CLI
//! renders these uniformly via [`Error`]'s `Display`.
//!
//! - missing `gluon.rs`: `Error::Config`
//! - rustc compile failure: `Error::Config` (full stderr inline)
//! - script process failure (non-zero exit / signal): `Error::Config`
//! - malformed stdout (not RawBuildModel JSON): `Error::Config`
//! - kconfig load failure: `Error::Diagnostics`
//! - kconfig duplicate key: `Error::Diagnostics`
//! - intern dangling reference: `Error::Diagnostics`

use crate::compile::RustcInfo;
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{BuildModel, RawBuildModel};
use std::path::Path;

pub mod cache;
pub mod rlib;

/// Compile `<project_root>/gluon.rs`, run it, parse stdout, and return
/// the fully-interned [`BuildModel`].
///
/// `build_dir` is the directory under which all script-runner cache
/// state is stored (typically `<project_root>/build`). `rustc_info` is
/// the probed compiler that will compile the script.
pub fn compile_and_run(
    project_root: &Path,
    build_dir: &Path,
    rustc_info: &RustcInfo,
) -> Result<BuildModel> {
    let script_path = project_root.join("gluon.rs");
    if !script_path.is_file() {
        return Err(Error::Config(format!(
            "no gluon.rs at {}",
            script_path.display()
        )));
    }

    let rlib = rlib::ensure_gluon_config_rlib(build_dir, rustc_info)?;
    let bin = cache::ensure_script_binary(&script_path, build_dir, rustc_info, &rlib)?;
    let stdout = run_script_binary(&bin)?;

    let raw: RawBuildModel = serde_json::from_slice(&stdout).map_err(|e| {
        Error::Config(format!(
            "gluon.rs stdout was not valid RawBuildModel JSON: {e}"
        ))
    })?;

    finalize(raw, project_root)
}

/// Spawn the compiled script binary and capture its stdout. Stderr is
/// captured too — but only surfaced if the process exits non-zero. A
/// successful run discards stderr (the script shouldn't be writing
/// there in normal operation).
fn run_script_binary(bin: &Path) -> Result<Vec<u8>> {
    use std::process::Command;
    let output = Command::new(bin).output().map_err(|e| Error::Io {
        path: bin.to_path_buf(),
        source: e,
    })?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "gluon.rs exited with status {}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        )));
    }
    Ok(output.stdout)
}

/// Final-stage transform: resolve `kconfig_includes` against the
/// project root, merge their lowered options/presets into the raw
/// model, run intern, and translate any intern diagnostics into
/// gluon-core's [`Diagnostic`] type.
///
/// Mirrors `gluon-config/src/builders/config_option.rs` for cross-source
/// duplicate detection so `.rs` and `.kconfig` users see the same
/// diagnostic wording.
fn finalize(mut raw: RawBuildModel, project_root: &Path) -> Result<BuildModel> {
    // Drain the include list — it's a sidecar field that doesn't belong
    // in the final BuildModel and we don't want it accidentally
    // re-serialised downstream.
    let includes = std::mem::take(&mut raw.kconfig_includes);

    for kconfig_path in includes {
        let abs = if kconfig_path.is_absolute() {
            kconfig_path.clone()
        } else {
            project_root.join(&kconfig_path)
        };

        // load_kconfig returns its own Vec<Diagnostic> already in
        // gluon-core's Diagnostic form — pass through as-is.
        let lowered = crate::kconfig::load_kconfig(&abs).map_err(Error::Diagnostics)?;

        for (name, opt) in lowered.options {
            let (_, inserted) = raw.config_options.insert(name.clone(), opt);
            if !inserted {
                return Err(Error::Diagnostics(vec![Diagnostic::error(format!(
                    "config option '{name}' from .kconfig conflicts with an existing declaration"
                ))]));
            }
        }
        for (name, preset) in lowered.presets {
            if raw.presets.contains_key(&name) {
                return Err(Error::Diagnostics(vec![Diagnostic::error(format!(
                    "preset '{name}' from .kconfig conflicts with an existing declaration"
                ))]));
            }
            raw.presets.insert(name, preset);
        }
    }

    let (model, intern_diags) = gluon_model::intern(raw);
    if !intern_diags.is_empty() {
        let diags: Vec<Diagnostic> = intern_diags
            .into_iter()
            .map(|d| Diagnostic::error(d.message).with_optional_span(d.span))
            .collect();
        return Err(Error::Diagnostics(diags));
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_minimal_gluon_rs(dir: &Path) {
        // Mirror the body in the plan's Step 4 verbatim so the parity
        // test in Task 12 can reuse a structurally-identical script.
        let body = r#"
fn main() -> gluon_config::Result<()> {
    let mut cfg = gluon_config::Config::new();
    cfg.project("demo", "0.1.0");
    let t = cfg.target("x86_64-unknown-none");
    cfg.profile("dev").target(t).opt_level(0).debug_info(true);
    cfg.emit()
}
"#;
        fs::write(dir.join("gluon.rs"), body).unwrap();
    }

    #[test]
    #[ignore = "requires rustc + spawns cargo build; gated behind --ignored"]
    fn compile_and_run_minimal_script() {
        // Skip if we can't even probe rustc (e.g. an isolated CI sandbox).
        if RustcInfo::probe().is_err() {
            eprintln!("skipping: RustcInfo::probe() failed");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        write_minimal_gluon_rs(tmp.path());
        let info = RustcInfo::probe().unwrap();
        let model =
            compile_and_run(tmp.path(), &tmp.path().join("build"), &info).expect("compile_and_run");

        assert_eq!(
            model.project.as_ref().unwrap().name,
            "demo",
            "project name should round-trip through the script binary"
        );
        assert!(
            model.profiles.lookup("dev").is_some(),
            "profile 'dev' should exist after intern"
        );
    }

    #[test]
    fn compile_and_run_errors_when_no_script() {
        let tmp = tempfile::tempdir().unwrap();
        let info = match RustcInfo::probe() {
            Ok(i) => i,
            Err(_) => return, // skip: no rustc available
        };
        let err = compile_and_run(tmp.path(), &tmp.path().join("build"), &info)
            .expect_err("must error when no gluon.rs");
        match err {
            Error::Config(msg) => assert!(
                msg.contains("no gluon.rs"),
                "expected missing-script message, got: {msg}"
            ),
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }
}
