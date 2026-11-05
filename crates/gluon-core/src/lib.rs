//! Gluon core library.
//!
//! Host-side build-system primitives consumed by the `gluon-cli` binary
//! and external embedders. The top-level entry points
//! ([`resolve_config`], [`build`], [`clean`], [`configure`],
//! [`find_project_root`]) are the stable surface that CLI and editor
//! integrations should depend on; everything inside `pub mod` blocks is
//! considered semi-internal and may be reshaped between MVP releases.

pub mod analyzer;
pub mod build_script;
pub mod cache;
pub mod compile;
pub mod config;
pub mod error;
pub mod fmt;
pub mod generated;
pub mod image;
pub mod kconfig;
pub mod manifest;
pub mod project_root;
pub mod rule;
pub mod run;
pub mod scheduler;
pub mod script_runner;
pub mod sysroot;
pub mod unit;
pub mod vendor;

pub use cache::{CacheLock, FingerprintStore, Freshness, FreshnessQuery, StaleReason};
pub use compile::{
    ArtifactMap, BuildLayout, CompileCtx, CompileUnitInput, DriverKind, Emit, RustcCommandBuilder,
    RustcInfo, compile_unit,
};
pub use error::{Diagnostic, Error, Level, Result};
pub use project_root::find_project_root;
pub use rule::builtin::ExecRule;
pub use rule::{RuleCtx, RuleFn, RuleRegistry};
pub use run::{RunOptions, run};
pub use scheduler::{
    BuildSummary, Dag, DagNode, JobDispatcher, NodeId, WorkerPool, build_dag, execute_pipeline,
};
pub use sysroot::ensure_sysroot;

// Re-export the model crate for convenience — embedders can use either
// `gluon_core::model::BuildModel` or `gluon_model::BuildModel`.
pub use gluon_model as model;

use gluon_model::{BuildModel, ConfigValue, ResolvedConfig};
use std::collections::BTreeMap;
use std::path::Path;

/// Load a [`BuildModel`] from `<project_root>/gluon.rs`.
///
/// This is the canonical entry point. CLI subcommands and embedders
/// should call this rather than `script_runner::compile_and_run` directly.
pub fn load_build_model(project_root: &Path) -> Result<BuildModel> {
    let rs_path = project_root.join("gluon.rs");
    if rs_path.is_file() {
        let layout = compile::BuildLayout::new(project_root.join("build"), "script-runner");
        let info = compile::RustcInfo::load_or_probe(&layout)?;
        return script_runner::compile_and_run(project_root, layout.root(), &info);
    }
    Err(Error::Config(format!(
        "no gluon.rs in {}",
        project_root.display()
    )))
}

/// Resolve a [`BuildModel`] into a [`ResolvedConfig`] for a specific
/// profile, optionally overriding the target and per-option config
/// values.
///
/// Thin wrapper around [`config::resolve`]. The `target` argument overrides the profile's declared
/// target when `Some`; pass `None` to use whatever target the profile
/// itself selected. `overrides` is typically loaded from a per-developer
/// `.gluon-config` file.
pub fn resolve_config(
    model: &BuildModel,
    profile: &str,
    target: Option<&str>,
    project_root: &Path,
    overrides: Option<&BTreeMap<String, ConfigValue>>,
) -> Result<ResolvedConfig> {
    config::resolve(model, profile, target, project_root, overrides)
}

/// Top-level build entry point. Builds the DAG, runs the scheduler, and
/// returns a
/// [`BuildSummary`] describing how many cacheable steps actually ran
/// rustc versus how many were short-circuited by the cache.
///
/// Uses the default set of built-in rules (`RuleRegistry::with_builtins`)
/// and the host's parallelism from `std::thread::available_parallelism`
/// (fallback 1). Callers that need to pin the worker count (CI
/// reproducibility, scheduler debugging, `-j` from the CLI) should use
/// [`build_with_workers`] instead.
pub fn build(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
) -> Result<BuildSummary> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    build_with_workers(ctx, model, resolved, workers)
}

/// Like [`build`], but pins the worker count instead of probing the host.
///
/// `workers` must be ≥ 1. The CLI's `-j/--jobs` flag is validated at
/// parse time so this assertion only fires on programmatic misuse.
///
/// Use this when:
/// - the CLI user passed `-j N` and expects exactly N workers,
/// - a CI run needs deterministic interleaving (e.g. `-j 1` for stable
///   stdout in golden tests),
/// - you're debugging a scheduler issue and want to bisect parallelism.
pub fn build_with_workers(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    workers: usize,
) -> Result<BuildSummary> {
    assert!(workers >= 1, "build_with_workers requires workers >= 1");

    // Cross-process lock around the build directory. Held for the
    // entire build so concurrent `gluon build` invocations in the same
    // project serialise instead of clobbering each other's outputs and
    // records. Bound to a name so it lives until the end of the
    // function. See `cache::lock` for the rationale.
    let _cache_guard = CacheLock::acquire(&ctx.layout)?;

    let rules = rule::RuleRegistry::with_builtins();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    if cache::remove_legacy_cache(&ctx.layout)? {
        use std::io::Write as _;
        let _ = writeln!(
            stderr,
            "note: removed the build cache of an older Gluon ({}) and its outputs; \
             this build starts clean",
            ctx.layout.legacy_cache_manifest().display()
        );
    }
    // Build the cross-target vendor context by reading per-target
    // scratch workspaces populated by `gluon vendor`. For host-only
    // models this short-circuits to an empty context with no cargo
    // invocation, so projects without cross deps pay zero overhead.
    // Models declaring cross deps without populated workspaces
    // produce a "run `gluon vendor`" diagnostic.
    let vendor_ctx = crate::vendor::cross::build_cross_vendor_context(
        model,
        &ctx.layout,
        &ctx.rustc_info,
        &resolved.project_root,
    )?;
    // Each node writes its own record when it finishes, so a failed
    // build keeps the records of every node that did finish.
    scheduler::execute_pipeline(
        ctx,
        model,
        resolved,
        &vendor_ctx,
        &rules,
        workers,
        &mut stdout,
        &mut stderr,
    )
}

/// Run the compile pipeline as a `gluon check` (metadata-only) pass.
///
/// Equivalent to [`build_with_workers`] except the caller is required
/// to construct `ctx` with a [`BuildLayout`] whose driver is
/// [`DriverKind::Check`]. The layout drives:
///
/// - the `--emit=metadata,dep-info` override on every per-crate rustc
///   invocation (suppressing codegen and link),
/// - the `tool/check/` subdirectory namespace for output, so the run
///   does not clobber a parallel `gluon build` cache.
///
/// The sysroot, generated config crate, and fingerprint records are
/// deliberately shared with `gluon build` — running check after build
/// (or vice versa) hits a hot sysroot cache and never invalidates the
/// other tool's freshness records.
pub fn check_with_workers(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    workers: usize,
) -> Result<BuildSummary> {
    debug_assert_eq!(
        ctx.driver(),
        DriverKind::Check,
        "check_with_workers requires a CompileCtx whose layout was constructed with DriverKind::Check"
    );
    build_with_workers(ctx, model, resolved, workers)
}

/// Run the compile pipeline as a `gluon clippy` (clippy-driver +
/// metadata-only) pass.
///
/// Same contract as [`check_with_workers`], except the layout's driver
/// must be [`DriverKind::Clippy`]. That selects `clippy-driver` as the
/// program path (resolved via `$CLIPPY_DRIVER` → sibling-of-rustc →
/// PATH), keeps the metadata-only emit, and routes user-crate output
/// under `tool/clippy/` so clippy artifacts never collide with build
/// or check artifacts.
pub fn clippy_with_workers(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    workers: usize,
) -> Result<BuildSummary> {
    debug_assert_eq!(
        ctx.driver(),
        DriverKind::Clippy,
        "clippy_with_workers requires a CompileCtx whose layout was constructed with DriverKind::Clippy"
    );
    build_with_workers(ctx, model, resolved, workers)
}

/// Remove the gluon build directory.
///
/// When `keep_sysroot` is `false`, the entire `layout.root()` is
/// removed.
///
/// When `true`, every entry under `layout.root()` is removed except the
/// `sysroot/` directory, the lockfile, and the sysroot records in
/// `.fingerprint/` (the node's `sysroot:<target>` record and the
/// per-crate `sysroot:<target>:<crate>` records, whose files start with
/// `sysroot_`). The next build then finds the sysroot fresh instead of
/// rebuilding it.
///
/// `NotFound` errors on the root are treated as success — "clean"
/// of an already-clean tree is a no-op.
pub fn clean(layout: &BuildLayout, keep_sysroot: bool) -> Result<()> {
    let root = layout.root();
    if !root.exists() {
        return Ok(());
    }

    // Hold the same lock that `build` takes, for the same reason: a
    // clean racing an in-flight build could `rm -rf` files the builder
    // is in the middle of writing. Acquire after the existence check so
    // we don't create a build dir on a no-op clean.
    let _cache_guard = CacheLock::acquire(layout)?;

    if !keep_sysroot {
        return cache::remove_path(root);
    }
    let fingerprints = layout.fingerprint_dir();
    for path in dir_entries(root)? {
        let name = path.file_name().unwrap_or_default();
        if name == "sysroot" || name == ".lock" {
            continue;
        }
        if path == fingerprints {
            for record in dir_entries(&fingerprints)? {
                let keep = record
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("sysroot_"));
                if !keep {
                    cache::remove_path(&record)?;
                }
            }
            continue;
        }
        cache::remove_path(&path)?;
    }
    Ok(())
}

/// Entries of `dir`, sorted.
fn dir_entries(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let io = |e| Error::Io {
        path: dir.to_path_buf(),
        source: e,
    };
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(io)? {
        out.push(entry.map_err(io)?.path());
    }
    out.sort();
    Ok(out)
}

/// Generate a `rust-project.json` file for rust-analyzer at `output`
/// (defaulting to `<project_root>/rust-project.json`) and ensure the
/// generated config crate has at least a stub `lib.rs` so rust-analyzer
/// does not see a dangling root.
///
/// The stub is only written if the file does not already exist; the
/// real generated content is written by [`build`] on the next run and
/// will overwrite the stub idempotently.
///
/// When `<project_root>/gluon.rs` exists, the emitted JSON also includes
/// a crate entry for it (pointing at the gluon-config source crate so
/// rust-analyzer resolves its types). If the workspace directory cannot
/// be resolved (e.g. `GLUON_CONFIG_WORKSPACE_DIR` is stale after an
/// install), the gluon.rs entry is silently omitted — the user gets a
/// degraded but functional rust-analyzer view for the kernel sources.
pub fn configure(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    output: Option<&Path>,
) -> Result<()> {
    // Detect whether the user has a gluon.rs and, if so, resolve the
    // gluon-config lib.rs path so rust-analyzer can index it as a dep.
    // This is a pure path resolution — no cargo build is triggered here.
    let gluon_rs_path = resolved.project_root.join("gluon.rs");
    let (opt_rs, opt_lib_rs) = if gluon_rs_path.is_file() {
        match script_runner::rlib::workspace_dir() {
            Ok(workspace) => {
                let lib_rs = workspace.join("crates/gluon-config/src/lib.rs");
                (Some(gluon_rs_path), Some(lib_rs))
            }
            Err(_) => {
                // Workspace path unavailable (e.g. post-install without a
                // source checkout). Emit JSON without the gluon.rs entry
                // rather than failing configure entirely.
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    let json = analyzer::generate_rust_project_json_with_script(
        model,
        resolved,
        &ctx.layout,
        &ctx.rustc_info,
        opt_rs.as_deref(),
        opt_lib_rs.as_deref(),
    );

    let default_path;
    let output_path = match output {
        Some(p) => p,
        None => {
            default_path = resolved.project_root.join("rust-project.json");
            &default_path
        }
    };
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let file = std::fs::File::create(output_path).map_err(|e| Error::Io {
        path: output_path.to_path_buf(),
        source: e,
    })?;
    serde_json::to_writer_pretty(file, &json).map_err(|e| Error::Io {
        path: output_path.to_path_buf(),
        source: std::io::Error::other(e),
    })?;

    // --- Materialise the config-crate stub ---
    //
    // rust-analyzer refuses to index a workspace with a dangling
    // `root_module`. The analyzer always points the generated config
    // crate at `<layout.generated_config_crate_dir()>/src/lib.rs`, so
    // we make sure that path exists with at least a comment header.
    // `gluon build` will overwrite it with the real generated source
    // on the next run, so this is idempotent.
    let stub_dir = ctx.layout.generated_config_crate_dir().join("src");
    let stub_path = stub_dir.join("lib.rs");
    if !stub_path.exists() {
        std::fs::create_dir_all(&stub_dir).map_err(|e| Error::Io {
            path: stub_dir.clone(),
            source: e,
        })?;
        let stub = "// Generated config crate stub. Populated by `gluon build`.\n\
                    #![no_std]\n\
                    #![allow(dead_code)]\n";
        std::fs::write(&stub_path, stub.as_bytes()).map_err(|e| Error::Io {
            path: stub_path.clone(),
            source: e,
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{BuildModel, PanicStrategy, ProjectDef, ResolvedProfile, TargetDef};
    use std::fs;
    use std::sync::Arc;

    fn fake_rustc_info() -> RustcInfo {
        RustcInfo {
            rustc_path: std::path::PathBuf::from("/usr/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "0.0.0".into(),
            sysroot: std::path::PathBuf::from("/fake-sysroot"),
            rust_src: None,
            mtime_ns: 0,
        }
    }

    fn make_ctx(build_root: &Path) -> CompileCtx {
        let layout = BuildLayout::new(build_root, "demo");
        fs::create_dir_all(build_root).unwrap();
        CompileCtx::new(layout, Arc::new(fake_rustc_info()))
    }

    #[test]
    fn clean_removes_entire_root_when_not_keep_sysroot() {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join("build");
        fs::create_dir_all(build.join("sysroot")).unwrap();
        fs::create_dir_all(build.join("cross")).unwrap();
        fs::write(build.join("cache-manifest.json"), b"{}").unwrap();
        let layout = BuildLayout::new(&build, "demo");

        clean(&layout, false).expect("clean ok");
        assert!(!build.exists(), "build dir must be gone");
    }

    #[test]
    fn clean_keep_sysroot_preserves_sysroot_only() {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join("build");
        fs::create_dir_all(build.join("sysroot/x86_64")).unwrap();
        fs::write(build.join("sysroot/x86_64/stamp"), b"v1").unwrap();
        fs::create_dir_all(build.join("cross/x/y")).unwrap();
        fs::create_dir_all(build.join("host/foo")).unwrap();
        let layout = BuildLayout::new(&build, "demo");

        clean(&layout, true).expect("clean ok");
        assert!(
            build.join("sysroot/x86_64/stamp").exists(),
            "sysroot must be preserved"
        );
        assert!(!build.join("cross").exists(), "cross must be gone");
        assert!(!build.join("host").exists(), "host must be gone");
    }

    #[test]
    fn clean_keep_sysroot_retains_sysroot_records() {
        use crate::cache::{FingerprintStore, RecordInput};

        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "t");
        let store = FingerprintStore::new(layout.fingerprint_dir(), Vec::new());
        let out = tmp.path().join("out");
        fs::write(&out, b"x").unwrap();
        let record = |key: &str, stderr: &[u8]| {
            store
                .record(RecordInput {
                    key,
                    command: [1; 32],
                    toolchain: [2; 32],
                    declared: &[],
                    discovered: &[],
                    env: BTreeMap::new(),
                    outputs: std::slice::from_ref(&out),
                    started_at: std::time::SystemTime::now(),
                    stderr,
                    data: None,
                })
                .unwrap()
        };
        record("sysroot:x86_64", b"");
        record("sysroot:x86_64:core", b"warning\n");
        record("image:boot", b"");
        record("crate:cross:kernel:dev:x86_64", b"warning\n");

        crate::clean(&layout, true).unwrap();

        let mut left: Vec<String> = fs::read_dir(layout.fingerprint_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), 3, "{left:?}");
        assert!(
            left.iter().all(|n| n.starts_with("sysroot_")),
            "only sysroot records may survive: {left:?}"
        );
        assert!(store.record_path("sysroot:x86_64").exists());
        assert!(store.record_path("sysroot:x86_64:core").exists());
    }

    #[test]
    fn clean_on_missing_root_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join("never-existed");
        let layout = BuildLayout::new(&build, "demo");
        clean(&layout, false).expect("clean of missing root must succeed");
        clean(&layout, true).expect("clean --keep-sysroot of missing root must succeed");
    }

    #[test]
    fn clean_on_empty_dir_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let layout = BuildLayout::new(&build, "demo");
        clean(&layout, true).expect("clean ok on empty");
        // The empty directory itself remains because we only iterate children.
        assert!(build.exists());
    }

    #[test]
    fn configure_writes_json_and_stub() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let build_root = project_root.join("build");
        let ctx = make_ctx(&build_root);

        let mut model = BuildModel::default();
        let (th, _) = model.targets.insert(
            "x86_64-unknown-none".into(),
            TargetDef {
                name: "x86_64-unknown-none".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: Some(PanicStrategy::Abort),
                span: None,
            },
        );
        let resolved = ResolvedConfig {
            project: ProjectDef {
                name: "demo".into(),
                version: "0.1.0".into(),
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
            options: BTreeMap::new(),
            crates: Vec::new(),
            build_dir: build_root.clone(),
            project_root: project_root.clone(),
        };

        configure(&ctx, &model, &resolved, None).expect("configure ok");

        // JSON exists at default path and parses.
        let rp = project_root.join("rust-project.json");
        assert!(rp.exists());
        let parsed: serde_json::Value =
            serde_json::from_reader(fs::File::open(&rp).unwrap()).expect("valid json");
        assert!(parsed["crates"].is_array());

        // Stub exists at the generated config crate location.
        let stub = ctx.layout.generated_config_crate_dir().join("src/lib.rs");
        assert!(stub.exists(), "stub lib.rs must be created");
        let contents = fs::read_to_string(&stub).unwrap();
        assert!(contents.contains("Generated config crate stub"));
        assert!(contents.contains("#![no_std]"));
    }

    #[test]
    fn configure_does_not_overwrite_existing_stub() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let build_root = project_root.join("build");
        let ctx = make_ctx(&build_root);

        // Pre-create a real generated lib.rs (simulating a prior `gluon build`).
        let stub_dir = ctx.layout.generated_config_crate_dir().join("src");
        fs::create_dir_all(&stub_dir).unwrap();
        let stub_path = stub_dir.join("lib.rs");
        fs::write(&stub_path, b"// REAL CONTENT\npub const X: u32 = 1;").unwrap();

        let mut model = BuildModel::default();
        let (th, _) = model.targets.insert(
            "x86_64-unknown-none".into(),
            TargetDef {
                name: "x86_64-unknown-none".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: Some(PanicStrategy::Abort),
                span: None,
            },
        );
        let resolved = ResolvedConfig {
            project: ProjectDef {
                name: "demo".into(),
                version: "0.1.0".into(),
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
            options: BTreeMap::new(),
            crates: Vec::new(),
            build_dir: build_root.clone(),
            project_root: project_root.clone(),
        };

        configure(&ctx, &model, &resolved, None).expect("configure ok");

        let contents = fs::read_to_string(&stub_path).unwrap();
        assert_eq!(
            contents, "// REAL CONTENT\npub const X: u32 = 1;",
            "configure must not clobber an existing config crate lib.rs"
        );
    }
}
