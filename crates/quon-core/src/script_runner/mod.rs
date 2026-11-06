//! Compile and run a user-authored `quon.rs` build script, then return a
//! fully-interned [`BuildModel`].
//!
//! ## Pipeline
//!
//! ```text
//!   quon.rs source
//!         │
//!         ▼
//!   ┌─────────────────────┐
//!   │ rlib::ensure_..()   │  build (or find) libquon_config.rlib in ~/.quon
//!   └─────────┬───────────┘
//!             │
//!             ▼
//!   ┌─────────────────────┐
//!   │ cache::ensure_..()  │  rustc a generated main.rs that calls quon.rs
//!   └─────────┬───────────┘
//!             │
//!             ▼
//!   ┌─────────────────────┐
//!   │ run_script_binary   │  spawn the binary, answer its questions, capture stdout
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
//! The rlib build is *expensive* (~5 s cold) but rarely changes: it
//! depends only on the quon version and rustc, so one build in
//! `$QUON_HOME` serves every project (see [`rlib`]). The script binary
//! is *cheap* (~1 s) but invalidates on every `quon.rs` edit, and
//! lives in the project's `build/script-runner/`. Splitting them lets a
//! normal "edit quon.rs, re-run" loop avoid the rlib cost.
//!
//! ## Concurrency
//!
//! The rlib build takes a lock on its entry in `$QUON_HOME`, so
//! concurrent quon processes build it once. The script compile takes
//! no lock: two concurrent runs in one project may both compile the
//! script into the same entry directory, which costs a redundant
//! compile.
//!
//! ## Error surface
//!
//! Every failure mode below maps to a single [`Error`] variant. The CLI
//! renders these uniformly via [`Error`]'s `Display`.
//!
//! - missing `quon.rs`: `Error::Config`
//! - rustc compile failure: `Error::Config` (full stderr inline)
//! - script process failure (non-zero exit / signal): `Error::Config`
//! - malformed stdout (not RawBuildModel JSON): `Error::Config`
//! - kconfig load failure: `Error::Diagnostics`
//! - kconfig duplicate key: `Error::Diagnostics`
//! - intern dangling reference: `Error::Diagnostics`

use crate::compile::RustcInfo;
use crate::config::{RunSelection, select_run};
use crate::error::{Diagnostic, Error, Result};
use crate::manifest::lower::LoadedWorkspace;
use quon_model::{BuildModel, RawBuildModel};
use std::path::Path;

pub mod cache;
mod file_hashes;
pub mod rlib;

/// Compile `<project_root>/quon.rs`, run it, parse stdout, and return
/// the fully-interned [`BuildModel`].
///
/// `build_dir` is the directory under which all script-runner cache
/// state is stored (typically `<project_root>/build`). `rustc_info` is
/// the probed compiler that will compile the script.
pub fn compile_and_run(
    project_root: &Path,
    build_dir: &Path,
    rustc_info: &RustcInfo,
    selection: &RunSelection,
) -> Result<BuildModel> {
    let raw = compile_and_run_raw(project_root, build_dir, rustc_info)?;
    finalize(
        raw,
        project_root,
        build_dir,
        rustc_info,
        selection,
        &mut LoadedWorkspace::default(),
    )
}

/// [`compile_and_run`] for several run entries: `quon.rs` runs once and
/// the workspace manifests are read once, then each entry's model is
/// finalized from its own copy. Models come back in `selections` order.
pub fn compile_and_run_entries(
    project_root: &Path,
    build_dir: &Path,
    rustc_info: &RustcInfo,
    selections: &[RunSelection],
) -> Result<Vec<BuildModel>> {
    let raw = compile_and_run_raw(project_root, build_dir, rustc_info)?;
    let mut loaded = LoadedWorkspace::default();
    selections
        .iter()
        .map(|selection| {
            finalize(
                raw.clone(),
                project_root,
                build_dir,
                rustc_info,
                selection,
                &mut loaded,
            )
        })
        .collect()
}

/// [`compile_and_run`] without the final transform: the model exactly
/// as `quon.rs` printed it, with no `.kconfig` merged and no Cargo
/// workspace read. `quon vendor` uses it, because a stale vendor
/// directory would fail the full load.
pub fn compile_and_run_raw(
    project_root: &Path,
    build_dir: &Path,
    rustc_info: &RustcInfo,
) -> Result<RawBuildModel> {
    let script_path = project_root.join("quon.rs");
    if !script_path.is_file() {
        return Err(Error::Config(format!(
            "no quon.rs at {}",
            script_path.display()
        )));
    }

    let lib = rlib::ensure_quon_config_rlib(rustc_info)?;
    let mut hashes =
        file_hashes::FileHashes::load(build_dir.join("script-runner").join("file-hashes.json"));
    let bin =
        cache::ensure_script_binary(&script_path, build_dir, rustc_info, &lib.paths, &mut hashes)?;
    // A failed save only means the next run rehashes; it must not fail
    // a build that has everything it needs.
    let _ = hashes.save();
    let stdout = run_script_binary(&bin, project_root)?;

    serde_json::from_slice(&stdout).map_err(|e| {
        Error::Config(format!(
            "quon.rs stdout was not valid RawBuildModel JSON: {e}"
        ))
    })
}

/// Build the shared quon-config library for `rustc_info` if it is
/// missing, and return it. `quon setup` calls this ahead of time;
/// other commands build it on first use.
pub fn ensure_config_lib(rustc_info: &RustcInfo) -> Result<rlib::ConfigLib> {
    rlib::ensure_quon_config_rlib(rustc_info)
}

/// Spawn the compiled script binary, answer the questions it asks
/// (see [`quon_model::script_protocol`]), and return the rest of its
/// stdout: the model. Stderr is captured too, but only surfaced if the
/// process exits non-zero. A successful run discards stderr (the
/// script shouldn't be writing there in normal operation).
fn run_script_binary(bin: &Path, project_root: &Path) -> Result<Vec<u8>> {
    use quon_model::script_protocol::REQUEST_PREFIX;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command, Stdio};

    let io_error = |source| Error::Io {
        path: bin.to_path_buf(),
        source,
    };
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_error)?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    // quon blocks on stdout while the script runs. A script that fills
    // the stderr pipe meanwhile would block too, so stderr drains on a
    // thread of its own.
    let stderr = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let mut model = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if stdout.read_until(b'\n', &mut line).map_err(io_error)? == 0 {
            break;
        }
        match line.strip_prefix(REQUEST_PREFIX.as_bytes()) {
            Some(request) => {
                let mut reply = serde_json::to_vec(&answer(request, project_root))
                    .expect("a reply always serializes");
                reply.push(b'\n');
                // A script that exits without reading its answer closes
                // the pipe; its exit status, checked below, says why.
                let _ = stdin.write_all(&reply).and_then(|()| stdin.flush());
            }
            None => model.extend_from_slice(&line),
        }
    }
    drop(stdin);
    let status = child.wait().map_err(io_error)?;
    let stderr = stderr.join().unwrap_or_default();
    if !status.success() {
        return Err(Error::Config(format!(
            "quon.rs exited with status {status}\nstderr:\n{}",
            String::from_utf8_lossy(&stderr),
        )));
    }
    Ok(model)
}

/// The reply to one question from `quon.rs`. Every failure becomes an
/// error reply, which the script records and reports with its other
/// config errors.
fn answer(request: &[u8], project_root: &Path) -> quon_model::script_protocol::ScriptReply {
    use quon_model::script_protocol::{ScriptReply, ScriptRequest};
    match serde_json::from_slice::<ScriptRequest>(request) {
        Ok(ScriptRequest::Members { dir }) => {
            match crate::manifest::script_members(&project_root.join(&dir)) {
                Ok(members) => ScriptReply::Members(members),
                // The script reports the reply as one of its own errors,
                // which carry the `error:` level already.
                Err(Error::Diagnostics(diags)) => ScriptReply::Error(
                    diags
                        .iter()
                        .map(|d| {
                            let mut text = match &d.span {
                                Some(span) => format!("{span}: {}", d.message),
                                None => d.message.clone(),
                            };
                            for note in &d.notes {
                                text.push_str(&format!("\n    note: {note}"));
                            }
                            text
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(e) => ScriptReply::Error(e.to_string()),
            }
        }
        Err(e) => ScriptReply::Error(format!("quon does not understand the question: {e}")),
    }
}

/// Final-stage transform: pick the run entry `selection` names
/// ([`select_run`]), resolve `kconfig_includes` against the
/// project root, merge their lowered options/presets into the raw
/// model, expand Cargo workspace package roots into crates
/// ([`crate::manifest::lower::expand`]), run intern, and translate any
/// intern diagnostics into quon-core's [`Diagnostic`] type.
///
/// A `.kconfig` option or preset with the name of one `quon.rs`
/// declared is an error, as a repeat within `quon.rs` is.
fn finalize(
    mut raw: RawBuildModel,
    project_root: &Path,
    build_dir: &Path,
    rustc_info: &RustcInfo,
    selection: &RunSelection,
    loaded: &mut LoadedWorkspace,
) -> Result<BuildModel> {
    // Before expansion, which resolves features over the roots that
    // selection keeps.
    select_run(&mut raw, selection)?;

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
        // quon-core's Diagnostic form — pass through as-is.
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

    // Manifests are read here, on every run, not by the script binary:
    // editing a Cargo.toml must not recompile quon.rs, and the
    // expansion can never be stale. The cfg probe cache in the build
    // directory is the file the build layout names
    // (`BuildLayout::target_cfg_cache`).
    let warnings = crate::manifest::lower::expand_loaded(
        &mut raw,
        project_root,
        rustc_info,
        Some(build_dir),
        loaded,
    )?;
    for w in &warnings {
        eprintln!("{w}");
    }

    let (model, intern_diags) = quon_model::intern(raw);
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

    fn write_minimal_quon_rs(dir: &Path) {
        // Mirror the body in the plan's Step 4 verbatim so the parity
        // test in Task 12 can reuse a structurally-identical script.
        let body = r#"
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("demo", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));
    q.add(Profile::dev());
    Ok(())
}
"#;
        fs::write(dir.join("quon.rs"), body).unwrap();
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
        write_minimal_quon_rs(tmp.path());
        let info = RustcInfo::probe().unwrap();
        let model = compile_and_run(
            tmp.path(),
            &tmp.path().join("build"),
            &info,
            &RunSelection::All,
        )
        .expect("compile_and_run");

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
    fn a_question_quon_cannot_answer_gets_an_error_reply() {
        use quon_model::script_protocol::ScriptReply;
        let tmp = tempfile::tempdir().unwrap();
        let reply = answer(br#"{"members":{"dir":"nowhere"}}"#, tmp.path());
        assert!(
            matches!(&reply, ScriptReply::Error(e) if e.contains("nowhere")),
            "{reply:?}"
        );
        // A loader diagnostic arrives without its `error:` level, which the
        // script's own error report adds.
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\"]\n",
        )
        .unwrap();
        let reply = answer(br#"{"members":{"dir":"."}}"#, tmp.path());
        assert!(
            matches!(&reply, ScriptReply::Error(e) if !e.starts_with("error")),
            "{reply:?}"
        );
        let reply = answer(b"{\"weather\":{}}", tmp.path());
        assert!(
            matches!(&reply, ScriptReply::Error(e) if e.contains("does not understand")),
            "{reply:?}"
        );
    }

    #[test]
    fn compile_and_run_errors_when_no_script() {
        let tmp = tempfile::tempdir().unwrap();
        // The missing-script check runs before rustc is touched, so a
        // hand-built RustcInfo keeps this test from spawning rustc.
        let info = RustcInfo {
            rustc_path: "/nonexistent/rustc".into(),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "0.0.0".into(),
            sysroot: "/nonexistent/sysroot".into(),
            rust_src: None,
            mtime_ns: 0,
        };
        let err = compile_and_run(
            tmp.path(),
            &tmp.path().join("build"),
            &info,
            &RunSelection::All,
        )
        .expect_err("must error when no quon.rs");
        match err {
            Error::Config(msg) => assert!(
                msg.contains("no quon.rs"),
                "expected missing-script message, got: {msg}"
            ),
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }
}
