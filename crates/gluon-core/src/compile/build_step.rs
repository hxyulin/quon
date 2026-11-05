//! Build-step executor: invoke a host-bin runner with curated env
//! vars, capture its single output file, and route the result through
//! gluon's cache.
//!
//! The semantic shape is the same as a crate compile: a [`FreshnessQuery`]
//! against the fingerprint store, a slow-path spawn on a miss, and a
//! record written on success. The differences from
//! [`crate::compile::compile_and_cache`] are:
//!
//! - Output path is `<build>/build-steps/<step-name>/<output>`, derived
//!   from [`BuildLayout::build_step_dir`].
//! - There is no depfile — gluon knows the full input set up-front
//!   (runner binary + every `crate_in` artifact + every `file_in` path).
//! - The runner is a user-authored host bin, not rustc; we don't merge
//!   any rustc-specific environment.
//!
//! ### Cache key
//!
//! The command hash is computed over the deterministic environment we set on
//! the child:
//!
//!   - `GLUON_OUTPUT_PATH=<absolute output path>`
//!   - every input env entry, in `BTreeMap` order: `KEY=<resolved value>`
//!   - every literal env entry, in `BTreeMap` order: `KEY=<value>`
//!
//! plus the runner binary's path and the empty argv (no positional
//! args today). Sources tracked for content/mtime freshness:
//!
//!   - the runner binary (so a runner-source change re-runs every step
//!     that uses it)
//!   - every `crate_in` resolved path (cache invalidates when an upstream
//!     artifact changes)
//!   - every `file_in` resolved path
//!
//! Literal env values participate via the command hash only; they have no
//! disk presence, and their content is captured in the hash.

use crate::cache::{Freshness, FreshnessQuery, RecordInput, hash::hash_argv};
use crate::compile::CompileCtx;
use crate::compile::extern_map::ArtifactLookup;
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{BuildModel, BuildStepDef, BuildStepInput, Handle, ResolvedConfig};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// Reserved env-var name used to communicate the output path. Must
/// match the constant in `gluon_config::builders::build_step`.
const RESERVED_OUTPUT_ENV: &str = "GLUON_OUTPUT_PATH";

/// Run a build step. Returns the output path and a `was_cached` flag
/// matching the convention used by other scheduler executors.
///
/// Caller invariants (the DAG enforces them):
/// - The runner crate has already been compiled — its handle is in
///   `artifacts`.
/// - Every `BuildStepInput::Crate` has had its handle populated by
///   intern AND its producer crate has already been compiled.
pub fn execute_build_step(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    handle: Handle<BuildStepDef>,
    artifacts: &dyn ArtifactLookup,
    stdout_sink: &mut Vec<u8>,
    stderr_sink: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let step = model.build_steps.get(handle).ok_or_else(|| {
        Error::Compile(format!(
            "execute_build_step: handle {:?} not in model.build_steps",
            handle
        ))
    })?;

    // --- Resolve runner binary path ---
    //
    // `step.runner` is a resolved handle post-intern; if the runner has
    // not been built we surface the runner's name from the model rather
    // than carrying a parallel string on `BuildStepDef`.
    let runner_path = artifacts.get(step.runner).ok_or_else(|| {
        let runner_name = model
            .crates
            .get(step.runner)
            .map(|c| c.name.as_str())
            .unwrap_or("<unknown>");
        Error::Compile(format!(
            "build_step('{}'): runner '{}' has not been built (DAG edge missing \
             or out-of-order dispatch)",
            step.name, runner_name
        ))
    })?;

    // --- Resolve inputs to a deterministic env-var map ---
    //
    // Preserve `BTreeMap` iteration order so the cache-key hash is
    // reproducible across runs. Each input becomes one (key, value)
    // pair; the value is the canonicalised absolute path for Crate /
    // File sources, the literal string for Literal sources.
    let mut env_map: BTreeMap<String, String> = BTreeMap::new();
    for (env_key, input) in &step.inputs {
        let value = resolve_input(step, env_key, input, model, resolved, artifacts)?;
        env_map.insert(env_key.clone(), value);
    }
    // Literal env entries layered on top. They cannot reference paths,
    // and last-write-wins on key collisions (already screened by the
    // builder, so collisions here would be a model bug — but `insert`
    // is idempotent semantics-wise and matches `command_builder`).
    for (k, v) in &step.env {
        env_map.insert(k.clone(), v.clone());
    }

    // --- Compute output path ---
    let out_dir = ctx.layout.build_step_dir(&step.name);
    let output_path = out_dir.join(&step.output);

    // Inject the reserved output env var. Resolve to an absolute path
    // even though `output_path` is already layout-derived: the layout
    // may itself be relative if the caller passed a non-absolute root,
    // and we want runners to see a stable absolute string.
    let abs_output = std::path::absolute(&output_path).unwrap_or_else(|_| output_path.clone());
    env_map.insert(
        RESERVED_OUTPUT_ENV.to_string(),
        abs_output.to_string_lossy().into_owned(),
    );

    // --- Cache key + freshness check ---
    //
    // Source set: runner binary + every crate_in resolved path + every
    // file_in resolved path. We re-derive these from `env_map` so the
    // set stays in lockstep with what the runner actually sees.
    let mut sources: Vec<PathBuf> = Vec::new();
    sources.push(runner_path.to_path_buf());
    for (env_key, input) in &step.inputs {
        match input {
            BuildStepInput::Crate { .. } | BuildStepInput::File { .. } => {
                if let Some(v) = env_map.get(env_key) {
                    sources.push(PathBuf::from(v));
                }
            }
            BuildStepInput::Literal { .. } => {
                // Literals contribute via argv_hash only; no source file.
            }
        }
    }

    // Hash the spawn intent. We pass an empty argv (build steps don't
    // take positional args today) and the deterministic env map. cwd
    // is the project root — see the spawn block below.
    let env_pairs: BTreeMap<OsString, OsString> = env_map
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    let command = hash_argv(
        runner_path,
        &[] as &[OsString],
        &env_pairs,
        Some(&resolved.project_root),
    );
    let cache_key = format!("buildstep-{}", step.name);

    // --- Cache check ---
    let toolchain = ctx.toolchain();
    let outputs = [output_path.clone()];
    let query = FreshnessQuery {
        key: &cache_key,
        command,
        toolchain,
        declared: &sources,
        outputs: &outputs,
        // Runner env is fixed by `env_map`, which is in the command
        // hash, and nothing records env reads.
        command_env: &env_pairs,
    };
    match ctx.store.check(&query) {
        Freshness::Fresh(_) => return Ok((output_path, true)),
        Freshness::Stale(reason) => {
            ctx.report_stale(stderr_sink, &format!("build step {}", step.name), &reason)
        }
    }

    // --- Slow path: spawn the runner ---
    std::fs::create_dir_all(&out_dir).map_err(|e| Error::Io {
        path: out_dir.clone(),
        source: e,
    })?;

    let mut cmd = Command::new(runner_path);
    // Project root is the working directory — predictable for the
    // user's runner code regardless of where `gluon build` was invoked
    // from. Runners that need scratch space should use `tempfile` /
    // `std::env::temp_dir()`.
    cmd.current_dir(&resolved.project_root);
    // We deliberately do NOT clear the parent environment: the runner
    // may legitimately want HOME / PATH / etc. for tooling (e.g. mkfs).
    // The injected vars override any same-named parent vars.
    for (k, v) in &env_map {
        cmd.env(k, v);
    }

    let started_at = std::time::SystemTime::now();
    let output = cmd.output().map_err(|e| Error::Io {
        path: runner_path.to_path_buf(),
        source: e,
    })?;

    // Stream stdout/stderr through the per-job worker buffers. The
    // worker pool flushes them atomically so output from parallel
    // build steps doesn't interleave.
    stdout_sink.extend_from_slice(&output.stdout);
    stderr_sink.extend_from_slice(&output.stderr);

    if !output.status.success() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        let exit_desc = match output.status.code() {
            Some(c) => format!("exit code {c}"),
            None => "killed by signal".to_string(),
        };
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "build_step('{}') runner '{}' failed ({exit_desc})\n\
             stderr:\n{stderr_text}",
                step.name,
                runner_path.display()
            ))
            .with_optional_span(step.span.clone()),
        ]));
    }

    if !output_path.exists() {
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "build_step('{}') runner '{}' exited 0 but did not write the declared \
             output file at GLUON_OUTPUT_PATH={}",
                step.name,
                runner_path.display(),
                abs_output.display(),
            ))
            .with_optional_span(step.span.clone()),
        ]));
    }

    // --- Record ---
    ctx.store.record(RecordInput {
        key: &cache_key,
        command,
        toolchain,
        declared: &sources,
        discovered: &[],
        env: BTreeMap::new(),
        outputs: &outputs,
        started_at,
        stderr: &[],
        data: None,
    })?;

    Ok((output_path, false))
}

/// Resolve a single [`BuildStepInput`] to the string value that will be
/// passed to the runner via the corresponding env var.
fn resolve_input(
    step: &BuildStepDef,
    env_key: &str,
    input: &BuildStepInput,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    artifacts: &dyn ArtifactLookup,
) -> Result<String> {
    match input {
        BuildStepInput::Crate { handle } => {
            let path = artifacts.get(*handle).ok_or_else(|| {
                let crate_name = model
                    .crates
                    .get(*handle)
                    .map(|c| c.name.as_str())
                    .unwrap_or("<unknown>");
                Error::Compile(format!(
                    "build_step('{}').input('{}'): producer crate '{}' has not been \
                     built (DAG edge missing or out-of-order dispatch)",
                    step.name, env_key, crate_name
                ))
            })?;
            // Canonicalise so the runner sees a stable absolute path
            // independent of where `gluon build` was invoked from. Fall
            // back to the unresolved absolute path on canonicalise
            // failure (matches `compile::env::resolve_crate_env`).
            let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            Ok(abs.to_string_lossy().into_owned())
        }
        BuildStepInput::File { path } => {
            // file_in paths are project-root-relative.
            let joined = resolved.project_root.join(path);
            if !joined.exists() {
                return Err(Error::Compile(format!(
                    "build_step('{}').input('{}'): file_in('{}') does not exist at {} \
                     (paths are relative to the project root)",
                    step.name,
                    env_key,
                    path,
                    joined.display()
                )));
            }
            let abs = std::fs::canonicalize(&joined).unwrap_or(joined);
            Ok(abs.to_string_lossy().into_owned())
        }
        BuildStepInput::Literal { value } => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{ArtifactMap, BuildLayout, RustcInfo};
    use gluon_model::{
        BuildModel, BuildStepDef, CrateDef, CrateType, GroupDef, ProjectDef, ResolvedConfig,
        ResolvedCrateRef, ResolvedProfile, TargetDef, TargetSpec,
    };
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Build a minimal context with a layout rooted in `tmp`. The
    /// rustc_info path is bogus — these tests don't spawn rustc,
    /// they spawn a stand-in runner script the test materialises into
    /// the runner crate's expected output location.
    fn fake_ctx(tmp: &Path) -> CompileCtx {
        let layout = BuildLayout::new(tmp.join("build"), "demo");
        let rustc_info = Arc::new(RustcInfo {
            rustc_path: PathBuf::from("/usr/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src: None,
            mtime_ns: 0,
        });
        CompileCtx::new(layout, rustc_info)
    }

    fn fixture_model_and_resolved(
        runner_path_in_artifacts: PathBuf,
    ) -> (
        BuildModel,
        ResolvedConfig,
        Handle<BuildStepDef>,
        ArtifactMap,
    ) {
        let mut model = BuildModel::default();
        let (th, _) = model.targets.insert(
            "x86_64-unknown-linux-gnu".into(),
            TargetDef {
                name: "x86_64-unknown-linux-gnu".into(),
                spec: "x86_64-unknown-linux-gnu".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        let (host_g, _) = model.groups.insert(
            "host".into(),
            GroupDef {
                name: "host".into(),
                ..Default::default()
            },
        );
        let (runner_h, _) = model.crates.insert(
            "runner".into(),
            CrateDef {
                name: "runner".into(),
                path: "tools/runner".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Host,
                deps: BTreeMap::new(),
                dev_deps: BTreeMap::new(),
                features: Vec::new(),
                root: None,
                linker_script: None,
                group: host_g,
                is_project_crate: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                requires_config: Vec::new(),
                artifact_deps: Vec::new(),
                artifact_env: BTreeMap::new(),
                span: None,
            },
        );
        let (bs_h, _) = model.build_steps.insert(
            "step".into(),
            BuildStepDef {
                name: "step".into(),
                runner: runner_h,
                inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                output: "out.txt".into(),
                span: None,
            },
        );

        let mut artifacts = ArtifactMap::new();
        artifacts.insert(runner_h, runner_path_in_artifacts);

        let resolved = ResolvedConfig {
            project: ProjectDef {
                name: "demo".into(),
                version: "0.0.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: th,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                qemu_boot_mode: None,
                test_timeout: None,
            },
            options: Default::default(),
            crates: vec![ResolvedCrateRef {
                handle: runner_h,
                target: th,
                host: true,
            }],
            build_dir: PathBuf::from("/tmp/build"),
            project_root: PathBuf::from("/tmp"),
        };
        (model, resolved, bs_h, artifacts)
    }

    /// Materialise an executable shell script that writes a fixed
    /// payload to `$GLUON_OUTPUT_PATH`. Returns the absolute path.
    /// Skipped on Windows (no chmod, different shebang semantics).
    #[cfg(unix)]
    fn write_runner_script(dir: &Path, payload: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("runner.sh");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s' '{}' > \"$GLUON_OUTPUT_PATH\"\n",
                payload.replace('\'', r"'\''")
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn cold_run_spawns_runner_and_records_output() {
        let tmp = TempDir::new().unwrap();
        let runner = write_runner_script(tmp.path(), "hello");
        let ctx = fake_ctx(tmp.path());
        let (model, resolved, bs_h, artifacts) = fixture_model_and_resolved(runner.clone());

        // Update resolved.project_root so file_in / cwd point inside tmp.
        let resolved = ResolvedConfig {
            project_root: tmp.path().to_path_buf(),
            ..resolved
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (out_path, was_cached) = execute_build_step(
            &ctx,
            &model,
            &resolved,
            bs_h,
            &artifacts,
            &mut stdout,
            &mut stderr,
        )
        .expect("runner must succeed");

        assert!(!was_cached, "cold run must not be cached");
        assert!(
            out_path.exists(),
            "runner must have written GLUON_OUTPUT_PATH"
        );
        let body = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(body, "hello", "runner payload must reach output file");
    }

    #[cfg(unix)]
    #[test]
    fn warm_run_with_unchanged_inputs_is_cached() {
        let tmp = TempDir::new().unwrap();
        let runner = write_runner_script(tmp.path(), "hello");
        let ctx = fake_ctx(tmp.path());
        let (model, resolved, bs_h, artifacts) = fixture_model_and_resolved(runner.clone());
        let resolved = ResolvedConfig {
            project_root: tmp.path().to_path_buf(),
            ..resolved
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        execute_build_step(
            &ctx,
            &model,
            &resolved,
            bs_h,
            &artifacts,
            &mut stdout,
            &mut stderr,
        )
        .unwrap();

        let (_out, was_cached) = execute_build_step(
            &ctx,
            &model,
            &resolved,
            bs_h,
            &artifacts,
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert!(
            was_cached,
            "second invocation with unchanged inputs must be a cache hit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn runner_failure_surfaces_stderr_diagnostic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runner.sh");
        std::fs::write(&path, "#!/bin/sh\necho 'bad' >&2\nexit 17\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let ctx = fake_ctx(tmp.path());
        let (model, resolved, bs_h, artifacts) = fixture_model_and_resolved(path);
        let resolved = ResolvedConfig {
            project_root: tmp.path().to_path_buf(),
            ..resolved
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let err = execute_build_step(
            &ctx,
            &model,
            &resolved,
            bs_h,
            &artifacts,
            &mut stdout,
            &mut stderr,
        )
        .expect_err("non-zero exit must error");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("exit code 17") && d.message.contains("bad")),
                    "expected exit-code + stderr in diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn runner_silent_success_without_writing_output_errors() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runner.sh");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let ctx = fake_ctx(tmp.path());
        let (model, resolved, bs_h, artifacts) = fixture_model_and_resolved(path);
        let resolved = ResolvedConfig {
            project_root: tmp.path().to_path_buf(),
            ..resolved
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let err = execute_build_step(
            &ctx,
            &model,
            &resolved,
            bs_h,
            &artifacts,
            &mut stdout,
            &mut stderr,
        )
        .expect_err("missing output must error");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("did not write the declared output")),
                    "expected missing-output diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }
}
