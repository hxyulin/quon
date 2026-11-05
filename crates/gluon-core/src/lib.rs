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
pub mod vendor;

pub use cache::{BuildRecord, Cache, CacheLock, CacheManifest, FreshnessQuery};
pub use compile::{
    ArtifactMap, BuildLayout, CompileCrateInput, CompileCtx, DriverKind, Emit, RustcCommandBuilder,
    RustcInfo, compile_crate,
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

/// Top-level build entry point. Builds the DAG, runs the scheduler,
/// persists the cache manifest on success, and returns a
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

    // Cross-process lock around the cache manifest. Held for the entire
    // build so concurrent `gluon build` invocations in the same project
    // serialise instead of clobbering each other's freshness records.
    // Released by drop after the final `cache.save()` below — the guard
    // is intentionally bound to a name so it lives until the end of the
    // function. See `cache::lock` for the rationale.
    let _cache_guard = CacheLock::acquire(&ctx.layout)?;

    let rules = rule::RuleRegistry::with_builtins();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
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
    let summary = scheduler::execute_pipeline(
        ctx,
        model,
        resolved,
        &vendor_ctx,
        &rules,
        workers,
        &mut stdout,
        &mut stderr,
    )?;
    // Persist the cache on success so the next run benefits from this build's
    // freshness records. On failure we intentionally skip the save — a partial
    // build's cache entries are written eagerly inside the per-node helpers, so
    // any nodes that succeeded are already recorded.
    ctx.cache
        .lock()
        .map_err(|_| Error::Config("cache mutex poisoned".into()))?
        .save()?;
    Ok(summary)
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
/// The sysroot, generated config crate, and cache manifest are
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
/// removed (including `cache-manifest.json`).
///
/// When `true`, every entry under `layout.root()` is removed
/// *except* the `sysroot/` subdirectory and the cache manifest. The
/// manifest is loaded, filtered to retain only entries whose key
/// starts with `"sysroot:"` (covers both rustc-side
/// `sysroot:<target>:<crate>` per-crate keys and composite-side
/// `sysroot:<target>` keys), and saved back. This preserves the
/// sysroot fast-path so the next build doesn't need to re-spawn
/// rustc just because clean wiped its cache state.
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
        match std::fs::remove_dir_all(root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io {
                path: root.to_path_buf(),
                source: e,
            }),
        }
    } else {
        // Iterate every direct child and skip the literal `sysroot`
        // entry AND the cache-manifest.json (which we filter in place
        // below to retain sysroot:* entries).
        let manifest_filename = layout
            .cache_manifest()
            .file_name()
            .map(|n| n.to_os_string());
        let entries = std::fs::read_dir(root).map_err(|e| Error::Io {
            path: root.to_path_buf(),
            source: e,
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::Io {
                path: root.to_path_buf(),
                source: e,
            })?;
            if entry.file_name() == "sysroot" {
                continue;
            }
            if manifest_filename.as_ref() == Some(&entry.file_name().to_os_string()) {
                continue;
            }
            let path = entry.path();
            let metadata = entry.metadata().map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?;
            let result = if metadata.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Io {
                        path: path.clone(),
                        source: e,
                    });
                }
            }
        }

        // Filter the cache manifest to retain only sysroot:* entries.
        // Both rustc-side `sysroot:<target>:<crate>` keys (per-crate)
        // and composite `sysroot:<target>` keys need preservation;
        // everything else points at artifacts that no longer exist.
        let manifest_path = layout.cache_manifest();
        if manifest_path.exists() {
            let mut cache = crate::cache::Cache::load(&manifest_path).0;
            cache.retain_sysroot_only();
            cache.save()?;
        }

        Ok(())
    }
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
    use crate::cache::Cache;
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
        let cache = Cache::load(layout.cache_manifest()).0;
        CompileCtx::new(layout, Arc::new(fake_rustc_info()), cache)
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
        fs::write(build.join("cache-manifest.json"), b"{}").unwrap();
        let layout = BuildLayout::new(&build, "demo");

        clean(&layout, true).expect("clean ok");
        assert!(
            build.join("sysroot/x86_64/stamp").exists(),
            "sysroot must be preserved"
        );
        assert!(!build.join("cross").exists(), "cross must be gone");
        assert!(!build.join("host").exists(), "host must be gone");
        assert!(
            build.join("cache-manifest.json").exists(),
            "cache manifest must be preserved (filtered to sysroot:* entries)"
        );
    }

    #[test]
    fn clean_keep_sysroot_retains_sysroot_cache_entries() {
        use crate::cache::Cache;
        use crate::cache::composite::CompositeRecord;

        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "t");
        std::fs::create_dir_all(layout.root()).expect("mkdir");
        let manifest = layout.cache_manifest();

        // Seed a sysroot composite entry plus a non-sysroot entry.
        let sysroot_out = layout.root().join("sysroot/x86_64/lib/libcore.rlib");
        std::fs::create_dir_all(sysroot_out.parent().unwrap()).expect("mkdir");
        std::fs::write(&sysroot_out, b"fake rlib").expect("write");

        let other_out = layout.root().join("crates/foo.rlib");
        std::fs::create_dir_all(other_out.parent().unwrap()).expect("mkdir");
        std::fs::write(&other_out, b"foo").expect("write");

        let mut cache = Cache::load(&manifest).0;
        cache
            .mark_composite_built(CompositeRecord {
                key: "sysroot:x86_64".into(),
                inputs: vec![],
                aux_hash: Some([1; 32]),
                output_path: sysroot_out.clone(),
            })
            .unwrap();
        cache
            .mark_composite_built(CompositeRecord {
                key: "image:boot".into(),
                inputs: vec![],
                aux_hash: None,
                output_path: other_out.clone(),
            })
            .unwrap();
        cache.save().unwrap();

        // Clean with keep_sysroot=true.
        crate::clean(&layout, true).unwrap();

        // Manifest should still exist; sysroot:* entries retained,
        // others removed.
        assert!(
            manifest.exists(),
            "manifest deleted under keep_sysroot=true"
        );
        let cache2 = Cache::load(&manifest).0;
        let composite_keys: Vec<_> = cache2.composite_keys().collect();
        assert!(composite_keys.iter().any(|k| **k == *"sysroot:x86_64"));
        assert!(!composite_keys.iter().any(|k| **k == *"image:boot"));
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
