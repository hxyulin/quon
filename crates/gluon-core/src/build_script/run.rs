//! Running one build script and recording its output.
//!
//! [`run_build_script`] is the action of a `RunBuildScript` node. It
//! checks the run's fingerprint record, and on a hit returns the
//! [`BuildScriptOutput`] stored in the record's `data` without running
//! anything. Otherwise it runs the compiled script with the environment
//! the caller assembled ([`super::script_env`]), parses its stdout, and
//! records the output with the run.
//!
//! ### Freshness
//!
//! - Command: the script binary's path, the environment Gluon sets and
//!   the working directory (the package directory).
//! - Declared input: the script binary, so a recompiled script reruns.
//! - Discovered inputs: the `rerun-if-changed` paths. A script that
//!   prints none is rerun when any file in its package changes, as in
//!   Cargo. That walk skips `target/`, dot-directories and Gluon's own
//!   build directory, which sits inside the package when the package is
//!   the project root.
//! - Env: the `rerun-if-env-changed` names, with the values the script
//!   saw.
//! - Output: the `output` file holding the script's stdout.
//!
//! A rerun that prints the same directives leaves the owner's rustc
//! command, and so the owner, unchanged.

use super::{BuildScriptOutput, ParseContext, parse_output, should_remove_inherited};
use crate::cache::{Freshness, FreshnessQuery, RecordInput, hash_argv};
use crate::compile::CompileCtx;
use crate::error::{Diagnostic, Error, Result};
use crate::scheduler::NodeData;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// One build-script run.
pub(crate) struct RunInput<'a> {
    /// Names the run in `-v` stale reasons and errors, such as
    /// `build script of crate foo (host)`.
    pub what: String,
    pub key: String,
    /// The compiled script.
    pub binary: &'a Path,
    /// What Gluon sets for the script; see [`super::script_env`].
    /// Inherited variables that [`should_remove_inherited`] names are
    /// removed first.
    pub env: BTreeMap<String, OsString>,
    /// The package directory: the working directory, and the root
    /// relative `rerun-if-changed` paths resolve against.
    pub manifest_dir: &'a Path,
    /// Holds `out/` (`OUT_DIR`) and `output` (the script's stdout).
    pub run_dir: &'a Path,
    /// The package declares `links`, which allows `cargo::metadata`.
    pub has_links: bool,
}

/// Check, run and record one build script; see the module docs.
/// Returns the parsed output and whether the record was fresh.
pub(crate) fn run_build_script(
    ctx: &CompileCtx,
    input: RunInput<'_>,
    stderr_sink: &mut Vec<u8>,
) -> Result<(Arc<BuildScriptOutput>, bool)> {
    let env_os: BTreeMap<OsString, OsString> = input
        .env
        .iter()
        .map(|(k, v)| (OsString::from(k), v.clone()))
        .collect();
    let command = hash_argv(input.binary, &[], &env_os, Some(input.manifest_dir));
    let toolchain = ctx.toolchain();
    let declared = [input.binary.to_path_buf()];
    let output_file = input.run_dir.join("output");
    let outputs = [output_file.clone()];
    let query = FreshnessQuery {
        key: &input.key,
        command,
        toolchain,
        declared: &declared,
        outputs: &outputs,
        command_env: &env_os,
    };
    match ctx.store.check(&query) {
        Freshness::Fresh(fp) => {
            if let Some(NodeData::BuildScript(out)) = &fp.data {
                stderr_sink.extend_from_slice(&ctx.store.stderr(&fp));
                return Ok((Arc::clone(out), true));
            }
            // A run record always carries its output; one without is
            // not trusted.
            ctx.report_stale(
                stderr_sink,
                &input.what,
                &crate::cache::StaleReason::FormatChanged,
            );
        }
        Freshness::Stale(reason) => ctx.report_stale(stderr_sink, &input.what, &reason),
    }

    let out_dir = input.run_dir.join("out");
    std::fs::create_dir_all(&out_dir).map_err(|e| io_err(&out_dir, e))?;
    let mut cmd = Command::new(input.binary);
    cmd.current_dir(input.manifest_dir);
    for (name, _) in std::env::vars_os() {
        if should_remove_inherited(&name) {
            cmd.env_remove(&name);
        }
    }
    cmd.envs(&env_os);
    let started_at = std::time::SystemTime::now();
    let output = cmd.output().map_err(|e| io_err(input.binary, e))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "{} failed: exit={:?}",
                input.what,
                output.status.code()
            ))
            .with_note(format!("stdout:\n{stdout}"))
            .with_note(format!(
                "stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )),
        ]));
    }
    let parsed = parse_output(
        &stdout,
        ParseContext {
            manifest_dir: input.manifest_dir,
            has_links: input.has_links,
        },
    )
    .map_err(|e| {
        Error::Diagnostics(vec![Diagnostic::error(format!(
            "{} printed a directive Gluon cannot use: {e}",
            input.what
        ))])
    })?;
    if !parsed.errors.is_empty() {
        let mut diag = Diagnostic::error(format!("{} reported errors", input.what));
        for e in &parsed.errors {
            diag = diag.with_note(e.clone());
        }
        return Err(Error::Diagnostics(vec![diag]));
    }
    std::fs::write(&output_file, &output.stdout).map_err(|e| io_err(&output_file, e))?;

    let mut warnings = String::new();
    for w in &parsed.warnings {
        warnings.push_str(&format!("warning: {}: {w}\n", input.what));
    }
    stderr_sink.extend_from_slice(warnings.as_bytes());

    let env: BTreeMap<String, Option<String>> = parsed
        .rerun_if_env_changed
        .iter()
        .map(|name| {
            let value = match input.env.get(name) {
                Some(v) => v.to_str().map(str::to_owned),
                None => std::env::var(name).ok(),
            };
            (name.clone(), value)
        })
        .collect();
    let out = Arc::new(parsed);
    // A `rerun-if-changed` path that does not exist is always stale, as
    // in Cargo: the run is not recorded, so the next build reruns it.
    if let Some(discovered) = discovered_inputs(&out, input.manifest_dir, ctx.layout.root()) {
        ctx.store.record(RecordInput {
            key: &input.key,
            command,
            toolchain,
            declared: &declared,
            discovered: &discovered,
            env,
            outputs: &outputs,
            started_at,
            stderr: warnings.as_bytes(),
            data: Some(NodeData::BuildScript(Arc::clone(&out))),
        })?;
    }
    Ok((out, false))
}

/// The files whose change reruns the script: the `rerun-if-changed`
/// paths, with directories expanded to the files under them, or every
/// file in the package when the script names none. `None` when a named
/// path does not exist. `build_dir` is Gluon's build directory, skipped
/// by the package walk.
fn discovered_inputs(
    out: &BuildScriptOutput,
    manifest_dir: &Path,
    build_dir: &Path,
) -> Option<Vec<PathBuf>> {
    let roots: Vec<&Path> = if out.rerun_if_changed.is_empty() {
        vec![manifest_dir]
    } else {
        out.rerun_if_changed.iter().map(PathBuf::as_path).collect()
    };
    let mut files = Vec::new();
    for root in roots {
        if !root.exists() {
            return None;
        }
        let walk = walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            // The package walk skips build output and VCS metadata;
            // an explicitly named path is taken as given. Other nodes
            // rewrite files under the build directory on every build.
            .filter_entry(|e| {
                e.depth() == 0
                    || !(e.file_type().is_dir()
                        && (e.file_name() == "target"
                            || e.file_name().to_string_lossy().starts_with('.')
                            || e.path() == build_dir))
            });
        for entry in walk.filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                files.push(entry.into_path());
            }
        }
    }
    files.sort();
    files.dedup();
    Some(files)
}

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::compile::{BuildLayout, RustcInfo};
    use std::os::unix::fs::PermissionsExt;

    struct Fx {
        tmp: tempfile::TempDir,
        pkg: PathBuf,
        script: PathBuf,
    }

    impl Fx {
        /// A package with `input.txt`, and a "compiled script" that is a
        /// shell script printing `body`.
        fn new(body: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let pkg = tmp.path().join("pkg");
            std::fs::create_dir_all(&pkg).unwrap();
            std::fs::write(pkg.join("input.txt"), b"v1").unwrap();
            let script = tmp.path().join("build-script-build");
            let fx = Self { tmp, pkg, script };
            fx.set_script(body);
            fx
        }

        fn set_script(&self, body: &str) {
            std::fs::write(&self.script, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&self.script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn ctx(&self) -> CompileCtx {
            CompileCtx::new(
                BuildLayout::new(self.tmp.path().join("build"), "demo"),
                Arc::new(RustcInfo {
                    rustc_path: PathBuf::from("/usr/bin/rustc"),
                    rustc_arg: "rustc".into(),
                    version: "rustc 0.0.0".into(),
                    host_triple: "x86_64-unknown-linux-gnu".into(),
                    commit_hash: None,
                    release: "0.0.0".into(),
                    sysroot: PathBuf::from("/fake-sysroot"),
                    rust_src: None,
                    mtime_ns: 0,
                }),
            )
        }

        fn run(&self, ctx: &CompileCtx, sink: &mut Vec<u8>) -> (Arc<BuildScriptOutput>, bool) {
            let run_dir = self.tmp.path().join("build/host/build/pkg-0123");
            let env = [("OUT_DIR".to_string(), OsString::from(run_dir.join("out")))].into();
            run_build_script(
                ctx,
                RunInput {
                    what: "build script of crate pkg (host)".into(),
                    key: "run:crate:host:pkg".into(),
                    binary: &self.script,
                    env,
                    manifest_dir: &self.pkg,
                    run_dir: &run_dir,
                    has_links: false,
                },
                sink,
            )
            .expect("run")
        }
    }

    const BODY: &str = "echo cargo::rustc-cfg=has_x\n\
                        echo cargo::rerun-if-changed=input.txt\n\
                        echo cargo::warning=generated\n\
                        echo hello > \"$OUT_DIR/gen.rs\"\n";

    #[test]
    fn run_records_output_and_a_hit_restores_it_without_running() {
        let fx = Fx::new(BODY);
        let mut sink = Vec::new();
        let (out, cached) = fx.run(&fx.ctx(), &mut sink);
        assert!(!cached);
        assert_eq!(out.cfgs, ["has_x"]);
        assert!(String::from_utf8_lossy(&sink).contains("generated"));

        // A new context reads the record from disk. The script writes
        // `gen.rs` whenever it runs, so its absence afterwards shows
        // the hit ran nothing.
        let gen_rs = fx.tmp.path().join("build/host/build/pkg-0123/out/gen.rs");
        std::fs::remove_file(&gen_rs).unwrap();
        let mut sink = Vec::new();
        let (again, cached) = fx.run(&fx.ctx(), &mut sink);
        assert!(!gen_rs.exists(), "a hit must not run the script");
        assert!(cached, "unchanged run must be a hit");
        assert_eq!(*again, *out);
        assert!(
            String::from_utf8_lossy(&sink).contains("generated"),
            "warnings replay on a hit"
        );
    }

    #[test]
    fn rerun_if_changed_input_reruns_and_same_output_is_equal() {
        let fx = Fx::new(BODY);
        let ctx = fx.ctx();
        let (first, _) = fx.run(&ctx, &mut Vec::new());
        std::fs::write(fx.pkg.join("input.txt"), b"v2").unwrap();
        let (second, cached) = fx.run(&ctx, &mut Vec::new());
        assert!(!cached, "a rerun-if-changed edit must rerun the script");
        assert_eq!(*first, *second, "same directives, so the owner stays fresh");
    }

    #[test]
    fn recompiled_script_reruns() {
        let fx = Fx::new(BODY);
        let ctx = fx.ctx();
        fx.run(&ctx, &mut Vec::new());
        fx.set_script(&BODY.replace("has_x", "has_y"));
        let (out, cached) = fx.run(&ctx, &mut Vec::new());
        assert!(!cached);
        assert_eq!(out.cfgs, ["has_y"]);
    }

    #[test]
    fn rerun_if_env_changed_is_recorded() {
        let fx = Fx::new(
            "echo cargo::rerun-if-env-changed=OUT_DIR\necho cargo::rerun-if-changed=input.txt\n",
        );
        let ctx = fx.ctx();
        fx.run(&ctx, &mut Vec::new());
        let fp: crate::cache::Fingerprint = serde_json::from_slice(
            &std::fs::read(ctx.store.record_path("run:crate:host:pkg")).unwrap(),
        )
        .unwrap();
        assert!(fp.env.contains_key("OUT_DIR"));
        assert!(matches!(fp.data, Some(NodeData::BuildScript(_))));
    }

    #[test]
    fn without_rerun_if_changed_any_package_file_reruns() {
        let fx = Fx::new("echo cargo::rustc-cfg=has_x\n");
        let ctx = fx.ctx();
        fx.run(&ctx, &mut Vec::new());
        assert!(fx.run(&ctx, &mut Vec::new()).1);
        std::fs::write(fx.pkg.join("other.rs"), b"new file").unwrap();
        // A new file is not in the recorded set, but an edited one is.
        std::fs::write(fx.pkg.join("input.txt"), b"v2").unwrap();
        assert!(!fx.run(&ctx, &mut Vec::new()).1);
    }

    /// A package at the project root holds Gluon's own build directory.
    /// Other nodes rewrite files there on every build, so the package
    /// walk must skip it or the script would rerun each time.
    #[test]
    fn package_walk_skips_gluons_build_dir() {
        let fx = Fx::new("echo cargo::rustc-cfg=has_x\n");
        let build = fx.pkg.join("build");
        let rlib = build.join("host/deps/libother.rlib");
        std::fs::create_dir_all(rlib.parent().unwrap()).unwrap();
        std::fs::write(&rlib, b"v1").unwrap();
        let ctx = CompileCtx::new(
            BuildLayout::new(&build, "demo"),
            Arc::clone(&fx.ctx().rustc_info),
        );
        fx.run(&ctx, &mut Vec::new());
        std::fs::write(&rlib, b"v2, rebuilt by another node").unwrap();
        assert!(
            fx.run(&ctx, &mut Vec::new()).1,
            "a file under the build dir must not rerun the script"
        );
        std::fs::write(fx.pkg.join("input.txt"), b"v2").unwrap();
        assert!(
            !fx.run(&ctx, &mut Vec::new()).1,
            "a package file still does"
        );
    }

    #[test]
    fn failing_script_names_the_run() {
        let fx = Fx::new("echo oops >&2\nexit 3\n");
        let ctx = fx.ctx();
        let run_dir = fx.tmp.path().join("build/run");
        let err = run_build_script(
            &ctx,
            RunInput {
                what: "build script of crate pkg (host)".into(),
                key: "run:k".into(),
                binary: &fx.script,
                env: BTreeMap::new(),
                manifest_dir: &fx.pkg,
                run_dir: &run_dir,
                has_links: false,
            },
            &mut Vec::new(),
        )
        .expect_err("must fail");
        let text = err.to_string();
        assert!(
            text.contains("build script of crate pkg (host)") && text.contains("oops"),
            "{text}"
        );
    }
}
